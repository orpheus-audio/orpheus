//! HTTP API: `/x-orpheus/v1/…`, plus the NMOS APIs from [`crate::nmos`].
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
use orpheus_clock::ptp::{PtpClock, PtpStatus};
use orpheus_core::DeviceId;
use orpheus_discovery::Discovery;
use orpheus_engine::{Engine, ReceiverStatus, SenderId, SenderStatus, StatsSnapshot};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::config::{Accept, RTP_PORT};
use orpheus_core::OutputType;

pub struct Node {
    pub id: DeviceId,
    pub name: String,
    /// Where the API listens.
    pub listen: SocketAddr,
    /// `None` if the audio system was not reachable.
    pub engine: Option<Mutex<Engine>>,
    /// `None` if the node runs on the system clock.
    pub ptp: Option<Arc<PtpClock>>,
    /// `None` if mDNS could not start.
    pub discovery: Option<Discovery>,
    /// NMOS resources and their versions.
    pub nmos: Mutex<orpheus_nmos::Store>,
    /// Staged and active routing (IS-05). Lock it before the engine.
    pub connections: Mutex<crate::connection::Connections>,
    pub block_frames: Option<u32>,
    /// UDP port the receiver listens on unless a connection asks for
    /// another.
    pub receiver_port: u16,
    /// What the receiver plays while no sender is connected.
    pub accept: Accept,
    /// The user's output types. Lock it last.
    pub outputs: Mutex<crate::outputs::Overrides>,
    /// Warnings about the network paths. Lock it last.
    pub health: Mutex<crate::health::Health>,
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
    tags(
        (name = "node", description = "The node itself"),
        (name = "outputs", description = "The audio system's outputs and their types"),
        (name = "senders", description = "Streams this node sends"),
        (name = "discovery", description = "Other nodes on the network"),
    ),
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
    clock: ClockInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<StatsSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<ReceiverEntry>,
    senders: Vec<SenderEntry>,
    /// Problems with the network paths of the clock and the streams.
    warnings: Vec<crate::health::Warning>,
}

#[derive(Serialize, ToSchema)]
struct ReceiverEntry {
    #[serde(flatten)]
    status: ReceiverStatus,
    /// What it plays while no sender is connected.
    accept: Accept,
    /// The sender connected to it: the name in its SDP file, else its
    /// address.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    /// The kind of output it plays on, as announced to other nodes.
    #[schema(value_type = String, example = "speaker")]
    output: OutputType,
    /// The output it plays on: the one its stream is linked to, else the
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    plays_on: Option<String>,
}

/// An output of the audio system.
#[derive(Serialize, ToSchema)]
struct OutputEntry {
    /// Machine name, e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo`.
    name: String,
    /// Name shown to the user.
    description: String,
    /// The kind of output: `override` if set, else `detected`.
    #[serde(rename = "type")]
    #[schema(value_type = String, example = "hifi")]
    kind: OutputType,
    /// What the audio system says; for a sink to another node, what the
    /// peer's receiver announces.
    #[schema(value_type = String, example = "computer")]
    detected: OutputType,
    /// Set by the user.
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    overridden: Option<OutputType>,
    /// Streams go here unless moved.
    default: bool,
    /// The receiver plays here.
    receiver: bool,
}

#[derive(Deserialize, ToSchema)]
struct SetType {
    /// `speaker`, `headphones`, `headset`, `tv`, `hifi`, `computer`,
    /// `portable`, `phone` or `generic`.
    #[serde(rename = "type")]
    #[schema(value_type = String, example = "hifi")]
    kind: OutputType,
}

/// Where the media clock comes from.
#[derive(Serialize, ToSchema)]
struct ClockInfo {
    source: ClockSource,
    /// Present when the source is PTP.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp: Option<PtpStatus>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ClockSource {
    /// Synchronized with the other nodes over PTP.
    Ptp,
    /// The system clock, kept in step by NTP at best.
    System,
}

#[derive(Serialize, ToSchema)]
struct SenderEntry {
    #[schema(value_type = u32)]
    id: SenderId,
    #[serde(flatten)]
    status: SenderStatus,
    /// The peer's receiver connected to it; absent if it only sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<crate::connection::PeerLink>,
}

