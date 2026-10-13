//! The IS-05 Connection API (`/x-nmos/connection/v1.1/…`): how the senders
//! and the receiver are routed.
//!
//! Each port has staged parameters, which controllers patch, and active
//! ones, which the engine runs. Staged parameters become active at once or
//! at a scheduled time. `POST /x-orpheus/v1/senders` creates a sender and
//! activates its destination the same way.
//!
//! The parameters last activated are saved to `routes.json` in the state
//! directory, as requested (with `auto`), and activated again at start.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use orpheus_engine::{Engine, Filter, Group, Route, SenderConfig, SenderId};
use orpheus_nmos::is04::Version;
use orpheus_nmos::is05::{
    self, Activation, Auto, Mode, ReceiverParams, ReceiverTransport, SenderParams, SenderTransport,
    TransportFile,
};
use orpheus_nmos::{Uuid, resource_id, sdp};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Node;
use crate::config::RTP_PORT;
use crate::nmos::{self, error};

const BASE: &str = "/x-nmos/connection/v1.1";

/// A failed request: status and message.
type Fail = (StatusCode, String);

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

/// The connection state of all ports.
pub struct Connections {
    senders: BTreeMap<SenderId, Port<SenderParams>>,
    receiver: Option<Port<ReceiverParams>>,
    /// Where the active parameters are saved.
    path: PathBuf,
    /// Numbers scheduled activations, so a cancelled one never fires.
    generation: u64,
}

struct Port<P> {
    staged: P,
    /// What the port runs, with every `auto` resolved.
    active: P,
    /// The parameters `active` was resolved from.
    requested: P,
    pending: Option<Pending>,
}

/// A scheduled activation.
struct Pending {
    generation: u64,
    task: tokio::task::AbortHandle,
}

impl<P: Params> Port<P> {
    /// A port running `active`, resolved from `requested`, which is also
    /// staged.
    fn new(mut requested: P, active: P) -> Self {
        *requested.activation() = Activation::default();
        Self {
            staged: requested.clone(),
            active,
            requested,
            pending: None,
        }
    }

    fn cancel(&mut self) {
        if let Some(p) = self.pending.take() {
            p.task.abort();
        }
    }
}

trait Params: Clone + Serialize + DeserializeOwned {
    fn activation(&mut self) -> &mut Activation;
}

impl Params for SenderParams {
    fn activation(&mut self) -> &mut Activation {
        &mut self.activation
    }
}

impl Params for ReceiverParams {
    fn activation(&mut self) -> &mut Activation {
        &mut self.activation
    }
}

/// Which port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortId {
    Sender(SenderId),
    Receiver,
}

/// What `routes.json` holds: the requested parameters of each port.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    senders: Vec<SavedSender>,
    receiver: Option<ReceiverParams>,
}

#[derive(Serialize, Deserialize)]
struct SavedSender {
    id: SenderId,
    name: String,
    /// Where it sent to, in case the request left that to `auto`.
    peer: SocketAddr,
    requested: SenderParams,
}

impl Connections {
    pub fn new(path: PathBuf) -> Self {
        Self {
            senders: BTreeMap::new(),
            receiver: None,
            path,
            generation: 0,
        }
    }

    pub fn sender_active(&self, id: SenderId) -> Option<&SenderParams> {
        self.senders.get(&id).map(|p| &p.active)
    }

    pub fn receiver_active(&self) -> Option<&ReceiverParams> {
        self.receiver.as_ref().map(|p| &p.active)
    }

    fn save(&self, engine: Option<&Engine>) {
        let names: BTreeMap<SenderId, String> = engine
            .map(|e| e.status().senders)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, s)| (id, s.name))
            .collect();
        let saved = Saved {
            senders: self
                .senders
                .iter()
                .filter_map(|(&id, p)| {
                    Some(SavedSender {
                        id,
                        name: names.get(&id).cloned().unwrap_or_default(),
                        peer: destination(&p.active)?,
                        requested: p.requested.clone(),
                    })
                })
                .collect(),
            receiver: self.receiver.as_ref().map(|p| p.requested.clone()),
        };
        let json = serde_json::to_vec_pretty(&saved).expect("routes serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving routes");
        }
    }
}

