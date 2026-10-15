//! HTTP API: `/x-orpheus/v1/…`, plus the NMOS APIs from [`crate::nmos`].
//!
//! The handlers carry their OpenAPI description (`#[utoipa::path]`); the
//! spec is served at `/x-orpheus/v1/openapi.json` and printed by
//! `orpheusd --openapi`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use orpheus_clock::ptp::{PtpClock, PtpStatus};
use orpheus_core::{DeviceId, SourceId};
use orpheus_discovery::Discovery;
use orpheus_engine::{Engine, ReceiverId, ReceiverStatus, SenderId, SenderStatus, StatsSnapshot};
use serde::{Deserialize, Serialize};
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::config::{Accept, Policy, RTP_PORT};
use crate::events::Topic;
use crate::policy::State as PolicyState;
use orpheus_core::OutputType;
use orpheus_nmos::sdp::Priority;

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
    /// The receivers' settings from the config file. Those that can change
    /// at runtime (`accept`, `policy`, `duck_db`, `link_offset_ms`) are read
    /// from `settings` instead.
    pub receiver: crate::config::ReceiverConfig,
    /// Settings that can change at runtime. Lock it last.
    pub settings: Mutex<crate::settings::Store>,
    /// The user's output types. Lock it last.
    pub outputs: Mutex<crate::outputs::Overrides>,
    /// Warnings about the network paths. Lock it last.
    pub health: Mutex<crate::health::Health>,
    /// What the policy does to each receiver's stream. Lock it last.
    pub mixer: Mutex<crate::policy::Mixer>,
    /// Aliases and pins of other nodes. Lock it last.
    pub peers: Mutex<crate::peers::Peers>,
    /// What the receivers remember of the sources they played. Lock it
    /// last.
    pub sources: Mutex<crate::sources::Sources>,
    /// Snapshots for the event stream.
    pub events: crate::events::Events,
}

impl Node {
    /// Where the media clock comes from, and the PTP status if PTP is on.
    pub fn clock(&self) -> (ClockSource, Option<PtpStatus>) {
        let Some(ptp) = &self.ptp else {
            return (ClockSource::System, None);
        };
        let status = ptp.status();
        let source = if status.running {
            ClockSource::Ptp
        } else if status.holdover {
            ClockSource::Holdover
        } else {
            ClockSource::System
        };
        (source, Some(status))
    }

    /// The settings in effect.
    pub fn settings(&self) -> crate::settings::Settings {
        self.settings.lock().unwrap().get().clone()
    }
}

/// An error status with a plain-text message.
type ApiError = (StatusCode, String);

const BASE: &str = "/x-orpheus/v1";

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Orpheus node API",
        description = "Orpheus extensions to the NMOS APIs of a node: status and streams.",
        license(name = "GPL-3.0-only", identifier = "GPL-3.0-only"),
    ),
    servers((url = "/", description = "The node serving this document")),
    tags(
        (name = "node", description = "The node itself"),
        (name = "settings", description = "Settings that can change while the node runs"),
        (name = "outputs", description = "The audio system's outputs and their types"),
        (name = "receivers", description = "Streams this node plays"),
        (name = "routes", description = "Where this node sends: a sink each, streamed to a peer"),
        (name = "sources", description = "What the receivers remember of the streams they played"),
        (name = "discovery", description = "Other nodes on the network"),
        (name = "events", description = "State pushed over a WebSocket"),
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
    /// What a free receiver plays.
    accept: Accept,
    /// Which streams are heard when several play at once.
    policy: Policy,
    /// How far normal streams are turned down under a high-priority one.
    duck_db: f32,
    /// The receivers: one per connection and a free one.
    receivers: Vec<ReceiverEntry>,
    /// The routes that run.
    routes: Vec<SenderEntry>,
    /// Problems with the network paths of the clock and the streams.
    warnings: Vec<crate::health::Warning>,
}

