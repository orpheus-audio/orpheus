//! HTTP API: `/x-orpheus/v1/…`, plus the NMOS APIs from [`crate::nmos`].
//!
//! The handlers carry their OpenAPI description (`#[utoipa::path]`); the
//! spec is served at `/x-orpheus/v1/openapi.json` and printed by
//! `orpheusd --openapi`.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use orpheus_clock::ptp::{PtpClock, PtpStatus};
use orpheus_core::{AudioFormat, DeviceId, Layout, SourceId};
use orpheus_discovery::Discovery;
use orpheus_engine::{Engine, ReceiverId, ReceiverStatus, SenderId, SenderStatus, StatsSnapshot};
use serde::de::DeserializeOwned;
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
    /// Channels of sources and inputs played on outputs. Lock it last.
    pub wires: Mutex<crate::wires::Wires>,
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

/// A failure: status and message.
pub type Fail = (StatusCode, String);

/// An error answer, in the format of the NMOS APIs, as every API of the
/// node answers errors: `{"code": 404, "error": "no route X", "debug":
/// null}`.
///
/// - 400: the request is malformed or has an invalid value;
/// - 404: the path, or what it names, doesn't exist;
/// - 409: the node's or the peer's state is in the way; it may work later,
///   or with `force`;
/// - 422: the request is valid, but what it asks for can't work: an
///   unknown peer, one whose receivers can't play the stream, settings
///   PTP can't run with;
/// - 500: the node failed;
/// - 502: the peer can't be reached, or answered with an error or nonsense;
/// - 503: a part of the node isn't running: the audio system, mDNS.
///
/// The NMOS APIs add 423 and 501 where they prescribe them.
pub struct ApiError(pub StatusCode, pub String);

impl From<Fail> for ApiError {
    fn from((code, msg): Fail) -> Self {
        Self(code, msg)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        crate::nmos::error(self.0, self.1)
    }
}

/// The body of an error answer.
#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// The HTTP status code.
    #[schema(example = 404)]
    pub code: u16,
    #[schema(example = "no route 3XJ7Q2WC")]
    pub error: String,
    /// Always `null`.
    pub debug: Option<String>,
}

/// A JSON request body; a body that is not JSON or doesn't fit is a 400.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(v)| Self(v))
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))
    }
}

/// Query parameters; ones that don't fit are a 400.
pub struct Params<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Self(v))
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))
    }
}

/// An ID in the path; one that can't be an ID names nothing, a 404.
pub struct Id<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Id<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        use axum::extract::path::ErrorKind;
        use axum::extract::rejection::PathRejection;
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(v)| Self(v))
            .map_err(|e| match e {
                PathRejection::FailedToDeserializePathParams(e) => match e.into_kind() {
                    ErrorKind::ParseErrorAtKey { value, .. }
                    | ErrorKind::ParseErrorAtIndex { value, .. }
                    | ErrorKind::ParseError { value, .. } => {
                        ApiError(StatusCode::NOT_FOUND, format!("no {value}"))
                    }
                    kind => ApiError(StatusCode::NOT_FOUND, kind.to_string()),
                },
                e => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.body_text()),
            })
    }
}

/// Runs `f` where it may block.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, Fail> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

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
        (name = "outputs", description = "The audio system's inputs and outputs, outputs made of channels of others, and their types"),
        (name = "wires", description = "Channels of sources and inputs played on chosen channels of outputs"),
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
    /// What the stream's channels carry: as set for its source, else as
    /// its SDP file says, else what the channel count suggests.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<LayoutChannel>>)]
    layout: Option<Layout>,
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
    /// default; absent while its own output is gone.
    #[serde(skip_serializing_if = "Option::is_none")]
    plays_on: Option<String>,
    /// The stream plays nowhere: the default output is a sink to another
    /// node, and Orpheus passes streams on to other nodes only when told.
    /// Choose an output for it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    held: bool,
    /// The stream plays on wires (see `/x-orpheus/v1/wires`), not on an
    /// output of its own.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    wired: bool,
    /// The stream plays nowhere until its output is back or it is played
    /// elsewhere; absent while it plays.
    #[serde(skip_serializing_if = "Option::is_none")]
    parked: Option<ParkedEntry>,
    /// The channels of the output it plays on.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<LayoutChannel>>)]
    output_channels: Option<Layout>,
    /// How the audio system mixes the stream's channels to the output's;
    /// absent where they match.
    #[serde(skip_serializing_if = "Option::is_none")]
    mix: Option<crate::mix::Mix>,
}

/// Why a stream plays nowhere.
#[derive(Serialize, ToSchema)]
struct ParkedEntry {
    why: ParkedWhy,
    /// The output that went, by machine name.
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    /// What it was called.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum ParkedWhy {
    /// Its output (or every output its wires go to) is gone for
    /// `[receiver] lost_after_ms` or more.
    Lost,
    /// The user removed its last wire.
    Unwired,
}

impl From<crate::sources::Parked> for ParkedEntry {
    fn from(p: crate::sources::Parked) -> Self {
        use crate::sources::Parked;
        match p {
            Parked::Lost {
                output,
                description,
            } => Self {
                why: ParkedWhy::Lost,
                output: Some(output),
                description: Some(description),
            },
            Parked::Unwired => Self {
                why: ParkedWhy::Unwired,
                output: None,
                description: None,
            },
        }
    }
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
    /// What its channels carry, in order.
    #[schema(value_type = Vec<LayoutChannel>)]
    channels: Layout,
    /// For an output made of channels of another: which.
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<CreatedOn>,
}

/// Where a created output plays.
#[derive(Serialize, ToSchema)]
struct CreatedOn {
    /// The output it plays on, by machine name.
    device: String,
    /// That output's channels, counted from 1.
    channels: Vec<usize>,
}