/// Sets up the receiver's connection and restarts the senders saved before
/// the last stop.
pub fn restore(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let mut conns = node.connections.lock().unwrap();
    let saved: Saved = match std::fs::read(&conns.path) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_else(|e| {
            tracing::warn!(error = %e, path = %conns.path.display(), "ignoring saved routes");
            Saved::default()
        }),
        Err(_) => Saved::default(),
    };
    let mut engine = engine.lock().unwrap();

    if engine.status().receiver.is_some() {
        let params = saved.receiver.unwrap_or_else(|| ReceiverParams {
            sender_id: None,
            master_enable: true,
            activation: Activation::default(),
            transport_file: TransportFile::default(),
            transport_params: vec![ReceiverTransport {
                source_ip: None,
                multicast_ip: None,
                interface_ip: Auto::Auto,
                destination_port: Auto::Auto,
                rtp_enabled: true,
            }],
        });
        match apply_receiver(&mut engine, node.receiver_port, &params) {
            Ok(active) => conns.receiver = Some(Port::new(params, active)),
            Err((_, e)) => tracing::warn!(error = %e, "restoring the receiver's connection"),
        }
    }

    for s in saved.senders {
        let peer = s.peer;
        let started = engine
            .insert_sender(s.id, &sender_config(node, s.id, s.name, peer))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
            .and_then(|()| apply_sender(&mut engine, s.id, &s.requested));
        match started {
            Ok(active) => {
                tracing::info!(id = s.id, %peer, "sender restored");
                conns.senders.insert(s.id, Port::new(s.requested, active));
            }
            Err((_, e)) => {
                engine.remove_sender(s.id);
                tracing::warn!(id = s.id, %peer, error = %e, "restoring sender");
            }
        }
    }
}

fn sender_config(node: &Node, id: SenderId, name: String, peer: SocketAddr) -> SenderConfig {
    SenderConfig {
        name: format!("orpheus.send.{id}"),
        description: name,
        format: nmos::FORMAT,
        encoding: nmos::ENCODING,
        peer,
        quantum: node.quantum,
    }
}

/// A sender's destination, from concrete parameters.
fn destination(p: &SenderParams) -> Option<SocketAddr> {
    let leg = p.transport_params.first()?;
    Some(SocketAddr::new(
        leg.destination_ip.value()?,
        leg.destination_port.value()?,
    ))
}

/// Starts a sender to `peer` and activates that destination.
pub fn add_sender(node: &Node, peer: SocketAddr, name: String) -> Result<SenderId, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    let id = engine.next_sender_id();
    engine
        .insert_sender(id, &sender_config(node, id, name, peer))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let params = SenderParams {
        receiver_id: None,
        master_enable: true,
        activation: Activation::default(),
        transport_params: vec![SenderTransport {
            source_ip: Auto::Auto,
            destination_ip: Auto::Value(peer.ip()),
            source_port: Auto::Auto,
            destination_port: Auto::Value(peer.port()),
            rtp_enabled: true,
        }],
    };
    let mut active = match apply_sender(&mut engine, id, &params) {
        Ok(active) => active,
        Err(e) => {
            engine.remove_sender(id);
            return Err(e);
        }
    };
    active.activation = Activation {
        mode: Some(Mode::ActivateImmediate),
        requested_time: None,
        activation_time: Some(nmos::now()),
    };
    conns.senders.insert(id, Port::new(params, active));
    conns.save(Some(&engine));
    Ok(id)
}

/// Stops a sender; false if there is no such sender.
pub fn remove_sender(node: &Node, id: SenderId) -> Result<bool, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    if let Some(mut port) = conns.senders.remove(&id) {
        port.cancel();
    }
    let removed = engine.remove_sender(id);
    conns.save(Some(&engine));
    Ok(removed)
}

fn lock_engine(node: &Node) -> Result<std::sync::MutexGuard<'_, Engine>, Fail> {
    node.engine
        .as_ref()
        .map(|e| e.lock().unwrap())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no audio system".into()))
}

fn unspecified(like: IpAddr) -> IpAddr {
    match like {
        IpAddr::V4(_) => [0, 0, 0, 0].into(),
        IpAddr::V6(_) => [0u16; 8].into(),
    }
}

