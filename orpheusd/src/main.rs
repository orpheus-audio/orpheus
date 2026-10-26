//! `orpheusd`: the Orpheus node daemon.

mod aes70;
mod api;
mod config;
mod connection;
mod events;
mod health;
mod home;
mod i18n;
mod mix;
mod network;
mod nmos;
mod outputs;
mod peer;
mod peers;
mod players;
mod policy;
mod settings;
mod sources;
mod wifi;
mod wires;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use axum::ServiceExt;
use axum::extract::Request;
use orpheus_clock::ptp::PtpClock;
use orpheus_clock::{Clock, SystemClock, Timescale};
use orpheus_core::AudioFormat;
use orpheus_discovery::{Announcement, Discovery};
use orpheus_engine::{Engine, LoopbackConfig};
use orpheus_pipewire::{Icons, Names, PipeWireBackend};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::signal::unix::{SignalKind, signal};
use tower::Layer;
use tower_http::cors::CorsLayer;
use tower_http::normalize_path::NormalizePathLayer;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;
use crate::home::Home;
use crate::i18n::{Text, Texts};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--openapi") {
        println!("{}", api::openapi().to_pretty_json()?);
        return Ok(());
    }
    init_logging();

    let home = Home::from_env()?;
    let config = Config::load(&home.config_path())?;
    let id = home.load_or_create_id()?;
    let name = config.name();
    tracing::info!(
        %id,
        %name,
        config = %home.config_path().display(),
        state = %home.state_dir().display(),
        cache = %home.cache_dir().display(),
        "starting"
    );

    let settings = settings::Store::load(&config, home.state_dir().join("settings.json"));
    let (clock, ptp) = start_clock(&config, settings.get());
    let mut icons = Icons::default();
    for (&kind, name) in &config.icons {
        icons.set(kind, name.as_str());
    }
    // Named once, in the language set at the start.
    let texts = i18n::texts(&settings.get().ui.language);
    let names = Names {
        meter: Box::new({
            let texts = texts.clone();
            move |output| texts.text(&Text::new("meter").arg("output", output))
        }),
        wire: Box::new({
            let texts = texts.clone();
            move |from, to| texts.text(&Text::new("wire").arg("from", from).arg("to", to))
        }),
    };
    let engine = match PipeWireBackend::connect(icons, names) {
        Ok(backend) => Some(start_engine(backend, clock, &config, &texts)?),
        Err(e) => {
            tracing::warn!(error = %e, "no audio system; running without audio");
            None
        }
    };
    let listener = listen_tcp(config.api.listen)
        .with_context(|| format!("binding API to {}", config.api.listen))?;
    let listen = listener.local_addr()?;
    tracing::info!(addr = %listen, "API listening");
    let aes70 = if config.aes70.enabled {
        let addr = SocketAddr::new(config.api.listen.ip(), config.aes70.port);
        match listen_tcp(addr) {
            Ok(l) => {
                tracing::info!(addr = %l.local_addr()?, "AES70 listening");
                Some(l)
            }
            Err(e) => {
                tracing::warn!(error = %e, %addr, "AES70 unavailable");
                None
            }
        }
    } else {
        None
    };

    let discovery = Discovery::start(&Announcement {
        id,
        name: name.clone(),
        port: listen.port(),
        version: env!("CARGO_PKG_VERSION").into(),
        aes70: match &aes70 {
            Some(l) => Some(orpheus_discovery::Aes70 {
                port: l.local_addr()?.port(),
                path: aes70::PATH.into(),
            }),
            None => None,
        },
    })
    .inspect_err(|e| tracing::warn!(error = %e, "mDNS unavailable; no discovery"))
    .ok();
    let node = Arc::new(api::Node {
        id,
        aes70: aes70.as_ref().map(|_| {
            aes70::Aes70::new(
                aes70::identity(&name, id),
                home.state_dir().join("aes70.json"),
            )
        }),
        name,
        listen,
        engine: engine.map(Mutex::new),
        ptp,
        discovery,
        nmos: Mutex::new(orpheus_nmos::Store::new()),
        connections: Mutex::new(connection::Connections::new(
            home.state_dir().join("routes.json"),
            config.routes,
        )),
        block_frames: config.audio.block_frames(AudioFormat::AES67.rate),
        receiver: config.receiver.clone(),
        settings: Mutex::new(settings),
        outputs: Mutex::new(outputs::Overrides::load(
            home.state_dir().join("outputs.json"),
        )),
        health: Mutex::default(),
        mixer: Mutex::default(),
        peers: Mutex::new(peers::Peers::load(home.state_dir().join("peers.json"))),
        sources: Mutex::new(sources::Sources::load(
            home.state_dir().join("sources.json"),
            config.sources,
        )),
        events: events::Events::default(),
        wires: Mutex::new(wires::Wires::load(home.state_dir().join("wires.json"))),
    });
    outputs::restore(&node);
    connection::restore(&node);

    let refresh = tokio::spawn(nmos::refresh_loop(node.clone()));
    let peers = tokio::spawn(connection::watch_peers(node.clone()));
    let health = tokio::spawn(health::watch(node.clone()));
    let mixer = tokio::spawn(policy::run(node.clone()));
    let events = tokio::spawn(events::watch(node.clone()));
    let streams = tokio::spawn(sources::run(node.clone()));
    let claims = tokio::spawn(connection::claim_on_play(node.clone()));
    let control = aes70.map(|l| {
        (
            tokio::spawn(aes70::listen(node.clone(), l)),
            tokio::spawn(aes70::run(node.clone())),
        )
    });
    // NMOS clients may add a trailing slash and call from browsers.
    let app =
        NormalizePathLayer::trim_trailing_slash().layer(api::router(node.clone()).layer(cors()));
    axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    refresh.abort();
    peers.abort();
    health.abort();
    events.abort();
    mixer.abort();
    streams.abort();
    claims.abort();
    if let Some((listen, run)) = control {
        listen.abort();
        run.abort();
        let _ = listen.await;
        let _ = run.await;
    }
    let _ = refresh.await;
    let _ = peers.await;
    let _ = health.await;
    let _ = mixer.await;
    let _ = streams.await;
    let _ = claims.await;
    drop(node);
    tracing::info!("stopped");
    Ok(())
}

