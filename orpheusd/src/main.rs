//! `orpheusd`: the Orpheus node daemon.

mod api;
mod config;
mod home;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use orpheus_clock::ptp::{PtpClock, PtpConfig};
use orpheus_clock::{Clock, SystemClock, Timescale};
use orpheus_core::AudioFormat;
use orpheus_core::rtp::Encoding;
use orpheus_engine::{Engine, LoopbackConfig, ReceiverConfig};
use orpheus_pipewire::PipeWireBackend;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::home::Home;

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--openapi") {
        println!("{}", api::openapi().to_pretty_json()?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let home = Home::from_env()?;
    let config = Config::load(&home.config_path())?;
    let id = home.load_or_create_id()?;
    let name = config.name();
    tracing::info!(%id, %name, home = %home.root().display(), "starting");

    let (clock, ptp) = start_clock(&config);
    let engine = match PipeWireBackend::connect() {
        Ok(backend) => Some(start_engine(backend, clock, &config)?),
        Err(e) => {
            tracing::warn!(error = %e, "no audio system; running without audio");
            None
        }
    };
    let node = Arc::new(api::Node {
        id,
        name,
        engine: engine.map(Mutex::new),
        ptp,
        quantum: config.audio.quantum,
    });

    let listener = listen_tcp(config.api.listen)
        .with_context(|| format!("binding API to {}", config.api.listen))?;
    tracing::info!(addr = %listener.local_addr()?, "API listening");

    axum::serve(listener, api::router(node.clone()))
        .with_graceful_shutdown(shutdown())
        .await?;
    drop(node);
    tracing::info!("stopped");
    Ok(())
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
fn start_clock(config: &Config) -> (Arc<dyn Clock>, Option<Arc<PtpClock>>) {
    let scale = timescale();
    let p = &config.ptp;
    if p.enabled {
        let cfg = PtpConfig {
            interface: p.interface.clone(),
            transport: p.transport,
            domain: p.domain,
            priority1: p.priority1,
            priority2: p.priority2,
            slave_only: p.slave_only,
            step_threshold_us: p.step_threshold_us,
            settle_threshold_us: p.settle_threshold_us,
            timescale: scale,
        };
        match PtpClock::start(cfg) {
            Ok(ptp) => {
                let ptp = Arc::new(ptp);
                return (ptp.clone(), Some(ptp));
            }
            Err(e) => tracing::warn!(error = %e, "PTP unavailable; using the system clock"),
        }
    }
    (Arc::new(SystemClock::with_timescale(scale)), None)
}

/// Starts the routes the config asks for.
fn start_engine(
    backend: PipeWireBackend,
    clock: Arc<dyn Clock>,
    config: &Config,
) -> Result<Engine> {
    let mut engine = Engine::new(Box::new(backend), clock);
    if config.loopback.enabled {
        engine
            .start_loopback(&LoopbackConfig {
                name: "orpheus.loopback".into(),
                description: "Orpheus loopback".into(),
                format: AudioFormat::AES67,
                latency_ms: config.loopback.latency_ms,
                quantum: config.audio.quantum,
                target: config.loopback.target.clone(),
            })
            .map_err(|e| anyhow!(e))
            .context("starting loopback")?;
        tracing::info!(latency_ms = config.loopback.latency_ms, "loopback running");
    }
    let rx = &config.receiver;
    if rx.enabled {
        let listen = SocketAddr::from(([0u16; 8], rx.port));
        engine
            .start_receiver(&ReceiverConfig {
                name: "orpheus.receive".into(),
                description: "Orpheus".into(),
                format: AudioFormat::AES67,
                encoding: Encoding::L24,
                listen,
                link_offset_ms: rx.link_offset_ms,
                quantum: config.audio.quantum,
                target: rx.target.clone(),
            })
            .map_err(|e| anyhow!(e))
            .with_context(|| format!("starting receiver on {listen}"))?;
        tracing::info!(%listen, link_offset_ms = rx.link_offset_ms, "receiver running");
    }
    Ok(engine)
}

/// Resolves on SIGINT or SIGTERM (systemd stop).
async fn shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