/// Points sender `id` where `p` says; returns `p` with every `auto`
/// resolved.
fn apply_sender(engine: &mut Engine, id: SenderId, p: &SenderParams) -> Result<SenderParams, Fail> {
    let not_found = || (StatusCode::NOT_FOUND, format!("no sender {id}"));
    let status = |e: &Engine| {
        e.status()
            .senders
            .into_iter()
            .find(|(n, _)| *n == id)
            .map(|(_, s)| s)
    };
    let current = status(engine).ok_or_else(not_found)?;
    let leg = &p.transport_params[0];
    let ip = leg.destination_ip.value().unwrap_or(current.peer.ip());
    let port = leg.destination_port.value().unwrap_or(RTP_PORT);
    // Keeps the interface of a link-local peer.
    let mut peer = current.peer;
    if peer.ip() == ip {
        peer.set_port(port);
    } else {
        peer = SocketAddr::new(ip, port);
    }
    let source = SocketAddr::new(
        leg.source_ip.value().unwrap_or(unspecified(ip)),
        leg.source_port.value().unwrap_or(0),
    );
    let route = Route {
        peer,
        source,
        enabled: p.master_enable && leg.rtp_enabled,
    };
    engine.set_sender_route(id, route).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("sending from {source}: {e}"),
        )
    })?;
    let now = status(engine).ok_or_else(not_found)?;
    let source_ip = leg
        .source_ip
        .value()
        .or_else(|| nmos::route(peer).map(|(ip, _)| ip))
        .unwrap_or(source.ip());
    Ok(SenderParams {
        transport_params: vec![SenderTransport {
            source_ip: Auto::Value(source_ip),
            destination_ip: Auto::Value(ip),
            source_port: Auto::Value(now.source_port),
            destination_port: Auto::Value(port),
            rtp_enabled: leg.rtp_enabled,
        }],
        ..p.clone()
    })
}

/// Sets the receiver's filter from `p`; returns `p` with every `auto`
/// resolved.
fn apply_receiver(
    engine: &mut Engine,
    default_port: u16,
    p: &ReceiverParams,
) -> Result<ReceiverParams, Fail> {
    let leg = &p.transport_params[0];
    let port = leg.destination_port.value().unwrap_or(default_port);
    engine.set_receiver_port(port).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("listening on port {port}: {e}"),
        )
    })?;
    let status = engine
        .status()
        .receiver
        .ok_or((StatusCode::NOT_FOUND, "no receiver".to_string()))?;
    let payload_type = match &p.transport_file.data {
        Some(data) => Some(sdp::parse(data).map_err(bad)?.payload_type),
        None => None,
    };
    // Of the same family as the stream, if it says.
    let family = leg.multicast_ip.or(leg.source_ip).map(|ip| ip.is_ipv4());
    let interface_ip = leg.interface_ip.value().or_else(|| {
        let ips = nmos::local_ips();
        let fits = |ip: &&IpAddr| family.is_none_or(|v4| ip.is_ipv4() == v4);
        ips.iter().find(fits).or(ips.first()).copied()
    });
    // Joined on that interface: without a default route, the system has
    // no choice to make.
    let group = leg.multicast_ip.map(|address| Group {
        address,
        interface: interface_ip,
    });
    engine
        .set_receiver_filter(Filter {
            enabled: p.master_enable && leg.rtp_enabled,
            source: leg.source_ip,
            payload_type,
            group,
        })
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("joining {group:?}: {e}"),
            )
        })?;
    let interface_ip = interface_ip.unwrap_or(status.listen.ip());
    Ok(ReceiverParams {
        transport_params: vec![ReceiverTransport {
            interface_ip: Auto::Value(interface_ip),
            destination_port: Auto::Value(status.listen.port()),
            ..leg.clone()
        }],
        ..p.clone()
    })
}