#[derive(Deserialize, ToSchema)]
struct NewSender {
    /// The node to send to: its name or ID as discovered, its host name or
    /// address (API on port 7480), or the URL of its API. With
    /// `send_only`, a host or address with an optional port (default 5004);
    /// IPv6 with a port as `[addr]:port`.
    #[schema(example = "Kitchen")]
    peer: String,
    /// Name of the sink; defaults to the peer's name.
    name: Option<String>,
    /// Only send; don't connect a receiver on the peer. For receivers
    /// without an NMOS API, or set to accept any stream.
    #[serde(default)]
    send_only: bool,
    /// Take over the peer's receiver even if another sender is connected
    /// to it.
    #[serde(default)]
    force: bool,
}

#[derive(Serialize, ToSchema)]
struct Created {
    #[schema(value_type = u32)]
    id: SenderId,
    /// Name of the sink.
    name: String,
    #[schema(value_type = String, example = "192.168.1.20:5004")]
    destination: SocketAddr,
    /// The peer's receiver now playing the sender; absent with `send_only`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    receiver: Option<orpheus_nmos::Uuid>,
}

fn parts() -> (Router<Arc<Node>>, utoipa::openapi::OpenApi) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(status))
        .routes(routes!(add_sender))
        .routes(routes!(remove_sender))
        .routes(routes!(sender_sdp))
        .routes(routes!(nodes))
        .routes(routes!(outputs))
        .routes(routes!(set_output_type, clear_output_type))
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
        .merge(crate::nmos::router())
        .merge(crate::connection::router())
        .route(
            &format!("{BASE}/openapi.json"),
            get(move || async move { ([(header::CONTENT_TYPE, "application/json")], spec) }),
        )
        // NMOS clients expect JSON errors everywhere.
        .fallback(|| async { crate::nmos::error(StatusCode::NOT_FOUND, "no such path".into()) })
        .method_not_allowed_fallback(|| async {
            crate::nmos::error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed".into())
        })
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
    let (sender, mut links) = {
        let conns = node.connections.lock().unwrap();
        (conns.receiver_sender(), conns.peer_links())
    };
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap().status());
    let engine = engine.unwrap_or_default();
    let view = crate::outputs::View::of(&node);
    let output = crate::outputs::receiver_type(&node);
    let plays_on = view
        .receiver
        .as_deref()
        .and_then(|n| view.find(n))
        .map(|o| o.description.clone());
    Json(Status {
        id: node.id.to_string(),
        name: node.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        audio: node.engine.is_some(),
        clock: match &node.ptp {
            Some(ptp) => ClockInfo {
                source: ClockSource::Ptp,
                ptp: Some(ptp.status()),
            },
            None => ClockInfo {
                source: ClockSource::System,
                ptp: None,
            },
        },
        loopback: engine.loopback,
        receiver: engine
            .receivers
            .into_iter()
            .next()
            .map(|(_, status)| ReceiverEntry {
                status,
                accept: node.accept,
                sender,
                output,
                plays_on,
            }),
        senders: engine
            .senders
            .into_iter()
            .map(|(id, status)| SenderEntry {
                id,
                status,
                receiver: links.remove(&id),
            })
            .collect(),
        warnings: node.health.lock().unwrap().warnings(),
    })
}