/// An input: a receiver's stream, or one of the audio system's.
#[derive(Serialize, ToSchema)]
struct InputEntry {
    /// Machine name, e.g. `orpheus.receive.0` or
    /// `alsa_input.usb-…-00.analog-stereo`.
    name: String,
    /// Name shown to the user: for a receiver, its stream's sender.
    description: String,
    /// `receiver` or `device`.
    kind: &'static str,
    /// For a receiver: its ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<u32>)]
    receiver: Option<ReceiverId>,
    /// What its channels carry, in order.
    #[schema(value_type = Vec<LayoutChannel>)]
    channels: Layout,
}

#[derive(Deserialize, ToSchema)]
struct NewOutput {
    /// Name shown to the user; the machine name is made from it,
    /// `orpheus.output.kitchen`.
    #[schema(example = "Kitchen")]
    name: String,
    /// The output to play on, by machine name.
    #[schema(example = "alsa_output.usb-Focusrite_Scarlett_4i4-00.pro-output-0")]
    device: String,
    /// Its channels to play on, counted from 1, one per channel of the new
    /// output.
    #[schema(example = json!([3, 4]))]
    channels: Vec<usize>,
    /// What the channels carry; by default stereo for two, and so on.
    #[serde(default)]
    #[schema(value_type = Option<LayoutText>)]
    layout: Option<LayoutBody>,
}

#[derive(Serialize, ToSchema)]
struct OutputCreated {
    /// Machine name of the new output.
    #[schema(example = "orpheus.output.kitchen")]
    name: String,
}

#[derive(Deserialize, ToSchema)]
struct PlayOn {
    /// Machine name of the output, as `/x-orpheus/v1/outputs` lists it.
    #[schema(example = "alsa_output.pci-0000_00_1f.3.analog-stereo")]
    output: String,
}

/// A channel of a layout.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct LayoutChannel {
    /// NMOS channel symbol: `L`, `R`, `C`, `LFE`, `Ls`, `Rs`, `Lss`,
    /// `Rss`, `Lrs`, `Rrs`, `Lc`, `Rc`, `Cs`, `HI`, `VIN`, `M1`, `M2`,
    /// `Lt`, `Rt`, `Lst`, `Rst`, `S`, `NSC000` to `NSC128`, or `U01` to
    /// `U64` for undefined.
    #[schema(example = "L")]
    symbol: String,
    /// A label to show instead of the symbol's name.
    #[schema(example = "Left")]
    label: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct SetLayout {
    #[schema(value_type = LayoutText)]
    layout: LayoutBody,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LayoutBody {
    Text(String),
    Channels(Layout),
}

/// ST 2110-30 groups or channel symbols as text, `51`, `ST,M` or
/// `L R C`, or the channels with their labels.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
#[serde(untagged)]
enum LayoutText {
    #[schema(example = "51")]
    Text(String),
    Channels(Vec<LayoutChannel>),
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
    /// For a running route to a peer's receiver: how that stands.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<crate::connection::PeerState>,
    /// What the sink's channels carry, and with them the stream's.
    #[schema(value_type = Vec<LayoutChannel>)]
    layout: Layout,
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
    /// What the sink's channels carry, as for a [source's
    /// layout](#/sources/set_source_layout); defaults to the layout the
    /// route had before, else stereo.
    #[schema(value_type = Option<LayoutText>)]
    layout: Option<LayoutBody>,
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
        .routes(routes!(set_route_layout))
        .routes(routes!(nodes))
        .routes(routes!(change_node))
        .routes(routes!(outputs, create_output))
        .routes(routes!(remove_output))
        .routes(routes!(inputs))
        .routes(routes!(list_wires, set_wire, remove_wire))
        .routes(routes!(set_output_type, clear_output_type))
        .routes(routes!(set_default_output))
        .routes(routes!(set_receiver_output, clear_receiver_output))
        .routes(routes!(list_sources))
        .routes(routes!(forget_source))
        .routes(routes!(set_source_layout, clear_source_layout))
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
        Topic::Inputs => serde_json::to_value(inputs_of(node)),
        Topic::Wires => serde_json::to_value(wires_of(node)),
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
    let (engine, mut layouts) = match &node.engine {
        Some(e) => {
            let e = e.lock().unwrap();
            let status = e.status();
            let layouts: BTreeMap<ReceiverId, Layout> = status
                .receivers
                .iter()
                .filter_map(|&(id, _)| Some((id, e.receiver_layout(id)?.clone())))
                .collect();
            (status, layouts)
        }
        None => Default::default(),
    };
    let mut engine = engine;
    let view = crate::outputs::View::of(node);
    let mixer = node.mixer.lock().unwrap();
    let lost_after = Duration::from_millis(node.settings().receiver.lost_after_ms.into());
    let (held, wired, lost, mut parked) = {
        let sources = node.sources.lock().unwrap();
        let ids = || ports.iter().map(|p| p.0);
        (
            ids().filter(|&id| sources.held(id)).collect::<Vec<_>>(),
            ids().filter(|&id| sources.on_wires(id)).collect::<Vec<_>>(),
            ids().filter(|&id| sources.lost(id)).collect::<Vec<_>>(),
            ids()
                .filter_map(|id| Some((id, sources.parked(id, lost_after)?)))
                .collect::<BTreeMap<_, _>>(),
        )
    };
    let receivers = ports
        .into_iter()
        .map(|(id, port, sender, priority)| {
            let stream = engine
                .receivers
                .iter()
                .position(|(r, _)| *r == id)
                .map(|i| engine.receivers.remove(i).1);
            let parked = parked.remove(&id);
            // Not on the default while it waits for its own.
            let on = view
                .receiver(id)
                .filter(|_| !held.contains(&id) && !wired.contains(&id) && !lost.contains(&id));
            let layout = layouts.remove(&id);
            let mix = layout
                .as_ref()
                .zip(on)
                .and_then(|(l, o)| crate::mix::placed(l, &o.channels));
            ReceiverEntry {
                id,
                uuid: crate::connection::receiver_uuid(node, id),
                port,
                sender,
                state: stream.as_ref().and(mixer.state(id)),
                layout,
                stream,
                priority,
                output: view.receiver_type(node, id),
                plays_on: on.map(|o| o.description.clone()),
                held: held.contains(&id),
                wired: wired.contains(&id),
                parked: parked.map(Into::into),
                output_channels: on.map(|o| o.channels.clone()),
                mix,
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
    let (engine, mut layouts) = match &node.engine {
        Some(e) => {
            let e = e.lock().unwrap();
            let senders = e.status().senders;
            let layouts: BTreeMap<SenderId, Layout> = senders
                .iter()
                .filter_map(|&(id, _)| Some((id, e.sender_layout(id)?.clone())))
                .collect();
            (senders, layouts)
        }
        None => Default::default(),
    };
    let stereo = || Layout::default_for(crate::nmos::FORMAT.channels.into());
    let conns = node.connections.lock().unwrap();
    let links = conns.peer_links();
    let active = engine.into_iter().map(|(id, s)| RouteEntry {
        layout: layouts.remove(&id).unwrap_or_else(stereo),
        id,
        name: s.name,
        priority: conns.sender_priority(id),
        peer: s.peer,
        node: conns.sender_node(id),
        active: true,
        state: links.get(&id).map(|l| l.state),
        last_used: None,
    });
    let cached = conns.cached().iter().map(|c| RouteEntry {
        id: c.id,
        name: c.name.clone(),
        priority: c.priority,
        peer: c.peer,
        node: c.node.clone(),
        active: false,
        state: None,
        layout: c.layout.clone().unwrap_or_else(stereo),
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
    /// What its channels carry, as set for it; absent for what its stream
    /// says.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<LayoutChannel>>)]
    layout: Option<Layout>,
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
            layout: s.layout.clone(),
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
        (status = NOT_FOUND, description = "No such source", body = ErrorBody),
    ),
)]
async fn forget_source(
    State(node): State<Arc<Node>>,
    Id(id): Id<SourceId>,
) -> Result<StatusCode, ApiError> {
    blocking(move || {
        crate::sources::forget(&node, id)
            .then_some(StatusCode::NO_CONTENT)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no source {id}")))
    })
    .await
}

/// Sets what the channels of a source carry, for senders that say
/// nothing or say it wrong: as ST 2110-30 groups or channel symbols
/// (`{"layout": "51"}`, `{"layout": "L R"}`), or as the channels with
/// labels. A stream playing it restarts with the layout. Kept across
/// restarts.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/sources/{id}/layout",
    tag = "sources",
    params(("id" = String, Path, description = "Source ID")),
    request_body = SetLayout,
    responses(
        (status = NO_CONTENT, description = "Set"),
        (status = BAD_REQUEST, description = "A malformed request or layout", body = ErrorBody),
        (status = NOT_FOUND, description = "No such source", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "Another number of channels than the streams have", body = ErrorBody),
    ),
)]
async fn set_source_layout(
    State(node): State<Arc<Node>>,
    Id(id): Id<SourceId>,
    Body(req): Body<SetLayout>,
) -> Result<StatusCode, ApiError> {
    let layout = layout_of(req.layout)?;
    blocking(move || {
        let channels = crate::connection::source_channels(&node, id);
        if let Some(channels) = channels.filter(|&c| c != layout.len()) {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "{layout} has {} channels; the source's stream has {channels}",
                    layout.len()
                ),
            ));
        }
        set_layout(&node, id, Some(layout))
    })
    .await
}