#[derive(Serialize, ToSchema)]
struct ReceiverEntry {
    #[schema(value_type = u32)]
    id: ReceiverId,
    /// NMOS ID.
    #[schema(value_type = String)]
    uuid: orpheus_nmos::Uuid,
    /// The UDP port it listens on, or will once connected.
    port: u16,
    /// The sender connected to it: the name in its SDP file, else its
    /// address.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<String>,
    /// The running stream; absent while the receiver is free (unless
    /// `accept` is `any`) or switched off.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<ReceiverStatus>,
    /// What the policy does to the stream; absent without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<PolicyState>,
    /// The connected sender's priority, from its SDP file.
    #[schema(value_type = String, example = "normal")]
    priority: Priority,
    /// The kind of output it plays on, as announced to other nodes.
    #[schema(value_type = String, example = "speaker")]
    output: OutputType,
    /// The output it plays on: the one its stream is linked to, else the
    /// default.
    #[serde(skip_serializing_if = "Option::is_none")]
    plays_on: Option<String>,
    /// The stream plays nowhere: the default output is a sink to another
    /// node, and Orpheus passes streams on to other nodes only when told.
    /// Choose an output for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    held: bool,
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
    /// The receivers whose streams play here.
    #[schema(value_type = Vec<u32>)]
    receivers: Vec<ReceiverId>,
}

#[derive(Deserialize, ToSchema)]
struct PlayOn {
    /// Machine name of the output, as `/x-orpheus/v1/outputs` lists it.
    #[schema(example = "alsa_output.pci-0000_00_1f.3.analog-stereo")]
    output: String,
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
    /// Present when PTP is on, also while it is not running.
    #[serde(skip_serializing_if = "Option::is_none")]
    ptp: Option<PtpStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// Synchronized with the other nodes over PTP.
    Ptp,
    /// PTP stopped after following a grandmaster; the clock runs on with
    /// its time and rate.
    Holdover,
    /// The system clock, kept in step by NTP at best: PTP is off or has
    /// not run yet.
    System,
}

#[derive(Serialize, ToSchema)]
struct SenderEntry {
    /// Route ID.
    #[schema(value_type = String)]
    id: SenderId,
    #[serde(flatten)]
    status: SenderStatus,
    /// What the SDP file announces to the peer's policy.
    #[schema(value_type = String, example = "normal")]
    priority: Priority,
    /// The peer's receiver connected to it; absent if it only sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<crate::connection::PeerLink>,
}

/// A route this node runs or remembers.
#[derive(Serialize, ToSchema)]
pub struct RouteEntry {
    /// Stays the same while the route is remembered; it also names the
    /// source the receivers recognize.
    #[schema(value_type = String)]
    id: SenderId,
    /// Name of the sink.
    name: String,
    #[schema(value_type = String, example = "normal")]
    priority: Priority,
    /// Where it sends, or sent to.
    #[schema(value_type = String, example = "192.168.1.20:5004")]
    peer: SocketAddr,
    /// The peer's Orpheus device ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    /// Whether it runs; otherwise it is remembered from before and comes
    /// back when connecting the same peer with the same priority.
    active: bool,
    /// When a remembered route last ran, in seconds since the Unix epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_used: Option<u64>,
}

#[derive(Deserialize, ToSchema)]
struct NewSender {
    /// The node to send to: its name or ID as discovered, its host name or
    /// address (API on port 7480), or the URL of its API. With
    /// `send_only`, a host or address with an optional port (default 5004);
    /// IPv6 with a port as `[addr]:port`.
    #[schema(example = "Kitchen")]
    peer: String,
    /// Name of the sink; defaults to the name the route had before, else
    /// the peer's name.
    name: Option<String>,
    /// Only send; don't connect a receiver on the peer. For receivers
    /// without an NMOS API, or set to accept any stream.
    #[serde(default)]
    send_only: bool,
    /// Take over the peer's receiver even if another sender is connected
    /// to it.
    #[serde(default)]
    force: bool,
    /// `high` plays over the peer's other streams and turns them down
    /// meanwhile, whatever the peer's policy; for announcements.
    #[serde(default)]
    #[schema(value_type = String, example = "normal")]
    priority: Priority,
}

#[derive(Serialize, ToSchema)]
struct Created {
    /// Route ID; the one it had if it was remembered.
    #[schema(value_type = String)]
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
        .routes(routes!(settings, change_settings))
        .routes(routes!(list_routes, add_sender))
        .routes(routes!(remove_sender))
        .routes(routes!(sender_sdp))
        .routes(routes!(nodes))
        .routes(routes!(change_node))
        .routes(routes!(outputs))
        .routes(routes!(set_output_type, clear_output_type))
        .routes(routes!(set_receiver_output, clear_receiver_output))
        .routes(routes!(list_sources))
        .routes(routes!(forget_source))
        .routes(routes!(events))
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
    Json(status_of(&node))
}