fn check_sender(p: &SenderParams) -> Result<(), String> {
    let leg = &p.transport_params[0];
    if let Auto::Value(ip) = leg.source_ip
        && !nmos::local_ips().contains(&ip)
    {
        return Err(format!("{ip} is not an address of this node"));
    }
    if let (Auto::Value(src), Auto::Value(dst)) = (leg.source_ip, leg.destination_ip)
        && src.is_ipv4() != dst.is_ipv4()
    {
        return Err(format!("cannot send from {src} to {dst}"));
    }
    if let Auto::Value(ip) = leg.destination_ip
        && ip.is_unspecified()
    {
        return Err("no destination".into());
    }
    if leg.destination_port == Auto::Value(0) {
        return Err("destination port 0".into());
    }
    Ok(())
}

fn check_receiver(p: &ReceiverParams) -> Result<(), String> {
    let leg = &p.transport_params[0];
    if let Auto::Value(ip) = leg.interface_ip
        && !nmos::local_ips().contains(&ip)
    {
        return Err(format!("{ip} is not an address of this node"));
    }
    if let Some(ip) = leg.multicast_ip
        && !ip.is_multicast()
    {
        return Err(format!("{ip} is not a multicast group"));
    }
    if leg.destination_port == Auto::Value(0) {
        return Err("destination port 0".into());
    }
    let file = &p.transport_file;
    match (&file.data, file.kind.as_deref()) {
        (None, _) => {}
        (Some(_), Some(is05::TRANSPORT_FILE_SDP)) => {}
        (Some(_), kind) => return Err(format!("transport file type {kind:?}, not SDP")),
    }
    if let Some(data) = &file.data {
        let s = sdp::parse(data)?;
        let want = nmos::FORMAT;
        if (s.encoding.as_str(), s.rate, s.channels)
            != (nmos::ENCODING.name(), want.rate, want.channels)
        {
            return Err(format!(
                "the receiver plays {}/{}/{}, not {}/{}/{}",
                nmos::ENCODING.name(),
                want.rate,
                want.channels,
                s.encoding,
                s.rate,
                s.channels
            ));
        }
    }
    Ok(())
}

