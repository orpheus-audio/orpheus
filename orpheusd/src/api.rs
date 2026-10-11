//! HTTP API: `/x-orpheus/v1/…`. IS-04 and IS-05 get mounted here later.
//!
//! The handlers carry their OpenAPI description (`#[utoipa::path]`); the
//! spec is served at `/x-orpheus/v1/openapi.json` and printed by
//! `orpheusd --openapi`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use orpheus_core::rtp::Encoding;
use orpheus_core::{AudioFormat, DeviceId};
use orpheus_engine::{Engine, ReceiverStatus, SenderConfig, SenderId, SenderStatus, StatsSnapshot};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::config::RTP_PORT;

pub struct Node {
    pub id: DeviceId,
    pub name: String,
    /// `None` if the audio system was not reachable.
    pub engine: Option<Mutex<Engine>>,
    pub quantum: Option<u32>,
}

/// An error status with a plain-text message.
type ApiError = (StatusCode, String);

const BASE: &str = "/x-orpheus/v1";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Orpheus node API",
        description = "Orpheus extensions to the NMOS APIs of a node: status and streams.",
        license(name = "GPL-3.0-or-later", identifier = "GPL-3.0-or-later"),
    ),
    servers((url = "/", description = "The node serving this document")),
    tags((name = "node", description = "The node itself"), (name = "senders", description = "Streams this node sends")),
)]
struct ApiDoc;

#[derive(Serialize, ToSchema)]
struct Status {
    /// Device ID, 8 characters Crockford Base32.
    #[schema(example = "B73AQ0P0")]
    id: String,
    /// Announced name.
    name: String,
    /// orpheusd version.
    version: &'static str,
    /// Whether the node reached its audio system; without it there are no
    /// streams.
    audio: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<StatsSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<ReceiverStatus>,
    senders: Vec<SenderEntry>,
}

#[derive(Serialize, ToSchema)]
struct SenderEntry {
    #[schema(value_type = u32)]
    id: SenderId,
    #[serde(flatten)]
    status: SenderStatus,
}

#[derive(Deserialize, ToSchema)]
struct NewSender {
    /// `host`, `host:port`, `ip` or `ip:port`; the port defaults to 5004.
    #[schema(example = "192.168.1.20")]
    peer: String,
    /// Shown in the sink name, "Orpheus → name"; defaults to the peer.
    name: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct Created {
    #[schema(value_type = u32)]
    id: SenderId,
}

fn parts() -> (Router<Arc<Node>>, utoipa::openapi::OpenApi) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(status))
        .routes(routes!(add_sender))
        .routes(routes!(remove_sender))
        .split_for_parts()
}

/// The OpenAPI description of [`router`].
pub fn openapi() -> utoipa::openapi::OpenApi {
    parts().1
}

pub fn router(node: Arc<Node>) -> Router {
    let (router, api) = parts();
    let spec = api.to_pretty_json().expect("serializing OpenAPI spec");
    router
        .route(
            &format!("{BASE}/openapi.json"),
            get(move || async move { ([(header::CONTENT_TYPE, "application/json")], spec) }),
        )
        .with_state(node)
}

/// Node identity and the state of all streams.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/status",
    tag = "node",
    responses((status = OK, description = "Node status", body = Status)),
)]
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

/// Sends this node's audio to a peer: creates a sink "Orpheus → name" and
/// streams what is played into it to the peer's receiver.
#[utoipa::path(
    post,
    path = "/x-orpheus/v1/senders",
    tag = "senders",
    request_body = NewSender,
    responses(
        (status = CREATED, description = "Sender started", body = Created),
        (status = BAD_REQUEST, description = "The peer does not resolve", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
        (status = INTERNAL_SERVER_ERROR, description = "Creating the sink failed", body = String, content_type = "text/plain"),
    ),
)]
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

/// Stops a sender and removes its sink.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/senders/{id}",
    tag = "senders",
    params(("id" = u32, Path, description = "Sender ID from the status")),
    responses(
        (status = NO_CONTENT, description = "Stopped"),
        (status = NOT_FOUND, description = "No such sender", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
    ),
)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_lists_all_routes() {
        let api = openapi();
        let mut paths: Vec<&str> = api.paths.paths.keys().map(String::as_str).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "/x-orpheus/v1/senders",
                "/x-orpheus/v1/senders/{id}",
                "/x-orpheus/v1/status",
            ]
        );
    }

    #[test]
    fn every_schema_reference_resolves() {
        let json = openapi().to_json().unwrap();
        let schemas = &openapi().components.unwrap().schemas;
        let prefix = "\"#/components/schemas/";
        for part in json.split(prefix).skip(1) {
            let name = &part[..part.find('"').unwrap()];
            assert!(schemas.contains_key(name), "dangling $ref to {name}");
        }
        assert!(schemas.contains_key("Status"));
    }
}