/// Logs to stderr, `info` and above (orpheus-ptp only warnings, as it logs
/// every PTP measurement at `info`) unless `RUST_LOG` says otherwise. Under
/// systemd, where stderr goes to the journal, each line starts with its
/// syslog priority (`<4>` for warnings) instead of a timestamp, so that
/// `journalctl -p` can filter by level.
fn init_logging() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,orpheus_ptp=warn".into());
    let fmt = tracing_subscriber::fmt().with_env_filter(filter);
    if stderr_is_journal() {
        fmt.with_ansi(false).event_format(JournalFormat).init();
    } else {
        fmt.init();
    }
}

/// One line per event: `<priority>target: message field=value …`.
struct JournalFormat;

impl<S, N> FormatEvent<S, N> for JournalFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let priority = match *meta.level() {
            Level::ERROR => 3,
            Level::WARN => 4,
            Level::INFO => 6,
            _ => 7,
        };
        write!(w, "<{priority}>{}: ", meta.target())?;
        ctx.field_format().format_fields(w.by_ref(), event)?;
        writeln!(w)
    }
}

/// Whether stderr is the journal stream systemd announces in `JOURNAL_STREAM`
/// (`device:inode`). A redirected stderr keeps the variable but not the inode.
fn stderr_is_journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(meta) = std::fs::metadata("/proc/self/fd/2") else {
        return false;
    };
    stream.to_str() == Some(&format!("{}:{}", meta.dev(), meta.ino()))
}