/// Takes what a receiver's new transport file says about the transport
/// parameters, except those the request sets itself.
fn from_transport_file(p: &mut ReceiverParams, body: &Value) -> Result<(), String> {
    let Some(data) = body.pointer("/transport_file/data").and_then(Value::as_str) else {
        return Ok(());
    };
    let s = sdp::parse(data)?;
    let explicit = |key: &str| {
        body.pointer(&format!("/transport_params/0/{key}"))
            .is_some()
    };
    let leg = &mut p.transport_params[0];
    if !explicit("source_ip") {
        leg.source_ip = s.source;
    }
    if !explicit("multicast_ip") {
        leg.multicast_ip = s.destination.is_multicast().then_some(s.destination);
    }
    if !explicit("destination_port") {
        leg.destination_port = Auto::Value(s.port);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP

pub fn router() -> Router<Arc<Node>> {
    let list = |items: &'static [&'static str]| get(move || async move { Json(items) });
    Router::new()
        .route("/x-nmos/connection", list(&["v1.1/"]))
        .route(BASE, list(&["bulk/", "single/"]))
        .route(&format!("{BASE}/single"), list(&["senders/", "receivers/"]))
        .route(&format!("{BASE}/bulk"), list(&["senders/", "receivers/"]))
        .route(&format!("{BASE}/bulk/{{kind}}"), post(bulk))
        .route(&format!("{BASE}/single/{{kind}}"), get(ports))
        .route(&format!("{BASE}/single/{{kind}}/{{id}}"), get(port_index))
        .route(
            &format!("{BASE}/single/{{kind}}/{{id}}/constraints"),
            get(constraints),
        )
        .route(
            &format!("{BASE}/single/{{kind}}/{{id}}/staged"),
            get(staged).patch(patch_staged),
        )
        .route(
            &format!("{BASE}/single/{{kind}}/{{id}}/active"),
            get(active),
        )
        .route(
            &format!("{BASE}/single/{{kind}}/{{id}}/transporttype"),
            get(transport_type),
        )
        .route(
            &format!("{BASE}/single/{{kind}}/{{id}}/transportfile"),
            get(transport_file),
        )
}

/// The port a URL names: `senders` or `receivers` and its UUID.
fn find(node: &Node, kind: &str, id: &str) -> Result<PortId, Fail> {
    let not_found = || (StatusCode::NOT_FOUND, format!("no {kind} {id}"));
    let uuid: Uuid = id.parse().map_err(|_| not_found())?;
    let conns = node.connections.lock().unwrap();
    match kind {
        "senders" => conns
            .senders
            .keys()
            .find(|&&n| resource_id(node.id, &format!("sender/{n}")) == uuid)
            .map(|&n| PortId::Sender(n)),
        "receivers" => (conns.receiver.is_some() && uuid == resource_id(node.id, "receiver/0"))
            .then_some(PortId::Receiver),
        _ => None,
    }
    .ok_or_else(not_found)
}

fn uuid_of(node: &Node, port: PortId) -> Uuid {
    match port {
        PortId::Sender(n) => resource_id(node.id, &format!("sender/{n}")),
        PortId::Receiver => resource_id(node.id, "receiver/0"),
    }
}

fn reply(result: Result<Value, Fail>) -> Response {
    match result {
        Ok(v) => Json(v).into_response(),
        Err((code, msg)) => error(code, msg),
    }
}

async fn ports(State(node): State<Arc<Node>>, Path(kind): Path<String>) -> Response {
    let conns = node.connections.lock().unwrap();
    let ids: Vec<PortId> = match kind.as_str() {
        "senders" => conns.senders.keys().map(|&n| PortId::Sender(n)).collect(),
        "receivers" => conns.receiver.iter().map(|_| PortId::Receiver).collect(),
        _ => return error(StatusCode::NOT_FOUND, format!("no {kind}")),
    };
    drop(conns);
    let list: Vec<String> = ids
        .into_iter()
        .map(|p| format!("{}/", uuid_of(&node, p)))
        .collect();
    Json(list).into_response()
}

async fn port_index(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    reply(find(&node, &kind, &id).map(|port| match port {
        PortId::Sender(_) => json!([
            "constraints/",
            "staged/",
            "active/",
            "transportfile/",
            "transporttype/"
        ]),
        PortId::Receiver => json!(["constraints/", "staged/", "active/", "transporttype/"]),
    }))
}

async fn constraints(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    let ips: Vec<String> = nmos::local_ips().iter().map(ToString::to_string).collect();
    reply(find(&node, &kind, &id).map(|port| match port {
        PortId::Sender(_) => json!([{
            "source_ip": {"enum": ips},
            "destination_ip": {},
            "source_port": {},
            "destination_port": {"minimum": 1, "maximum": 65535},
            "rtp_enabled": {},
        }]),
        // Ports below 1024 need privileges.
        PortId::Receiver => json!([{
            "source_ip": {},
            "multicast_ip": {},
            "interface_ip": {"enum": ips},
            "destination_port": {"minimum": 1024, "maximum": 65535},
            "rtp_enabled": {},
        }]),
    }))
}

async fn staged(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    reply(find(&node, &kind, &id).and_then(|port| view(&node, port, |p| &p.staged, |p| &p.staged)))
}

async fn active(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    reply(find(&node, &kind, &id).and_then(|port| view(&node, port, |p| &p.active, |p| &p.active)))
}

/// Staged or active parameters of a port, as JSON.
fn view(
    node: &Node,
    port: PortId,
    sender: fn(&Port<SenderParams>) -> &SenderParams,
    receiver: fn(&Port<ReceiverParams>) -> &ReceiverParams,
) -> Result<Value, Fail> {
    let conns = node.connections.lock().unwrap();
    let gone = || (StatusCode::NOT_FOUND, "gone".to_string());
    match port {
        PortId::Sender(n) => conns.senders.get(&n).map(|p| json!(sender(p))),
        PortId::Receiver => conns.receiver.as_ref().map(|p| json!(receiver(p))),
    }
    .ok_or_else(gone)
}

async fn transport_type(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    reply(find(&node, &kind, &id).map(|_| json!(is05::TRANSPORT_RTP)))
}

async fn transport_file(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    let port = match find(&node, &kind, &id) {
        Ok(PortId::Sender(n)) => n,
        Ok(PortId::Receiver) => {
            return error(
                StatusCode::NOT_FOUND,
                "receivers have no transport file".into(),
            );
        }
        Err((code, msg)) => return error(code, msg),
    };
    let sdp = tokio::task::spawn_blocking(move || nmos::sdp(&node, port)).await;
    match sdp {
        Ok(Some(sdp)) => ([(header::CONTENT_TYPE, is05::TRANSPORT_FILE_SDP)], sdp).into_response(),
        _ => error(StatusCode::NOT_FOUND, format!("no sender {id}")),
    }
}

async fn patch_staged(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let result = tokio::task::spawn_blocking(move || {
        let port = find(&node, &kind, &id)?;
        patch(&node, port, body)
    })
    .await
    .unwrap_or_else(|e| Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())));
    match result {
        Ok((code, v)) => (code, Json(v)).into_response(),
        Err((code, msg)) => error(code, msg),
    }
}