/// A layout as given, of 1 to [`AudioFormat::MAX_CHANNELS`] channels.
fn layout_of(body: LayoutBody) -> Result<Layout, ApiError> {
    let layout = match body {
        LayoutBody::Text(text) => {
            Layout::parse(&text).map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?
        }
        LayoutBody::Channels(layout) => layout,
    };
    let max = usize::from(AudioFormat::MAX_CHANNELS);
    if !(1..=max).contains(&layout.len()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!(
                "{layout} has {} channels; streams have 1 to {max}",
                layout.len()
            ),
        ));
    }
    Ok(layout)
}

/// Goes back to the layout a source's stream says.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/sources/{id}/layout",
    tag = "sources",
    params(("id" = String, Path, description = "Source ID")),
    responses(
        (status = NO_CONTENT, description = "Back to what its stream says"),
        (status = NOT_FOUND, description = "No such source", body = ErrorBody),
    ),
)]
async fn clear_source_layout(
    State(node): State<Arc<Node>>,
    Id(id): Id<SourceId>,
) -> Result<StatusCode, ApiError> {
    blocking(move || set_layout(&node, id, None)).await
}

fn set_layout(
    node: &Node,
    id: SourceId,
    layout: Option<Layout>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !node.sources.lock().unwrap().set_layout(id, layout) {
        return Err((StatusCode::NOT_FOUND, format!("no source {id}")));
    }
    // Streams of the source restart with it.
    crate::connection::reapply_receivers(node);
    Ok(StatusCode::NO_CONTENT)
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
        (status = BAD_REQUEST, description = "A malformed request or peer", body = ErrorBody),
        (status = CONFLICT, description = "The peer has no free receiver, or this node is already connected to it with the same priority", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "No such peer, or it has no receiver that can play the stream", body = ErrorBody),
        (status = BAD_GATEWAY, description = "The peer cannot be reached or refused the connection", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
        (status = INTERNAL_SERVER_ERROR, description = "Creating the sink failed", body = ErrorBody),
    ),
)]
async fn add_sender(
    State(node): State<Arc<Node>>,
    Body(req): Body<NewSender>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let layout = req.layout.map(layout_of).transpose()?;
    let created = if req.send_only {
        let peer = resolve(&req.peer).await?;
        let name = req.name.unwrap_or_else(|| req.peer.clone());
        let label = name.clone();
        let priority = req.priority;
        let id =
            blocking(move || crate::connection::add_sender(&node, peer, label, priority, layout))
                .await?;
        Created {
            id,
            name,
            destination: peer,
            receiver: None,
        }
    } else {
        let c = blocking(move || {
            crate::connection::connect(&node, &req.peer, req.name, req.force, req.priority, layout)
        })
        .await?;
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
        (status = BAD_REQUEST, description = "A malformed query", body = ErrorBody),
        (status = NOT_FOUND, description = "No such route", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn remove_sender(
    State(node): State<Arc<Node>>,
    Id(id): Id<SenderId>,
    Params(q): Params<RemoveQuery>,
) -> Result<StatusCode, ApiError> {
    let forget = q.forget;
    let removed = blocking(move || crate::connection::remove_sender(&node, id, forget)).await?;
    if removed {
        tracing::info!(%id, forget, "route disconnected");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(StatusCode::NOT_FOUND, format!("no route {id}")))
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
        (status = NOT_FOUND, description = "No such route running", body = ErrorBody),
    ),
)]
async fn sender_sdp(
    State(node): State<Arc<Node>>,
    Id(id): Id<SenderId>,
) -> Result<([(header::HeaderName, &'static str); 1], String), ApiError> {
    blocking(move || {
        crate::nmos::sdp(&node, id)
            .map(|sdp| ([(header::CONTENT_TYPE, "application/sdp")], sdp))
            .ok_or((StatusCode::NOT_FOUND, format!("no route {id} running")))
    })
    .await
}

/// Sets what the channels of a route's sink carry, and with them how many
/// channels its stream has: as ST 2110-30 groups or channel symbols
/// (`{"layout": "51"}`), or as the channels with labels. A running route
/// starts its sink again, and the peer's receiver restarts with the new
/// stream; a remembered route keeps it for when it runs again. Streams of
/// up to 8 channels go in 1 ms packets, more in 125 µs packets.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/routes/{id}/layout",
    tag = "routes",
    params(("id" = String, Path, description = "Route ID")),
    request_body = SetLayout,
    responses(
        (status = NO_CONTENT, description = "Set"),
        (status = BAD_REQUEST, description = "A malformed request or layout", body = ErrorBody),
        (status = NOT_FOUND, description = "No such route", body = ErrorBody),
        (status = BAD_GATEWAY, description = "The peer cannot be reached or refused the stream; the route keeps its layout", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn set_route_layout(
    State(node): State<Arc<Node>>,
    Id(id): Id<SenderId>,
    Body(req): Body<SetLayout>,
) -> Result<StatusCode, ApiError> {
    let layout = layout_of(req.layout)?;
    blocking(move || {
        crate::connection::set_layout(&node, id, layout)?
            .then_some(StatusCode::NO_CONTENT)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no route {id}")))
    })
    .await
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

/// The settings that can change while the node runs: the clock, the
/// receivers and the senders. Each comes from the default, config.toml, or a change over
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
        (status = BAD_REQUEST, description = "An unknown or invalid setting; nothing changed", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "PTP could not start with the new settings; nothing changed", body = ErrorBody),
    ),
)]
async fn change_settings(
    State(node): State<Arc<Node>>,
    Body(patch): Body<serde_json::Value>,
) -> Result<Json<SettingsView>, ApiError> {
    blocking(move || {
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
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, format!("PTP: {e}")))?;
            tracing::info!(settings = ?new.ptp, "PTP restarted");
        }
        store.commit(change);
        let view = SettingsView::of(&store);
        drop(store);
        let r = (&old.receiver, &new.receiver);
        if r.0.accept != r.1.accept || r.0.link_offset_ms != r.1.link_offset_ms {
            crate::connection::reapply_receivers(&node);
        }
        if old.sender.fade_in_ms != new.sender.fade_in_ms
            && let Some(engine) = &node.engine
        {
            engine
                .lock()
                .unwrap()
                .set_sender_fade_in(new.sender.fade_in_ms);
        }
        tracing::info!(%patch, "settings changed");
        Ok(Json(view))
    })
    .await
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
        (status = SERVICE_UNAVAILABLE, description = "mDNS is not running", body = ErrorBody),
    ),
)]
async fn nodes(State(node): State<Arc<Node>>) -> Result<Json<Vec<crate::peers::Known>>, ApiError> {
    nodes_of(&node).map(Json).ok_or(ApiError(
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
        (status = BAD_REQUEST, description = "A malformed request, or an empty alias", body = ErrorBody),
        (status = NOT_FOUND, description = "Not an Orpheus node this node has seen", body = ErrorBody),
        (status = CONFLICT, description = "Another node has that alias", body = ErrorBody),
    ),
)]
async fn change_node(
    State(node): State<Arc<Node>>,
    Id(dev): Id<DeviceId>,
    Body(req): Body<NodeChange>,
) -> Result<StatusCode, ApiError> {
    use crate::peers::{Change, Refused};
    blocking(move || {
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
            let created = overrides.find_created(&o.name).map(|c| CreatedOn {
                device: c.device.clone(),
                channels: c.channels.clone(),
            });
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
                channels: o.channels.clone(),
                created,
            }
        })
        .collect()
}