/// A topic's state for the event stream, as its GET returns it.
pub fn snapshot(node: &Node, topic: Topic) -> serde_json::Value {
    match topic {
        Topic::Status => serde_json::to_value(status_of(node)),
        Topic::Nodes => serde_json::to_value(nodes_of(node)),
        Topic::Outputs => serde_json::to_value(outputs_of(node)),
        Topic::Settings => serde_json::to_value(SettingsView::of(&node.settings.lock().unwrap())),
        Topic::Routes => serde_json::to_value(routes_of(node)),
        Topic::Sources => serde_json::to_value(sources_of(node)),
    }
    .unwrap_or_default()
}

fn status_of(node: &Node) -> Status {
    let (ports, mut links, sender_priorities) = {
        let conns = node.connections.lock().unwrap();
        let priorities = conns.receiver_priorities();
        let ports: Vec<(ReceiverId, u16, Option<String>, Priority)> = conns
            .receiver_ids()
            .into_iter()
            .map(|id| {
                let port = conns
                    .receiver_active(id)
                    .and_then(|a| a.transport_params.first()?.destination_port.value())
                    .unwrap_or_default();
                let priority = priorities.get(&id).copied().unwrap_or_default();
                (
                    id,
                    port,
                    conns.receiver_sender(id, &node.peers.lock().unwrap()),
                    priority,
                )
            })
            .collect();
        let senders: BTreeMap<SenderId, Priority> = conns
            .sender_ids()
            .into_iter()
            .map(|id| (id, conns.sender_priority(id)))
            .collect();
        (ports, conns.peer_links(), senders)
    };
    let engine = node.engine.as_ref().map(|e| e.lock().unwrap().status());
    let mut engine = engine.unwrap_or_default();
    let view = crate::outputs::View::of(node);
    let mixer = node.mixer.lock().unwrap();
    let held: Vec<ReceiverId> = {
        let sources = node.sources.lock().unwrap();
        ports
            .iter()
            .map(|p| p.0)
            .filter(|&id| sources.held(id))
            .collect()
    };
    let receivers = ports
        .into_iter()
        .map(|(id, port, sender, priority)| {
            let stream = engine
                .receivers
                .iter()
                .position(|(r, _)| *r == id)
                .map(|i| engine.receivers.remove(i).1);
            ReceiverEntry {
                id,
                uuid: crate::connection::receiver_uuid(node, id),
                port,
                sender,
                state: stream.as_ref().and(mixer.state(id)),
                stream,
                priority,
                output: view.receiver_type(node, id),
                plays_on: view
                    .receiver(id)
                    .filter(|_| !held.contains(&id))
                    .map(|o| o.description.clone()),
                held: held.contains(&id),
            }
        })
        .collect();
    drop(mixer);
    let settings = node.settings().receiver;
    Status {
        id: node.id.to_string(),
        name: node.name.clone(),
        version: env!("CARGO_PKG_VERSION"),
        audio: node.engine.is_some(),
        clock: {
            let (source, ptp) = node.clock();
            ClockInfo { source, ptp }
        },
        loopback: engine.loopback,
        accept: settings.accept,
        policy: settings.policy,
        duck_db: settings.duck_db,
        receivers,
        routes: engine
            .senders
            .into_iter()
            .map(|(id, status)| SenderEntry {
                id,
                status,
                priority: sender_priorities.get(&id).copied().unwrap_or_default(),
                receiver: links.remove(&id),
            })
            .collect(),
        warnings: node.health.lock().unwrap().warnings(),
    }
}

fn routes_of(node: &Node) -> Vec<RouteEntry> {
    let engine = node
        .engine
        .as_ref()
        .map(|e| e.lock().unwrap().status().senders)
        .unwrap_or_default();
    let conns = node.connections.lock().unwrap();
    let active = engine.into_iter().map(|(id, s)| RouteEntry {
        id,
        name: s.name,
        priority: conns.sender_priority(id),
        peer: s.peer,
        node: conns.sender_node(id),
        active: true,
        last_used: None,
    });
    let cached = conns.cached().iter().map(|c| RouteEntry {
        id: c.id,
        name: c.name.clone(),
        priority: c.priority,
        peer: c.peer,
        node: c.node.clone(),
        active: false,
        last_used: Some(c.used),
    });
    active.chain(cached).collect()
}