#[derive(Deserialize)]
struct BulkItem {
    id: String,
    params: Value,
}

async fn bulk(
    State(node): State<Arc<Node>>,
    Path(kind): Path<String>,
    body: Result<Json<Vec<BulkItem>>, JsonRejection>,
) -> Response {
    if kind != "senders" && kind != "receivers" {
        return error(StatusCode::NOT_FOUND, format!("no {kind}"));
    }
    let items = match body {
        Ok(Json(items)) => items,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let results = tokio::task::spawn_blocking(move || {
        items
            .into_iter()
            .map(|item| {
                let result =
                    find(&node, &kind, &item.id).and_then(|p| patch(&node, p, item.params));
                match result {
                    Ok((code, _)) => json!({"id": item.id, "code": code.as_u16()}),
                    Err((code, msg)) => json!({
                        "id": item.id,
                        "code": code.as_u16(),
                        "error": msg,
                        "debug": null,
                    }),
                }
            })
            .collect::<Vec<_>>()
    })
    .await;
    match results {
        Ok(r) => Json(r).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Staging and activation

/// Applies a PATCH to a port's staged parameters and activates them if it
/// asks to. Returns the status and the staged parameters to reply with.
fn patch(node: &Arc<Node>, port: PortId, body: Value) -> Result<(StatusCode, Value), Fail> {
    let mut conns = node.connections.lock().unwrap();
    let conns = &mut *conns;
    match port {
        PortId::Sender(n) => {
            let p = conns
                .senders
                .get_mut(&n)
                .ok_or((StatusCode::NOT_FOUND, "gone".into()))?;
            let staged = staged_patch(p, &body, |s| is05::patch(s, body.clone()))?;
            check_sender(&staged).map_err(bad)?;
            let reply = stage_or_activate(node, &mut conns.generation, p, port, staged)?;
            conns.save(node.engine.as_ref().map(|e| e.lock().unwrap()).as_deref());
            Ok(reply)
        }
        PortId::Receiver => {
            let p = conns
                .receiver
                .as_mut()
                .ok_or((StatusCode::NOT_FOUND, "gone".into()))?;
            let staged = staged_patch(p, &body, |s| {
                let mut new = is05::patch(s, body.clone())?;
                from_transport_file(&mut new, &body)?;
                Ok(new)
            })?;
            check_receiver(&staged).map_err(bad)?;
            let reply = stage_or_activate(node, &mut conns.generation, p, port, staged)?;
            conns.save(node.engine.as_ref().map(|e| e.lock().unwrap()).as_deref());
            Ok(reply)
        }
    }
}

/// The staged parameters with the PATCH applied. While an activation is
/// scheduled, only a request that cancels it is taken.
fn staged_patch<P: Params>(
    port: &mut Port<P>,
    body: &Value,
    apply: impl FnOnce(&P) -> Result<P, String>,
) -> Result<P, Fail> {
    if port.pending.is_some() {
        let cancels = body.pointer("/activation").is_some()
            && body.pointer("/activation/mode").is_none_or(Value::is_null);
        if !cancels {
            return Err((
                StatusCode::LOCKED,
                "an activation is scheduled; cancel it first".into(),
            ));
        }
        port.cancel();
        *port.staged.activation() = Activation::default();
    }
    let mut base = port.staged.clone();
    // Times the API sets itself never come from the request.
    *base.activation() = Activation::default();
    let mut staged = apply(&base).map_err(bad)?;
    staged.activation().activation_time = None;
    Ok(staged)
}

fn stage_or_activate<P: Activate>(
    node: &Arc<Node>,
    generation: &mut u64,
    port: &mut Port<P>,
    id: PortId,
    mut staged: P,
) -> Result<(StatusCode, Value), Fail> {
    let activation = *staged.activation();
    let now = nmos::now();
    match activation.mode {
        None => {
            *staged.activation() = Activation::default();
            port.staged = staged;
            Ok((StatusCode::OK, json!(port.staged)))
        }
        Some(Mode::ActivateImmediate) => {
            let done = Activation {
                mode: Some(Mode::ActivateImmediate),
                requested_time: None,
                activation_time: Some(now),
            };
            let mut active = P::activate(node, id, &staged)?;
            *active.activation() = done;
            port.active = active;
            touch(node, id);
            *staged.activation() = Activation::default();
            port.requested = staged.clone();
            port.staged = staged.clone();
            *staged.activation() = done;
            Ok((StatusCode::OK, json!(staged)))
        }
        Some(mode) => {
            let requested = activation
                .requested_time
                .ok_or_else(|| bad("a scheduled activation needs requested_time"))?;
            let at = match mode {
                Mode::ActivateScheduledAbsolute => requested,
                _ => Version::from_tai_nanos(now.tai_nanos().saturating_add(requested.tai_nanos())),
            };
            *staged.activation() = Activation {
                activation_time: Some(at),
                ..activation
            };
            port.staged = staged;
            *generation += 1;
            let generation = *generation;
            let wait = Duration::from_nanos(at.tai_nanos().saturating_sub(now.tai_nanos()));
            let node = node.clone();
            let task = tokio::runtime::Handle::current().spawn(async move {
                tokio::time::sleep(wait).await;
                let _ = tokio::task::spawn_blocking(move || fire(&node, id, generation)).await;
            });
            port.pending = Some(Pending {
                generation,
                task: task.abort_handle(),
            });
            Ok((StatusCode::ACCEPTED, json!(port.staged)))
        }
    }
}

/// How the parameters of one kind of port reach the engine.
trait Activate: Params {
    fn activate(node: &Node, id: PortId, p: &Self) -> Result<Self, Fail>;
}

impl Activate for SenderParams {
    fn activate(node: &Node, id: PortId, p: &Self) -> Result<Self, Fail> {
        let PortId::Sender(n) = id else {
            unreachable!("sender parameters for a receiver")
        };
        apply_sender(&mut *lock_engine(node)?, n, p)
    }
}

impl Activate for ReceiverParams {
    fn activate(node: &Node, _: PortId, p: &Self) -> Result<Self, Fail> {
        apply_receiver(&mut *lock_engine(node)?, node.receiver_port, p)
    }
}

/// Gives the port's IS-04 resource a new version: an activation counts as
/// a change even where the resource looks the same.
fn touch(node: &Node, id: PortId) {
    node.nmos.lock().unwrap().touch(uuid_of(node, id));
}

/// Runs a scheduled activation, unless it was cancelled meanwhile.
fn fire(node: &Node, id: PortId, generation: u64) {
    let mut conns = node.connections.lock().unwrap();
    let result = match id {
        PortId::Sender(n) => conns
            .senders
            .get_mut(&n)
            .map(|p| fire_port(node, id, generation, p)),
        PortId::Receiver => conns
            .receiver
            .as_mut()
            .map(|p| fire_port(node, id, generation, p)),
    };
    match result {
        Some(Ok(true)) => conns.save(node.engine.as_ref().map(|e| e.lock().unwrap()).as_deref()),
        Some(Err((_, e))) => tracing::warn!(?id, error = %e, "scheduled activation failed"),
        _ => {}
    }
}

fn fire_port<P: Activate>(
    node: &Node,
    id: PortId,
    generation: u64,
    port: &mut Port<P>,
) -> Result<bool, Fail> {
    if port
        .pending
        .as_ref()
        .is_none_or(|p| p.generation != generation)
    {
        return Ok(false);
    }
    port.pending = None;
    let mut staged = port.staged.clone();
    let activation = *staged.activation();
    *port.staged.activation() = Activation::default();
    *staged.activation() = Activation::default();
    let mut active = P::activate(node, id, &staged)?;
    *active.activation() = activation;
    port.active = active;
    port.requested = staged;
    touch(node, id);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver(file: Option<&str>) -> ReceiverParams {
        ReceiverParams {
            sender_id: None,
            master_enable: true,
            activation: Activation::default(),
            transport_file: TransportFile {
                data: file.map(Into::into),
                kind: file.map(|_| is05::TRANSPORT_FILE_SDP.into()),
            },
            transport_params: vec![ReceiverTransport {
                source_ip: None,
                multicast_ip: None,
                interface_ip: Auto::Auto,
                destination_port: Auto::Auto,
                rtp_enabled: true,
            }],
        }
    }

    const SDP: &str = "v=0\r\no=- 1 1 IN IP4 10.0.0.1\r\ns=x\r\nc=IN IP4 10.0.0.2\r\nt=0 0\r\n\
                       m=audio 5004 RTP/AVP 97\r\na=rtpmap:97 L24/48000/2\r\n";

    #[test]
    fn receivers_check_their_transport_file() {
        assert_eq!(check_receiver(&receiver(None)), Ok(()));
        assert_eq!(check_receiver(&receiver(Some(SDP))), Ok(()));
        let other = |from: &str, to: &str| SDP.replace(from, to);
        for bad in [
            other("L24/48000/2", "L16/48000/2"),
            other("L24/48000/2", "L24/44100/2"),
        ] {
            assert!(check_receiver(&receiver(Some(&bad))).is_err(), "{bad}");
        }
        let multicast = SDP.replace("c=IN IP4 10.0.0.2", "c=IN IP4 239.1.1.1/32");
        assert_eq!(check_receiver(&receiver(Some(&multicast))), Ok(()));
        let mut p = receiver(None);
        p.transport_params[0].multicast_ip = Some("10.0.0.2".parse().unwrap());
        assert!(check_receiver(&p).is_err());
        let mut p = receiver(Some(SDP));
        p.transport_file.kind = Some("text/plain".into());
        assert!(check_receiver(&p).is_err());
        let mut p = receiver(None);
        p.transport_params[0].destination_port = Auto::Value(0);
        assert!(check_receiver(&p).is_err());
    }

    #[test]
    fn transport_files_set_parameters() {
        let mut p = receiver(None);
        let body = json!({"transport_file": {"data": SDP, "type": "application/sdp"}});
        from_transport_file(&mut p, &body).unwrap();
        assert_eq!(
            p.transport_params[0].source_ip,
            Some("10.0.0.1".parse().unwrap())
        );
        assert_eq!(p.transport_params[0].destination_port, Auto::Value(5004));
        assert_eq!(p.transport_params[0].multicast_ip, None);
        let multicast = SDP.replace("c=IN IP4 10.0.0.2", "c=IN IP4 239.1.1.1/32");
        let body = json!({"transport_file": {"data": multicast, "type": "application/sdp"}});
        from_transport_file(&mut p, &body).unwrap();
        assert_eq!(
            p.transport_params[0].multicast_ip,
            Some("239.1.1.1".parse().unwrap())
        );

        // The request's own parameters win.
        let mut p = receiver(None);
        let body = json!({
            "transport_file": {"data": SDP, "type": "application/sdp"},
            "transport_params": [{"source_ip": null}],
        });
        from_transport_file(&mut p, &body).unwrap();
        assert_eq!(p.transport_params[0].source_ip, None);
    }

    #[test]
    fn senders_check_addresses() {
        let p = |src: Auto<IpAddr>, dst: &str| SenderParams {
            receiver_id: None,
            master_enable: true,
            activation: Activation::default(),
            transport_params: vec![SenderTransport {
                source_ip: src,
                destination_ip: Auto::Value(dst.parse().unwrap()),
                source_port: Auto::Auto,
                destination_port: Auto::Value(5004),
                rtp_enabled: true,
            }],
        };
        assert_eq!(check_sender(&p(Auto::Auto, "10.0.0.2")), Ok(()));
        assert_eq!(check_sender(&p(Auto::Auto, "239.1.1.1")), Ok(()));
        assert!(check_sender(&p(Auto::Auto, "0.0.0.0")).is_err());
        assert!(check_sender(&p(Auto::Value("192.0.2.77".parse().unwrap()), "10.0.0.2")).is_err());
    }
}