/// The inputs: the receivers' streams, then the audio system's inputs,
/// such as microphones.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/inputs",
    tag = "outputs",
    responses((status = OK, description = "The inputs", body = Vec<InputEntry>)),
)]
async fn inputs(State(node): State<Arc<Node>>) -> Json<Vec<InputEntry>> {
    let list = tokio::task::spawn_blocking(move || inputs_of(&node))
        .await
        .unwrap_or_default();
    Json(list)
}

fn inputs_of(node: &Node) -> Vec<InputEntry> {
    let Some(engine) = &node.engine else {
        return Vec::new();
    };
    let names: BTreeMap<ReceiverId, String> = {
        let conns = node.connections.lock().unwrap();
        let peers = node.peers.lock().unwrap();
        conns
            .receiver_ids()
            .into_iter()
            .filter_map(|id| Some((id, conns.receiver_sender(id, &peers)?)))
            .collect()
    };
    let engine = engine.lock().unwrap();
    let receivers = engine.status().receivers.into_iter().filter_map(|(id, _)| {
        Some(InputEntry {
            name: crate::connection::receiver_stream(id),
            description: names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("Receiver {id}")),
            kind: "receiver",
            receiver: Some(id),
            channels: engine.receiver_layout(id)?.clone(),
        })
    });
    let devices = engine.inputs().into_iter().map(|i| InputEntry {
        name: i.name,
        description: i.description,
        kind: "device",
        receiver: None,
        channels: i.channels,
    });
    receivers.chain(devices).collect()
}