/// A source the receivers remember.
#[derive(Serialize, ToSchema)]
pub struct SourceEntry {
    #[schema(value_type = String, example = "K7QM3XRA")]
    id: SourceId,
    /// The name its stream had when it last played.
    name: String,
    /// What its stream is recognized by: `orpheus.source.<NMOS source ID>`
    /// from an Orpheus or NMOS sender, else `orpheus.session.<SDP origin>`,
    /// else `orpheus.from.<address>`.
    key: String,
    /// The sending Orpheus node's device ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    /// The output chosen for it, by machine name; absent for wherever
    /// streams play by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<orpheus_engine::Volume>,
    /// The receiver playing it now.
    #[serde(skip_serializing_if = "Option::is_none")]
    receiver: Option<ReceiverId>,
    /// When it last played, in seconds since the Unix epoch.
    last_used: u64,
}

fn sources_of(node: &Node) -> Vec<SourceEntry> {
    let playing: BTreeMap<String, ReceiverId> = match &node.engine {
        Some(engine) => {
            let engine = engine.lock().unwrap();
            engine
                .status()
                .receivers
                .iter()
                .filter_map(|&(id, _)| Some((engine.receiver_key(id)??.to_owned(), id)))
                .collect()
        }
        None => BTreeMap::new(),
    };
    let sources = node.sources.lock().unwrap();
    sources
        .list()
        .iter()
        .map(|s| SourceEntry {
            id: s.id,
            name: s.name.clone(),
            key: s.key.clone(),
            node: s.node.clone(),
            output: s.output.clone(),
            volume: s.volume.clone(),
            receiver: playing.get(&s.key).copied(),
            last_used: s.used,
        })
        .collect()
}

/// The sources the receivers remember, most recently played first: where
/// each plays and how loud. A stream is put on its source's output when it
/// starts; moving it or changing its volume, in a mixer or over this API,
/// is remembered for its source.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/sources",
    tag = "sources",
    responses((status = OK, description = "Sources", body = Vec<SourceEntry>)),
)]
async fn list_sources(State(node): State<Arc<Node>>) -> Json<Vec<SourceEntry>> {
    Json(sources_of(&node))
}

/// Forgets a source's output and volume. A stream playing it goes to the
/// default output at full volume and stays listed; others are dropped.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/sources/{id}",
    tag = "sources",
    params(("id" = String, Path, description = "Source ID")),
    responses(
        (status = NO_CONTENT, description = "Forgotten"),
        (status = NOT_FOUND, description = "No such source", body = String, content_type = "text/plain"),
    ),
)]
async fn forget_source(
    State(node): State<Arc<Node>>,
    Path(id): Path<SourceId>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || crate::sources::forget(&node, id))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .then_some(StatusCode::NO_CONTENT)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no source {id}")))
}

/// The routes that run, then the ones remembered, most recently used
/// first.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/routes",
    tag = "routes",
    responses((status = OK, description = "Routes", body = Vec<RouteEntry>)),
)]
async fn list_routes(State(node): State<Arc<Node>>) -> Json<Vec<RouteEntry>> {
    Json(routes_of(&node))
}

/// Sends this node's audio to a peer: creates a sink named after it and
/// streams what is played into it to a free receiver of the peer, which is
/// connected to the sender through the peer's IS-05 API. Both ends are
/// activated through IS-05, where they can then be redirected or paused.
/// The route is kept across restarts. A route to the peer with the same
/// priority remembered from before comes back with its ID and name.
#[utoipa::path(
    post,
    path = "/x-orpheus/v1/routes",
    tag = "routes",
    request_body = NewSender,
    responses(
        (status = CREATED, description = "Route started", body = Created),
        (status = BAD_REQUEST, description = "The peer does not resolve", body = String, content_type = "text/plain"),
        (status = CONFLICT, description = "The peer has no free receiver, or this node is already connected to it with the same priority", body = String, content_type = "text/plain"),
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
        let priority = req.priority;
        let id = tokio::task::spawn_blocking(move || {
            crate::connection::add_sender(&node, peer, label, priority)
        })
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
            crate::connection::connect(&node, &req.peer, req.name, req.force, req.priority)
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
    tracing::info!(id = %created.id, peer = %created.destination, name = %created.name, "sender started");
    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize, utoipa::IntoParams)]