/// Sends this node's audio to a peer: creates a sink named after it and
/// streams what is played into it to a free receiver of the peer, which is
/// connected to the sender through the peer's IS-05 API. Both ends are
/// activated through IS-05, where they can then be redirected or paused.
/// The sender is kept across restarts.
#[utoipa::path(
    post,
    path = "/x-orpheus/v1/senders",
    tag = "senders",
    request_body = NewSender,
    responses(
        (status = CREATED, description = "Sender started", body = Created),
        (status = BAD_REQUEST, description = "The peer does not resolve", body = String, content_type = "text/plain"),
        (status = CONFLICT, description = "The peer has no free receiver", body = String, content_type = "text/plain"),
        (status = BAD_GATEWAY, description = "The peer cannot be reached or refused the connection", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
        (status = INTERNAL_SERVER_ERROR, description = "Creating the sink failed", body = String, content_type = "text/plain"),
    ),
)]
async fn add_sender(
    State(node): State<Arc<Node>>,
    Json(req): Json<NewSender>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let created = if req.send_only {
        let peer = resolve(&req.peer).await?;
        let name = req.name.unwrap_or_else(|| req.peer.clone());
        let label = name.clone();
        let id =
            tokio::task::spawn_blocking(move || crate::connection::add_sender(&node, peer, label))
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
        Created {
            id,
            name,
            destination: peer,
            receiver: None,
        }
    } else {
        let c = tokio::task::spawn_blocking(move || {
            crate::connection::connect(&node, &req.peer, req.name, req.force)
        })
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
        Created {
            id: c.id,
            name: c.name,
            destination: c.destination,
            receiver: Some(c.receiver),
        }
    };
    tracing::info!(id = created.id, peer = %created.destination, name = %created.name, "sender started");
    Ok((StatusCode::CREATED, Json(created)))
}

/// Stops a sender, removes its sink and forgets it. The peer's receiver it
/// was connected to is disconnected, if it can be reached.
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
    let removed = tokio::task::spawn_blocking(move || crate::connection::remove_sender(&node, id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    if removed {
        tracing::info!(id, "sender stopped");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no sender {id}")))
    }
}

/// The SDP file of a sender's stream, for AES67 and NMOS receivers. The
/// sender's NMOS resource links to it as `manifest_href`.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/senders/{id}/sdp",
    tag = "senders",
    params(("id" = u32, Path, description = "Sender ID from the status")),
    responses(
        (status = OK, description = "The SDP file", body = String, content_type = "application/sdp"),
        (status = NOT_FOUND, description = "No such sender", body = String, content_type = "text/plain"),
    ),
)]
async fn sender_sdp(
    State(node): State<Arc<Node>>,
    Path(id): Path<SenderId>,
) -> Result<([(header::HeaderName, &'static str); 1], String), ApiError> {
    tokio::task::spawn_blocking(move || crate::nmos::sdp(&node, id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(|sdp| ([(header::CONTENT_TYPE, "application/sdp")], sdp))
        .ok_or((StatusCode::NOT_FOUND, format!("no sender {id}")))
}

/// The other nodes found over mDNS: Orpheus nodes and other NMOS nodes.
/// Nodes that left stay listed as offline.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/nodes",
    tag = "discovery",
    responses(
        (status = OK, description = "Online nodes first, then by name", body = Vec<orpheus_discovery::Node>),
        (status = SERVICE_UNAVAILABLE, description = "mDNS is not running", body = String, content_type = "text/plain"),
    ),
)]
async fn nodes(
    State(node): State<Arc<Node>>,
) -> Result<Json<Vec<orpheus_discovery::Node>>, ApiError> {
    node.discovery.as_ref().map(|d| Json(d.nodes())).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "mDNS is not running".into(),
    ))
}