/// Creates an output that plays on chosen channels of another, such as
/// channels 3 and 4 of an audio interface; applications and receivers
/// play into it like into any output. Kept across restarts; while the
/// other output is missing it plays nowhere.
#[utoipa::path(
    post,
    path = "/x-orpheus/v1/outputs",
    tag = "outputs",
    request_body = NewOutput,
    responses(
        (status = CREATED, description = "Created", body = OutputCreated),
        (status = BAD_REQUEST, description = "A malformed request, name or layout", body = ErrorBody),
        (status = CONFLICT, description = "An output of that name exists", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "No such output to play on, or not those channels", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
        (status = INTERNAL_SERVER_ERROR, description = "The audio system refused", body = ErrorBody),
    ),
)]
async fn create_output(
    State(node): State<Arc<Node>>,
    Body(req): Body<NewOutput>,
) -> Result<(StatusCode, Json<OutputCreated>), ApiError> {
    let bad = |e: String| ApiError(StatusCode::BAD_REQUEST, e);
    let description = req.name.trim().to_owned();
    let name = crate::outputs::created_name(&description)
        .ok_or_else(|| bad(format!("{:?} is no name for an output", req.name)))?;
    if req.channels.is_empty() {
        return Err(bad("no channels to play on".into()));
    }
    let layout = match req.layout {
        Some(LayoutBody::Text(text)) => Layout::parse(&text).map_err(|e| bad(e.to_string()))?,
        Some(LayoutBody::Channels(layout)) => layout,
        None => Layout::default_for(req.channels.len()),
    };
    if layout.len() != req.channels.len() {
        return Err(bad(format!(
            "{layout} has {} channels, not {}",
            layout.len(),
            req.channels.len()
        )));
    }
    blocking(move || {
        let view = crate::outputs::View::of(&node);
        if view.find(&name).is_some() {
            return Err((StatusCode::CONFLICT, format!("there is an output {name}")));
        }
        let device = view
            .find(&req.device)
            .ok_or_else(|| (StatusCode::UNPROCESSABLE_ENTITY, format!("no output {}", req.device)))?;
        let count = device.channels.len();
        if let Some(&c) = req.channels.iter().find(|&&c| c == 0 || c > count) {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{} has channels 1 to {count}, not {c}", device.description),
            ));
        }
        let created = crate::outputs::Created {
            name: name.clone(),
            description,
            device: req.device,
            channels: req.channels,
            layout,
        };
        let Some(engine) = &node.engine else {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()));
        };
        // Kept first, so that two requests can't both create it.
        if !node.outputs.lock().unwrap().add_created(created.clone()) {
            return Err((StatusCode::CONFLICT, format!("there is an output {name}")));
        }
        let added = engine.lock().unwrap().add_output(&created.spec());
        if let Err(e) = added {
            node.outputs.lock().unwrap().remove_created(&name);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
        tracing::info!(output = %name, device = %created.device, channels = ?created.channels, "output created");
        Ok((StatusCode::CREATED, Json(OutputCreated { name })))
    })
    .await
}

/// Removes an output made of channels of another.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/outputs/{name}",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "No such created output", body = ErrorBody),
    ),
)]
async fn remove_output(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    blocking(move || {
        let removed = node.outputs.lock().unwrap().remove_created(&name);
        if removed.is_none() {
            return Err((StatusCode::NOT_FOUND, format!("no created output {name}")));
        }
        if let Some(engine) = &node.engine {
            engine.lock().unwrap().remove_output(&name);
        }
        tracing::info!(output = %name, "output removed");
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// A wire: channels of a source or an input played on an output.
#[derive(Serialize, ToSchema)]
struct WireEntry {
    /// The source it comes from, by ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    source: Option<SourceId>,
    /// The input it comes from, by machine name.
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<String>,
    /// The source's or input's name.
    from_name: String,
    /// The output, by machine name.
    output: String,
    /// The output's name; its machine name while it is missing.
    output_name: String,
    /// The channel pairs as set; absent for all channels, where they
    /// belong.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<WirePair>>)]
    channels: Option<Vec<crate::wires::Pair>>,
    /// The channels it links now; empty while the source plays nowhere
    /// or an end is missing.
    links: Vec<WireLink>,
    /// How the links mix the channels; absent where each plays on its
    /// own.
    #[serde(skip_serializing_if = "Option::is_none")]
    mix: Option<crate::mix::Mix>,
    /// The input channels a wire of all channels leaves out, such as the
    /// LFE onto stereo, counted from 1.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dropped: Vec<usize>,
    /// The channels of the source or input, while known.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<LayoutChannel>>)]
    from_channels: Option<Layout>,
    /// The channels of the output, while it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<LayoutChannel>>)]
    output_channels: Option<Layout>,
}

/// A channel of the source or input linked to a channel of the output.
#[derive(Serialize, ToSchema)]
struct WireLink {
    /// The input channel, counted from 1.
    from: usize,
    /// The output channel, counted from 1.
    to: usize,
    /// Whether the audio system has made the link.
    linked: bool,
    /// It plays the channel on another one as part of a mix, such as the
    /// centre on the left.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    folded: bool,
}

/// An output channel and the input channel played on it.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct WirePair {
    to: WireChannel,
    from: WireChannel,
}

/// A channel: its number, counted from 1, or its symbol or label.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
#[serde(untagged)]
enum WireChannel {
    #[schema(example = 1)]
    Number(usize),
    #[schema(example = "L")]
    Name(String),
}

/// A wire to set: from a source or an input, to an output.
#[derive(Deserialize, ToSchema)]
#[allow(dead_code)]
struct NewWire {
    /// The source, by ID; or give `input`.
    source: Option<String>,
    /// The input, by machine name; or give `source`.
    input: Option<String>,
    /// The output, by machine name.
    output: String,
    /// The channel pairs; leave it out for all channels, where they
    /// belong.
    channels: Option<Vec<WirePair>>,
}

/// Names a wire by its ends.
#[derive(Deserialize, utoipa::IntoParams)]
struct WireQuery {
    /// The source it comes from, by ID; or give `input`.
    #[param(value_type = Option<String>)]
    source: Option<SourceId>,
    /// The input it comes from, by machine name.
    input: Option<String>,
    /// The output, by machine name.
    output: String,
}

/// The wires, with what each links now.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/wires",
    tag = "wires",
    responses((status = OK, description = "The wires", body = Vec<WireEntry>)),
)]
async fn list_wires(State(node): State<Arc<Node>>) -> Json<Vec<WireEntry>> {
    let list = tokio::task::spawn_blocking(move || wires_of(&node))
        .await
        .unwrap_or_default();
    Json(list)
}