/// Binds a TCP listener. `[::]` also accepts IPv4; where IPv6 is disabled
/// it falls back to `0.0.0.0`.
fn listen_tcp(addr: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let open = |addr: SocketAddr| -> std::io::Result<std::net::TcpListener> {
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        if addr.is_ipv6() {
            socket.set_only_v6(false)?;
        }
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;
        socket.bind(&addr.into())?;
        socket.listen(1024)?;
        Ok(socket.into())
    };
    let listener = open(addr).or_else(|e| {
        if addr.is_ipv6() && addr.ip().is_unspecified() {
            open(SocketAddr::from(([0, 0, 0, 0], addr.port())))
        } else {
            Err(e)
        }
    })?;
    tokio::net::TcpListener::from_std(listener)
}

/// The time base, normally TAI from the system clock. For testing on one
/// host, `ORPHEUS_DEBUG_CLOCK_SKEW_PPM` and `ORPHEUS_DEBUG_CLOCK_OFFSET_MS`
/// make it run fast and off, as if it were another machine.
fn timescale() -> Timescale {
    let var = |name: &str| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let skew = var("ORPHEUS_DEBUG_CLOCK_SKEW_PPM");
    let offset_ms = var("ORPHEUS_DEBUG_CLOCK_OFFSET_MS");
    let scale = Timescale::tai();
    if skew == 0.0 && offset_ms == 0.0 {
        return scale;
    }
    tracing::warn!(skew_ppm = skew, offset_ms, "debug: skewed clock");
    scale.skewed(skew, (offset_ms * 1e6) as i64)
}

/// Starts PTP if enabled, falling back to the system clock.
fn start_clock(
    config: &Config,
    settings: &settings::Settings,
) -> (Arc<dyn Clock>, Option<Arc<PtpClock>>) {
    let scale = timescale();
    if config.ptp.enabled {
        match PtpClock::start(settings::ptp_config(&settings.ptp), scale) {
            Ok(ptp) => {
                let ptp = Arc::new(ptp);
                return (ptp.clone(), Some(ptp));
            }
            Err(e) => tracing::warn!(error = %e, "PTP unavailable; using the system clock"),
        }
    }
    (Arc::new(SystemClock::with_timescale(scale)), None)
}

/// Starts the loopback if the config asks for it. The receivers and
/// senders come from [`connection::restore`].
fn start_engine(
    backend: PipeWireBackend,
    clock: Arc<dyn Clock>,
    config: &Config,
    texts: &Texts,
) -> Result<Engine> {
    let mut engine = Engine::new(Box::new(backend), clock);
    if config.loopback.enabled {
        engine
            .start_loopback(&LoopbackConfig {
                name: "orpheus.loopback".into(),
                description: texts.text(&Text::new("loopback")),
                playback_description: texts.text(
                    &Text::new("loopback-playback").arg("name", texts.text(&Text::new("loopback"))),
                ),
                format: AudioFormat::AES67,
                latency_ms: config.loopback.latency_ms,
                block_frames: config.audio.block_frames(AudioFormat::AES67.rate),
            })
            .map_err(|e| anyhow!(e))
            .context("starting loopback")?;
        tracing::info!(latency_ms = config.loopback.latency_ms, "loopback running");
    }
    Ok(engine)
}

/// Any origin may call the API, as NMOS asks, so that browser-based
/// controllers work.
fn cors() -> CorsLayer {
    use axum::http::Method;
    use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
    // Listed rather than `*`, which NMOS tools don't accept.
    CorsLayer::new()
        .allow_origin(tower_http::cors::Any)
        .allow_methods([
            Method::GET,
            Method::PUT,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
            Method::HEAD,
        ])
        .allow_headers([CONTENT_TYPE, ACCEPT, AUTHORIZATION])
        .expose_headers(tower_http::cors::Any)
}

/// Resolves on SIGINT or SIGTERM (systemd stop).
async fn shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