struct RemoveQuery {
    /// Also forget the route, so that connecting the peer again starts a
    /// new one; works on remembered routes too.
    #[serde(default)]
    forget: bool,
}

/// Disconnects a route: stops its sender and removes its sink. The peer's
/// receiver it was connected to is disconnected, if it can be reached. The
/// route is remembered unless `forget` is set.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/routes/{id}",
    tag = "routes",
    params(("id" = String, Path, description = "Route ID"), RemoveQuery),
    responses(
        (status = NO_CONTENT, description = "Disconnected or forgotten"),
        (status = NOT_FOUND, description = "No such route", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
    ),
)]
async fn remove_sender(
    State(node): State<Arc<Node>>,
    Path(id): Path<SenderId>,
    Query(q): Query<RemoveQuery>,
) -> Result<StatusCode, ApiError> {
    let forget = q.forget;
    let removed =
        tokio::task::spawn_blocking(move || crate::connection::remove_sender(&node, id, forget))
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    if removed {
        tracing::info!(%id, forget, "route disconnected");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no route {id}")))
    }
}

/// The SDP file of a sender's stream, for AES67 and NMOS receivers. The
/// sender's NMOS resource links to it as `manifest_href`.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/routes/{id}/sdp",
    tag = "routes",
    params(("id" = String, Path, description = "Route ID")),
    responses(
        (status = OK, description = "The SDP file", body = String, content_type = "application/sdp"),
        (status = NOT_FOUND, description = "No such route running", body = String, content_type = "text/plain"),
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
        .ok_or((StatusCode::NOT_FOUND, format!("no route {id} running")))
}

/// The settings in effect and where each comes from.
#[derive(Serialize, ToSchema)]
struct SettingsView {
    #[serde(flatten)]
    settings: crate::settings::Settings,
    /// By section and key: `default`, `config` (config.toml) or `api`.
    #[schema(value_type = BTreeMap<String, BTreeMap<String, crate::settings::Source>>)]
    sources: BTreeMap<String, BTreeMap<String, crate::settings::Source>>,
}

impl SettingsView {
    fn of(store: &crate::settings::Store) -> Self {
        Self {
            settings: store.get().clone(),
            sources: store.sources(),
        }
    }
}

/// The settings that can change while the node runs: the clock and the
/// receivers. Each comes from the default, config.toml, or a change over
/// the API, which wins.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/settings",
    tag = "settings",
    responses((status = OK, description = "The settings in effect", body = SettingsView)),
)]
async fn settings(State(node): State<Arc<Node>>) -> Json<SettingsView> {
    Json(SettingsView::of(&node.settings.lock().unwrap()))
}

/// Changes settings, as a JSON merge patch: `{"receiver": {"policy":
/// "mix"}}` sets a value, `null` in place of a value goes back to
/// config.toml or the default. Kept across restarts, in the state
/// directory; config.toml is not touched. A change of the clock settings
/// restarts PTP without the clock jumping; a change of `link_offset_ms`
/// restarts the running streams.
#[utoipa::path(
    patch,
    path = "/x-orpheus/v1/settings",
    tag = "settings",
    request_body(content = serde_json::Value, description = "Sections of keys to change", content_type = "application/merge-patch+json"),
    responses(
        (status = OK, description = "The settings now in effect", body = SettingsView),
        (status = BAD_REQUEST, description = "An unknown or invalid setting, or PTP could not start with it; nothing changed", body = String, content_type = "text/plain"),
    ),
)]
async fn change_settings(
    State(node): State<Arc<Node>>,
    Json(patch): Json<serde_json::Value>,
) -> Result<Json<SettingsView>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let mut store = node.settings.lock().unwrap();
        let change = store
            .patch(&patch)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        let old = store.get().clone();
        let new = change.settings.clone();
        if new.ptp != old.ptp
            && let Some(ptp) = &node.ptp
        {
            ptp.reconfigure(crate::settings::ptp_config(&new.ptp))
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("PTP: {e}")))?;
            tracing::info!(settings = ?new.ptp, "PTP restarted");
        }
        store.commit(change);
        let view = SettingsView::of(&store);
        drop(store);
        let r = (&old.receiver, &new.receiver);
        if r.0.accept != r.1.accept || r.0.link_offset_ms != r.1.link_offset_ms {
            crate::connection::reapply_receivers(&node);
        }
        tracing::info!(%patch, "settings changed");
        Ok(Json(view))
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// The other nodes found over mDNS: Orpheus nodes and other NMOS nodes,
/// with this node's aliases and pins. Nodes that left stay listed as
/// offline; pinned ones also after a restart.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/nodes",
    tag = "discovery",
    responses(
        (status = OK, description = "Online nodes first, then by the name shown", body = Vec<crate::peers::Known>),
        (status = SERVICE_UNAVAILABLE, description = "mDNS is not running", body = String, content_type = "text/plain"),
    ),
)]
async fn nodes(State(node): State<Arc<Node>>) -> Result<Json<Vec<crate::peers::Known>>, ApiError> {
    nodes_of(&node).map(Json).ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "mDNS is not running".to_string(),
    ))
}