fn wires_of(node: &Node) -> Vec<WireEntry> {
    let wires = node.wires.lock().unwrap().list().to_vec();
    wire_entries(node, &wires)
}

fn wire_entries(node: &Node, wires: &[crate::wires::Wire]) -> Vec<WireEntry> {
    use crate::wires::Origin;
    let (resolved, made, inputs, outputs) = match &node.engine {
        Some(engine) => {
            let engine = engine.lock().unwrap();
            (
                crate::wires::resolve(node, &engine, wires),
                engine.links(),
                engine.inputs(),
                engine.outputs(),
            )
        }
        None => Default::default(),
    };
    let names: BTreeMap<SourceId, String> = node
        .sources
        .lock()
        .unwrap()
        .list()
        .iter()
        .map(|s| (s.id, s.name.clone()))
        .collect();
    wires
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let r = resolved.get(i);
            let wired = r.and_then(|r| {
                let (from, to) = (r.from_layout.as_ref()?, r.to_layout.as_ref()?);
                Some(crate::mix::wired(&r.pairs, from, to, w.channels.is_none()))
            });
            let links = r
                .map(|r| {
                    r.links(w)
                        .enumerate()
                        .map(|(i, l)| WireLink {
                            from: l.from_channel + 1,
                            to: l.to_channel + 1,
                            linked: made.contains(&l),
                            folded: wired.as_ref().is_some_and(|m| m.folded[i]),
                        })
                        .collect()
                })
                .unwrap_or_default();
            let (source, input, from_name) = match &w.from {
                Origin::Source(id) => (
                    Some(*id),
                    None,
                    names.get(id).cloned().unwrap_or_else(|| id.to_string()),
                ),
                Origin::Input(name) => (
                    None,
                    Some(name.clone()),
                    inputs
                        .iter()
                        .find(|i| i.name == *name)
                        .map_or_else(|| name.clone(), |i| i.description.clone()),
                ),
            };
            WireEntry {
                source,
                input,
                from_name,
                output: w.output.clone(),
                output_name: outputs
                    .iter()
                    .find(|o| o.name == w.output)
                    .map_or_else(|| w.output.clone(), |o| o.description.clone()),
                channels: w.channels.clone(),
                links,
                mix: wired.as_ref().and_then(|m| m.mix),
                dropped: wired
                    .map_or_else(Vec::new, |m| m.dropped.into_iter().map(|c| c + 1).collect()),
                from_channels: r.and_then(|r| r.from_layout.clone()),
                output_channels: r.and_then(|r| r.to_layout.clone()),
            }
        })
        .collect()
}