/// The audio system's outputs and their types. The receiver announces the
/// type of the output it plays on; a sink to another node shows the type
/// of that node's receiver.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/outputs",
    tag = "outputs",
    responses((status = OK, description = "The outputs, by description", body = Vec<OutputEntry>)),
)]
async fn outputs(State(node): State<Arc<Node>>) -> Json<Vec<OutputEntry>> {
    let list = tokio::task::spawn_blocking(move || {
        let view = crate::outputs::View::of(&node);
        let links = node.connections.lock().unwrap().peer_types();
        let overrides = node.outputs.lock().unwrap();
        view.outputs
            .iter()
            .map(|o| {
                let detected = crate::outputs::sink_sender(&o.name)
                    .and_then(|id| links.get(&id).copied())
                    .unwrap_or(o.kind);
                let overridden = overrides.get(&o.name);
                OutputEntry {
                    name: o.name.clone(),
                    description: o.description.clone(),
                    kind: overridden.unwrap_or(detected),
                    detected,
                    overridden,
                    default: o.default,
                    receiver: view.receiver.as_deref() == Some(o.name.as_str()),
                }
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    Json(list)
}

/// Sets the type of an output, overriding what was detected. Kept across
/// restarts.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/outputs/{name}",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    request_body = SetType,
    responses(
        (status = NO_CONTENT, description = "Set"),
        (status = NOT_FOUND, description = "No such output", body = String, content_type = "text/plain"),
    ),
)]
async fn set_output_type(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
    Json(req): Json<SetType>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || {
        if crate::outputs::View::of(&node).find(&name).is_none() {
            return Err((StatusCode::NOT_FOUND, format!("no output {name}")));
        }
        set_type(&node, &name, Some(req.kind));
        Ok(StatusCode::NO_CONTENT)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Goes back to the detected type of an output.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/outputs/{name}",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    responses(
        (status = NO_CONTENT, description = "Back to the detected type"),
        (status = NOT_FOUND, description = "The output has no type set", body = String, content_type = "text/plain"),
    ),
)]
async fn clear_output_type(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || {
        if set_type(&node, &name, None) {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err((StatusCode::NOT_FOUND, format!("{name} has no type set")))
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Overrides an output's type, or with `None` stops overriding it, and
/// passes it on to the sink if it is one to another node.
fn set_type(node: &Node, name: &str, kind: Option<OutputType>) -> bool {
    let changed = node.outputs.lock().unwrap().set(name, kind);
    if changed {
        tracing::info!(output = name, kind = ?kind, "output type");
        if let Some(id) = crate::outputs::sink_sender(name) {
            crate::connection::update_sink_type(node, id);
        }
    }
    changed
}

/// Resolves a peer: a host name or address, optionally with a port.
/// Prefers IPv4, which every receiver (also AES67 devices) listens on.
async fn resolve(peer: &str) -> Result<SocketAddr, ApiError> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, e);
    let (host, port) = split_host_port(peer).ok_or_else(|| bad(format!("bad peer {peer:?}")))?;
    let port = port.unwrap_or(RTP_PORT);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    // Also link-local addresses with an interface name (`fe80::1%eth0`).
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| bad(format!("resolving {peer}: {e}")))?
        .collect();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or(found.first())
        .copied()
        .ok_or_else(|| bad(format!("{peer} has no address")))
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`; a bare IPv6 address has no
/// port.
pub fn split_host_port(peer: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = peer.strip_prefix('[') {
        let (host, rest) = rest.split_once(']')?;
        return match rest {
            "" => Some((host, None)),
            _ => Some((host, Some(rest.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match peer.split_once(':') {
        Some((host, port)) if !port.contains(':') => Some((host, Some(port.parse().ok()?))),
        _ => Some((peer, None)),
    }
    .filter(|(host, _)| !host.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_addresses() {
        let cases = [
            ("box", Some(("box", None))),
            ("box.local:5006", Some(("box.local", Some(5006)))),
            ("192.168.1.2", Some(("192.168.1.2", None))),
            ("192.168.1.2:5006", Some(("192.168.1.2", Some(5006)))),
            ("fd00::2", Some(("fd00::2", None))),
            ("fe80::2%eth0", Some(("fe80::2%eth0", None))),
            ("[fd00::2]", Some(("fd00::2", None))),
            ("[fe80::2%eth0]:5006", Some(("fe80::2%eth0", Some(5006)))),
            ("box:http", None),
            ("[fd00::2]5006", None),
            (":5006", None),
        ];
        for (peer, want) in cases {
            assert_eq!(split_host_port(peer), want, "{peer}");
        }
    }

    #[test]
    fn spec_lists_all_routes() {
        let api = openapi();
        let mut paths: Vec<&str> = api.paths.paths.keys().map(String::as_str).collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "/x-orpheus/v1/nodes",
                "/x-orpheus/v1/outputs",
                "/x-orpheus/v1/outputs/{name}",
                "/x-orpheus/v1/senders",
                "/x-orpheus/v1/senders/{id}",
                "/x-orpheus/v1/senders/{id}/sdp",
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
