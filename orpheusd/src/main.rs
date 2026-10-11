//! `orpheusd`: the Orpheus node daemon.

mod api;
mod config;
mod home;

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use orpheus_core::AudioFormat;
use orpheus_engine::{Engine, LoopbackConfig};
use orpheus_pipewire::PipeWireBackend;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::home::Home;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let home = Home::from_env()?;
    let config = Config::load(&home.config_path())?;
    let id = home.load_or_create_id()?;
    let name = config.name();
    tracing::info!(%id, %name, home = %home.root().display(), "starting");

    // M1: the engine only exists for the local loopback test route.
    let mut engine = None;
    let mut loopback = None;
    if config.loopback.enabled {
        let backend = PipeWireBackend::connect().map_err(|e| anyhow!(e))?;
        let mut e = Engine::new(Box::new(backend));
        let stats = e
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
        loopback = Some(stats);
        engine = Some(e);
    }
    let node = Arc::new(api::NodeInfo { id, name, loopback });

    let listener = tokio::net::TcpListener::bind(config.api.listen)
        .await
        .with_context(|| format!("binding API to {}", config.api.listen))?;
    tracing::info!(addr = %config.api.listen, "API listening");

    axum::serve(listener, api::router(node))
        .with_graceful_shutdown(shutdown())
        .await?;
    drop(engine);
    tracing::info!("stopped");
    Ok(())
}

/// Resolves on SIGINT or SIGTERM (systemd stop).
async fn shutdown() {
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