/// Plays channels of a source or an input on an output, replacing the
/// wire between the two. Each pair names an output channel and the input
/// channel played on it, by number (from 1), symbol or label; without
/// pairs all channels go where they belong. A source with wires plays
/// only on them. What several wires bring into one output channel is
/// summed. Kept across restarts.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/wires",
    tag = "wires",
    request_body = NewWire,
    responses(
        (status = OK, description = "Set", body = WireEntry),
        (status = BAD_REQUEST, description = "A malformed request, or no channels", body = ErrorBody),
        (status = UNPROCESSABLE_ENTITY, description = "No such source, input, output or channel", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn set_wire(
    State(node): State<Arc<Node>>,
    Body(wire): Body<crate::wires::Wire>,
) -> Result<Json<WireEntry>, ApiError> {
    use crate::wires::{Channel, Origin};
    let bad = |e: String| ApiError(StatusCode::BAD_REQUEST, e);
    if let Some(pairs) = &wire.channels {
        if pairs.is_empty() {
            return Err(bad("no channels; leave `channels` out for all".into()));
        }
        let empty = |c: &Channel| match c {
            Channel::Number(n) => *n == 0,
            Channel::Name(name) => name.trim().is_empty(),
        };
        if pairs.iter().any(|p| empty(&p.to) || empty(&p.from)) {
            return Err(bad("channels are counted from 1, or named".into()));
        }
    }
    blocking(move || {
        let unknown = |e: String| (StatusCode::UNPROCESSABLE_ENTITY, e);
        let Some(engine) = &node.engine else {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()));
        };
        let (inputs, outputs) = {
            let engine = engine.lock().unwrap();
            (engine.inputs(), engine.outputs())
        };
        let output = outputs
            .iter()
            .find(|o| o.name == wire.output)
            .ok_or_else(|| unknown(format!("no output {}", wire.output)))?;
        // The source's layout as far as known without its stream.
        let (from_name, from_layout) = match &wire.from {
            Origin::Source(id) => {
                let sources = node.sources.lock().unwrap();
                let s = sources
                    .list()
                    .iter()
                    .find(|s| s.id == *id)
                    .ok_or_else(|| unknown(format!("no source {id}")))?;
                (s.name.clone(), s.layout.clone())
            }
            Origin::Input(name) => {
                let i = inputs
                    .iter()
                    .find(|i| i.name == *name)
                    .ok_or_else(|| unknown(format!("no input {name}")))?;
                (i.description.clone(), Some(i.channels.clone()))
            }
        };
        // Else as its stream says, if it plays.
        let from_layout = from_layout.or_else(|| {
            let engine = engine.lock().unwrap();
            crate::wires::resolve(&node, &engine, std::slice::from_ref(&wire))
                .pop()
                .and_then(|r| r.from_layout)
        });
        for p in wire.channels.iter().flatten() {
            if p.to.index(&output.channels).is_none() {
                return Err(unknown(format!(
                    "{} has no channel {} (it has {})",
                    output.description, p.to, output.channels
                )));
            }
            if let Some(layout) = &from_layout
                && p.from.index(layout).is_none()
            {
                return Err(unknown(format!(
                    "{from_name} has no channel {} (it has {layout})",
                    p.from
                )));
            }
        }
        node.wires.lock().unwrap().set(wire.clone());
        if let Origin::Source(id) = wire.from {
            node.sources.lock().unwrap().park(id, false);
        }
        tracing::info!(from = %from_name, output = %wire.output, "wired");
        crate::sources::follow(&node);
        crate::wires::follow(&node);
        Ok(Json(wire_entries(&node, &[wire]).remove(0)))
    })
    .await
}

/// Removes the wire between a source or an input and an output. A source
/// left without wires is parked: it plays nowhere until played on an output
/// or wired again; with `[receiver] when_lost = "default"` it plays on its
/// output again.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/wires",
    tag = "wires",
    params(WireQuery),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = BAD_REQUEST, description = "Neither or both of source and input", body = ErrorBody),
        (status = NOT_FOUND, description = "No such wire", body = ErrorBody),
    ),
)]
async fn remove_wire(
    State(node): State<Arc<Node>>,
    Query(q): Query<WireQuery>,
) -> Result<StatusCode, ApiError> {
    use crate::wires::Origin;
    let from = match (q.source, q.input) {
        (Some(id), None) => Origin::Source(id),
        (None, Some(name)) => Origin::Input(name),
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "give either source or input".into(),
            ));
        }
    };
    blocking(move || {
        let left = {
            let mut wires = node.wires.lock().unwrap();
            if !wires.remove(&from, &q.output) {
                return Err((StatusCode::NOT_FOUND, "no such wire".into()));
            }
            wires.outputs()
        };
        tracing::info!(from = ?from, output = %q.output, "unwired");
        // Its last wire: stopped on purpose, not moved to its output.
        if let Origin::Source(id) = from
            && !left.contains_key(&id)
            && node.settings().receiver.when_lost != crate::config::WhenLost::Default
            && node.sources.lock().unwrap().park(id, true)
        {
            tracing::info!(%id, "source parked");
        }
        crate::sources::follow(&node);
        crate::wires::follow(&node);
        Ok(StatusCode::NO_CONTENT)
    })
    .await
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
/// (`null` while mDNS is not running), `outputs`, `settings`, `routes` and
/// `sources`. Every topic asked for comes once on connecting, then again
/// whenever it changed: `nodes`, `settings` and `routes` within 250 ms,
/// `status`, `outputs` and `sources` at most once a second. Messages from
/// the client are ignored.
#[utoipa::path(
    get,
    path = "/x-orpheus/v1/events",
    tag = "events",
    params(EventsQuery),
    responses(
        (status = SWITCHING_PROTOCOLS, description = "WebSocket opened"),
        (status = BAD_REQUEST, description = "Unknown topic, or not a WebSocket request", body = ErrorBody),
    ),
)]
async fn events(
    State(node): State<Arc<Node>>,
    Params(q): Params<EventsQuery>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Result<axum::response::Response, ApiError> {
    let unknown = |t: &str| {
        let known: Vec<String> = Topic::ALL
            .iter()
            .filter_map(|t| serde_json::to_value(t).ok()?.as_str().map(str::to_owned))
            .collect();
        ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown topic {t:?}; expected {}", known.join(", ")),
        )
    };
    let topics = match q.topics.as_deref() {
        None => Topic::ALL.to_vec(),
        Some(list) => list
            .split(',')
            .map(|t| {
                serde_json::from_value(serde_json::Value::String(t.trim().to_owned()))
                    .map_err(|_| unknown(t))
            })
            .collect::<Result<_, _>>()?,
    };
    let ws = ws.map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.body_text()))?;
    Ok(ws.on_upgrade(move |socket| crate::events::serve(socket, node, topics)))
}

