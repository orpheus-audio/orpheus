//! HTTP API: `/x-orpheus/v1/…`. IS-04 and IS-05 get mounted here later.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use orpheus_core::rtp::Encoding;
use orpheus_core::{AudioFormat, DeviceId};
use orpheus_engine::{Engine, ReceiverStatus, SenderConfig, SenderId, SenderStatus, StatsSnapshot};
use serde::{Deserialize, Serialize};

use crate::config::RTP_PORT;

pub struct Node {
    pub id: DeviceId,
    pub name: String,
    /// `None` if the audio system was not reachable.
    pub engine: Option<Mutex<Engine>>,
    pub quantum: Option<u32>,
}

type ApiError = (StatusCode, String);

#[derive(Serialize)]
struct Status {
    id: String,
    name: String,
    version: &'static str,
    audio: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<StatsSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<ReceiverStatus>,
    senders: Vec<SenderEntry>,
}

#[derive(Serialize)]
struct SenderEntry {
    id: SenderId,
    #[serde(flatten)]
    status: SenderStatus,
}

#[derive(Deserialize)]
struct NewSender {
    /// `host`, `host:port`, `ip` or `ip:port`; the port defaults to 5004.
    peer: String,
    /// Shown in the sink name, "Orpheus → name"; defaults to the peer.
    name: Option<String>,
}

#[derive(Serialize)]
struct Created {
    id: SenderId,
}

pub fn router(node: Arc<Node>) -> Router {
    Router::new()
        .route("/x-orpheus/v1/status", get(status))
        .route("/x-orpheus/v1/senders", axum::routing::post(add_sender))
        .route("/x-orpheus/v1/senders/{id}", delete(remove_sender))
        .with_state(node)
}

async fn status(State(node): State<Arc<Node>>) -> Json<Status> {
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap().status());
    let engine = engine.unwrap_or_default();
    Json(Status {
        id: node.id.to_string(),
        name: node.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        audio: node.engine.is_some(),
        loopback: engine.loopback,
        receiver: engine.receiver,
        senders: engine
            .senders
            .into_iter()
            .map(|(id, status)| SenderEntry { id, status })
            .collect(),
    })
}

async fn add_sender(
    State(node): State<Arc<Node>>,
    Json(req): Json<NewSender>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let peer = resolve(&req.peer).await?;
    let label = req.name.unwrap_or_else(|| req.peer.clone());
    let id = tokio::task::spawn_blocking(move || {
        let mut engine = lock_engine(&node)?;
        let cfg = SenderConfig {
            name: format!("orpheus.send.{}", engine.next_sender_id()),
            description: format!("Orpheus → {label}"),
            format: AudioFormat::AES67,
            encoding: Encoding::L24,
            peer,
            quantum: node.quantum,
        };
        engine
            .add_sender(&cfg)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    tracing::info!(id, %peer, "sender started");
    Ok((StatusCode::CREATED, Json(Created { id })))
}

async fn remove_sender(
    State(node): State<Arc<Node>>,
    Path(id): Path<SenderId>,
) -> Result<StatusCode, ApiError> {
    let removed = tokio::task::spawn_blocking(move || {
        Ok::<_, ApiError>(lock_engine(&node)?.remove_sender(id))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    if removed {
        tracing::info!(id, "sender stopped");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no sender {id}")))
    }
}

fn lock_engine(node: &Node) -> Result<std::sync::MutexGuard<'_, Engine>, ApiError> {
    node.engine
        .as_ref()
        .map(|e| e.lock().unwrap())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()))
}

/// Resolves a peer, preferring IPv4 (the receiver listens on IPv4).
async fn resolve(peer: &str) -> Result<SocketAddr, ApiError> {
    if let Ok(addr) = peer.parse::<SocketAddr>() {
        return Ok(addr);
    }
    if let Ok(ip) = peer.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, RTP_PORT));
    }
    let found: Vec<SocketAddr> = match peer.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            tokio::net::lookup_host((host, port.parse::<u16>().unwrap())).await
        }
        _ => tokio::net::lookup_host((peer, RTP_PORT)).await,
    }
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("resolving {peer}: {e}")))?
    .collect();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or(found.first())
        .copied()
        .ok_or((StatusCode::BAD_REQUEST, format!("{peer} has no address")))
}