/// `None` if mDNS is not running.
fn nodes_of(node: &Node) -> Option<Vec<crate::peers::Known>> {
    let d = node.discovery.as_ref()?;
    Some(node.peers.lock().unwrap().list(d.nodes()))
}

/// A change to another node's alias or pin; fields left out stay.
#[derive(Deserialize, ToSchema)]
struct NodeChange {
    /// This node's own name for it; `null` removes the alias.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, example = "Kitchen")]
    alias: Option<Option<String>>,
    /// Keep it listed while offline, also after a restart.
    pinned: Option<bool>,
}

/// Tells a field set to `null` from one left out.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// Sets or removes another Orpheus node's alias, or pins or unpins it.
/// Kept across restarts. The alias replaces the node's announced name
/// wherever this node shows it, also in the names of sinks to it and of
/// streams from it.
#[utoipa::path(
    patch,
    path = "/x-orpheus/v1/nodes/{id}",
    tag = "discovery",
    params(("id" = String, Path, description = "Orpheus device ID of the node")),
    request_body = NodeChange,
    responses(
        (status = NO_CONTENT, description = "Changed"),
        (status = BAD_REQUEST, description = "Not a device ID, or an empty alias", body = String, content_type = "text/plain"),
        (status = NOT_FOUND, description = "Not an Orpheus node this node has seen", body = String, content_type = "text/plain"),
        (status = CONFLICT, description = "Another node has that alias", body = String, content_type = "text/plain"),
    ),
)]
async fn change_node(
    State(node): State<Arc<Node>>,
    Path(id): Path<String>,
    Json(req): Json<NodeChange>,
) -> Result<StatusCode, ApiError> {
    use crate::peers::{Change, Refused};
    let dev: DeviceId = id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, format!("{id} is not a device ID")))?;
    tokio::task::spawn_blocking(move || {
        let seen = node.discovery.as_ref().map(|d| d.nodes()).unwrap_or_default();
        let renames = req.alias.is_some();
        let change = Change {
            alias: req.alias,
            pinned: req.pinned,
        };
        node.peers
            .lock()
            .unwrap()
            .change(dev, change, &seen)
            .map_err(|e| match e {
                Refused::Unknown => (StatusCode::NOT_FOUND, format!("no Orpheus node {dev} seen")),
                Refused::Empty => (StatusCode::BAD_REQUEST, "empty alias".into()),
                Refused::Taken(other) => {
                    (StatusCode::CONFLICT, format!("node {other} already has that alias"))
                }
            })?;
        tracing::info!(id = %dev, alias = ?node.peers.lock().unwrap().alias(&dev.to_string()), "peer's alias or pin changed");
        if renames {
            crate::connection::rename_peer(&node, &dev.to_string());
        }
        Ok(StatusCode::NO_CONTENT)
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
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
    let list = tokio::task::spawn_blocking(move || outputs_of(&node))
        .await
        .unwrap_or_default();
    Json(list)
}

fn outputs_of(node: &Node) -> Vec<OutputEntry> {
    let view = crate::outputs::View::of(node);
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
                receivers: view
                    .receivers
                    .iter()
                    .filter(|(_, name)| **name == o.name)
                    .map(|(&id, _)| id)
                    .collect(),
            }
        })
        .collect()
}

#[derive(Deserialize, utoipa::IntoParams)]
struct EventsQuery {
    /// The topics to send, separated by commas; all if left out.
    #[param(example = "status,nodes")]
    topics: Option<String>,
}

