//! `orpheusd`: the Orpheus node daemon.

mod api;
mod config;
mod home;

use std::sync::Arc;

use anyhow::{Context, Result};
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
    let node = Arc::new(api::NodeInfo {
        id,
        name: config.name(),
    });
    tracing::info!(%id, name = %node.name, home = %home.root().display(), "starting");

    let listener = tokio::net::TcpListener::bind(config.api.listen)
        .await
        .with_context(|| format!("binding API to {}", config.api.listen))?;
    tracing::info!(addr = %config.api.listen, "API listening");

    axum::serve(listener, api::router(node))
        .with_graceful_shutdown(shutdown())
        .await?;
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
