//! `orpheusd`: the Orpheus node daemon.

mod api;
mod config;
mod home;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use orpheus_clock::SystemClock;
use orpheus_core::AudioFormat;
use orpheus_core::rtp::Encoding;
use orpheus_engine::{Engine, LoopbackConfig, ReceiverConfig};
use orpheus_pipewire::PipeWireBackend;
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

    let engine = match PipeWireBackend::connect() {
        Ok(backend) => Some(start_engine(backend, &config)?),
        Err(e) => {
            tracing::warn!(error = %e, "no audio system; running without audio");
            None
        }
    };
    let node = Arc::new(api::Node {
        id,
        name,
        engine: engine.map(Mutex::new),
        quantum: config.audio.quantum,
    });

    let listener = tokio::net::TcpListener::bind(config.api.listen)
        .await
        .with_context(|| format!("binding API to {}", config.api.listen))?;
    tracing::info!(addr = %config.api.listen, "API listening");

    axum::serve(listener, api::router(node.clone()))
        .with_graceful_shutdown(shutdown())
        .await?;
    drop(node);
    tracing::info!("stopped");
    Ok(())
}

/// Starts the routes the config asks for.
fn start_engine(backend: PipeWireBackend, config: &Config) -> Result<Engine> {
    let mut engine = Engine::new(Box::new(backend), Arc::new(SystemClock));
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
        let listen = SocketAddr::from(([0, 0, 0, 0], rx.port));
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