/// Opens a WebSocket that pushes snapshots instead of being polled. Each
/// text message is one topic's full state, as its GET returns it:
/// `{"topic": "status", "data": {…}}`. The topics are `status`, `nodes`
/// (`null` while mDNS is not running), `outputs`, `settings` and `routes`.
/// Every topic asked for comes once on connecting, then again whenever it
/// changed: `nodes`, `settings` and `routes` within 250 ms, `status` and
/// `outputs` at most once a second. Messages from the client are ignored.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/events",
    tag = "events",
    params(EventsQuery),
    responses(
        (status = SWITCHING_PROTOCOLS, description = "WebSocket opened"),
        (status = BAD_REQUEST, description = "Unknown topic, or not a WebSocket request", body = String, content_type = "text/plain"),
    ),
)]
async fn events(
    State(node): State<Arc<Node>>,
    Query(q): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Result<axum::response::Response, ApiError> {
    let topics = match q.topics.as_deref() {
        None => Topic::ALL.to_vec(),
        Some(list) => list
            .split(',')
            .map(|t| {
                serde_json::from_value(serde_json::Value::String(t.trim().to_owned())).map_err(
                    |_| {
                        (
                            StatusCode::BAD_REQUEST,
                            format!(
                                "unknown topic {t:?}; expected status, nodes, outputs or settings"
                            ),
                        )
                    },
                )
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(ws.on_upgrade(move |socket| crate::events::serve(socket, node, topics)))
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

/// Plays a receiver's stream on an output. The audio system remembers the
/// output for the sending node, as it does when the user moves the stream,
/// and plays its streams there again later, on any receiver.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/receivers/{id}/output",
    tag = "receivers",
    params(("id" = u32, Path, description = "Receiver ID from the status")),
    request_body = PlayOn,
    responses(
        (status = NO_CONTENT, description = "Moved"),
        (status = NOT_FOUND, description = "No such output, or the receiver is not playing", body = String, content_type = "text/plain"),
        (status = CONFLICT, description = "The audio system cannot move the stream", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
    ),
)]
async fn set_receiver_output(
    State(node): State<Arc<Node>>,
    Path(id): Path<ReceiverId>,
    Json(req): Json<PlayOn>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || play_on(&node, id, Some(&req.output)))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Plays a receiver's stream on the default output again, also later for
/// the sending node.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/receivers/{id}/output",
    tag = "receivers",
    params(("id" = u32, Path, description = "Receiver ID from the status")),
    responses(
        (status = NO_CONTENT, description = "Back on the default output"),
        (status = NOT_FOUND, description = "The receiver is not playing", body = String, content_type = "text/plain"),
        (status = CONFLICT, description = "The audio system cannot move the stream", body = String, content_type = "text/plain"),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = String, content_type = "text/plain"),
    ),
)]
async fn clear_receiver_output(
    State(node): State<Arc<Node>>,
    Path(id): Path<ReceiverId>,
) -> Result<StatusCode, ApiError> {
    tokio::task::spawn_blocking(move || play_on(&node, id, None))
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

/// Moves receiver `id`'s stream to `output`, or with `None` to the
/// default output.
fn play_on(node: &Node, id: ReceiverId, output: Option<&str>) -> Result<StatusCode, ApiError> {
    let Some(engine) = &node.engine else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()));
    };
    if let Some(name) = output
        && crate::outputs::View::of(node).find(name).is_none()
    {
        return Err((StatusCode::NOT_FOUND, format!("no output {name}")));
    }
    let engine = engine.lock().unwrap();
    match engine.set_receiver_output(id, output) {
        Ok(true) => {
            if let Some(key) = engine.receiver_key(id).flatten() {
                node.sources.lock().unwrap().choose(key, output);
            }
            tracing::info!(id, output = output.unwrap_or("default"), "receiver output");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            format!("receiver {id} is not playing"),
        )),
        Err(e) => Err((StatusCode::CONFLICT, e.to_string())),
    }
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
                "/x-orpheus/v1/events",
                "/x-orpheus/v1/nodes",
                "/x-orpheus/v1/nodes/{id}",
                "/x-orpheus/v1/outputs",
                "/x-orpheus/v1/outputs/{name}",
                "/x-orpheus/v1/receivers/{id}/output",
                "/x-orpheus/v1/routes",
                "/x-orpheus/v1/routes/{id}",
                "/x-orpheus/v1/routes/{id}/sdp",
                "/x-orpheus/v1/settings",
                "/x-orpheus/v1/sources",
                "/x-orpheus/v1/sources/{id}",
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