/// Sets the type of an output, overriding what was detected. Kept across
/// restarts.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/outputs/{name}/type",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    request_body = SetType,
    responses(
        (status = NO_CONTENT, description = "Set"),
        (status = BAD_REQUEST, description = "A malformed request", body = ErrorBody),
        (status = NOT_FOUND, description = "No such output", body = ErrorBody),
    ),
)]
async fn set_output_type(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
    Body(req): Body<SetType>,
) -> Result<StatusCode, ApiError> {
    blocking(move || {
        if crate::outputs::View::of(&node).find(&name).is_none() {
            return Err((StatusCode::NOT_FOUND, format!("no output {name}")));
        }
        set_type(&node, &name, Some(req.kind));
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

/// Goes back to the detected type of an output.
#[utoipa::path(
    delete,
    path = "/x-orpheus/v1/outputs/{name}/type",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    responses(
        (status = NO_CONTENT, description = "Back to the detected type"),
        (status = NOT_FOUND, description = "The output has no type set", body = ErrorBody),
    ),
)]
async fn clear_output_type(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    blocking(move || {
        if set_type(&node, &name, None) {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err((StatusCode::NOT_FOUND, format!("{name} has no type set")))
        }
    })
    .await
}

/// Makes an output the default one, where applications play and
/// receivers' streams go unless moved, as the desktop's sound settings
/// do. A sink to another node is fine: what plays here then goes there.
/// The audio system keeps it across restarts.
#[utoipa::path(
    put,
    path = "/x-orpheus/v1/outputs/{name}/default",
    tag = "outputs",
    params(("name" = String, Path, description = "Machine name of the output")),
    responses(
        (status = NO_CONTENT, description = "Now the default"),
        (status = NOT_FOUND, description = "No such output", body = ErrorBody),
        (status = INTERNAL_SERVER_ERROR, description = "The audio system did not change the default", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn set_default_output(
    State(node): State<Arc<Node>>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    blocking(move || {
        let Some(engine) = &node.engine else {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()));
        };
        if crate::outputs::View::of(&node).find(&name).is_none() {
            return Err((StatusCode::NOT_FOUND, format!("no output {name}")));
        }
        engine
            .lock()
            .unwrap()
            .set_default_output(&name)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        tracing::info!(output = %name, "default output");
        Ok(StatusCode::NO_CONTENT)
    })
    .await
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
        (status = BAD_REQUEST, description = "A malformed request", body = ErrorBody),
        (status = NOT_FOUND, description = "No such receiver or output", body = ErrorBody),
        (status = CONFLICT, description = "The receiver is not playing", body = ErrorBody),
        (status = INTERNAL_SERVER_ERROR, description = "The audio system did not move the stream", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn set_receiver_output(
    State(node): State<Arc<Node>>,
    Id(id): Id<ReceiverId>,
    Body(req): Body<PlayOn>,
) -> Result<StatusCode, ApiError> {
    blocking(move || play_on(&node, id, Some(&req.output))).await
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
        (status = NOT_FOUND, description = "No such receiver", body = ErrorBody),
        (status = CONFLICT, description = "The receiver is not playing", body = ErrorBody),
        (status = INTERNAL_SERVER_ERROR, description = "The audio system did not move the stream", body = ErrorBody),
        (status = SERVICE_UNAVAILABLE, description = "No audio system", body = ErrorBody),
    ),
)]
async fn clear_receiver_output(
    State(node): State<Arc<Node>>,
    Id(id): Id<ReceiverId>,
) -> Result<StatusCode, ApiError> {
    blocking(move || play_on(&node, id, None)).await
}

/// Moves receiver `id`'s stream to `output`, or with `None` to the
/// default output, in place of any wires of its source.
fn play_on(node: &Node, id: ReceiverId, output: Option<&str>) -> Result<StatusCode, Fail> {
    let Some(engine) = &node.engine else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()));
    };
    if !node
        .connections
        .lock()
        .unwrap()
        .receiver_ids()
        .contains(&id)
    {
        return Err((StatusCode::NOT_FOUND, format!("no receiver {id}")));
    }
    if let Some(name) = output
        && crate::outputs::View::of(node).find(name).is_none()
    {
        return Err((StatusCode::NOT_FOUND, format!("no output {name}")));
    }
    let engine = engine.lock().unwrap();
    match engine.set_receiver_output(id, output) {
        Ok(true) => {
            if let Some(key) = engine.receiver_key(id).flatten() {
                let mut sources = node.sources.lock().unwrap();
                sources.choose(key, output);
                if let Some(s) = sources.get(key) {
                    node.wires.lock().unwrap().remove_source(s.id);
                }
            }
            drop(engine);
            tracing::info!(id, output = output.unwrap_or("default"), "receiver output");
            crate::sources::follow(node);
            crate::wires::follow(node);
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((
            StatusCode::CONFLICT,
            format!("receiver {id} is not playing"),
        )),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
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
    let bad = |e: String| ApiError(StatusCode::BAD_REQUEST, e);
    let unknown = |e: String| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e);
    let (host, port) = split_host_port(peer).ok_or_else(|| bad(format!("bad peer {peer:?}")))?;
    let port = port.unwrap_or(RTP_PORT);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    // Also link-local addresses with an interface name (`fe80::1%eth0`).
    let found: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| unknown(format!("resolving {peer}: {e}")))?
        .collect();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or(found.first())
        .copied()
        .ok_or_else(|| unknown(format!("{peer} has no address")))
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
                "/x-orpheus/v1/inputs",
                "/x-orpheus/v1/nodes",
                "/x-orpheus/v1/nodes/{id}",
                "/x-orpheus/v1/outputs",
                "/x-orpheus/v1/outputs/{name}",
                "/x-orpheus/v1/outputs/{name}/default",
                "/x-orpheus/v1/outputs/{name}/type",
                "/x-orpheus/v1/receivers/{id}/output",
                "/x-orpheus/v1/routes",
                "/x-orpheus/v1/routes/{id}",
                "/x-orpheus/v1/routes/{id}/layout",
                "/x-orpheus/v1/routes/{id}/sdp",
                "/x-orpheus/v1/settings",
                "/x-orpheus/v1/sources",
                "/x-orpheus/v1/sources/{id}",
                "/x-orpheus/v1/sources/{id}/layout",
                "/x-orpheus/v1/status",
                "/x-orpheus/v1/wires",
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
