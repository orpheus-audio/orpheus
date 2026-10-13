//! The IS-05 Connection API (`/x-nmos/connection/v1.1/…`): how the senders
//! and the receiver are routed.
//!
//! Each port has staged parameters, which controllers patch, and active
//! ones, which the engine runs. Staged parameters become active at once or
//! at a scheduled time. `POST /x-orpheus/v1/senders` creates a sender and
//! activates its destination the same way; unless asked to only send, it
//! also connects a free receiver on the peer to it, through the peer's
//! IS-05 API, and disconnects it again when the sender goes.
//!
//! Each connection has a receiver of its own, and there is always one free
//! receiver more, up to `[receiver] max`. Receiver `k` (NMOS ID from
//! `receiver/k`) listens on `[receiver] port + 2k`, and only while it is
//! connected does it run, with a playback stream named after its sender.
//! A connection keeps its receiver across restarts, so a slot saved for a
//! connection is never handed out to another. Free receivers beyond the
//! lowest go after [`FREE_GRACE`]. What the free receiver plays is a
//! setting (`[receiver] accept`): nothing, or any stream sent to its port.
//!
//! The parameters last activated are saved to `routes.json` in the state
//! directory, as requested (with `auto`), and activated again at start.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use orpheus_core::OutputType;
use orpheus_engine::{
    Engine, Filter, Group, ReceiverConfig, ReceiverId, Route, SenderConfig, SenderId,
};
use orpheus_nmos::is04::Version;
use orpheus_nmos::is05::{
    self, Activation, Auto, Mode, ReceiverParams, ReceiverTransport, SenderParams, SenderTransport,
    TransportFile,
};
use orpheus_nmos::sdp::Priority;
use orpheus_nmos::{Uuid, resource_id, sdp};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Node;
use crate::config::{Accept, RTP_PORT};
use crate::nmos::FORMAT;
use crate::nmos::{self, error};
use crate::outputs;
use crate::peer::{self, PeerReceiver};
use crate::policy::State as PolicyState;

const BASE: &str = "/x-nmos/connection/v1.1";

/// How long a free receiver beyond the lowest stays, so that controllers
/// that just saw it can still use it.
const FREE_GRACE: Duration = Duration::from_secs(60);

/// A failed request: status and message.
type Fail = (StatusCode, String);

fn bad(msg: impl Into<String>) -> Fail {
    (StatusCode::BAD_REQUEST, msg.into())
}

/// The connection state of all ports.
pub struct Connections {
    senders: BTreeMap<SenderId, Port<SenderParams>>,
    /// The receivers advertised: the connected ones and the free ones.
    receivers: BTreeMap<ReceiverId, Port<ReceiverParams>>,
    /// Since when each free receiver is free.
    free_since: BTreeMap<ReceiverId, Instant>,
    /// What each sender is connected to.
    links: BTreeMap<SenderId, Link>,
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
    Receiver(ReceiverId),
}

/// What `routes.json` holds: the requested parameters of each port; of
/// the receivers only the connected ones.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    senders: Vec<SavedSender>,
    receivers: BTreeMap<ReceiverId, ReceiverParams>,
}

#[derive(Serialize, Deserialize)]
struct SavedSender {
    id: SenderId,
    name: String,
    /// Where it sent to, in case the request left that to `auto`.
    peer: SocketAddr,
    requested: SenderParams,
    #[serde(flatten)]
    link: Link,
}

/// The other end of a sender.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Link {
    /// The user named the sink; otherwise it follows the peer's name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    named: bool,
    /// Written into the SDP file, for the peer's policy.
    #[serde(default)]
    priority: Priority,
    /// Whether the peer's receiver plays the sender, as far as we know.
    #[serde(skip)]
    state: PeerState,
    /// The peer's receiver warns that our packets arrive with a wide
    /// spread.
    #[serde(skip)]
    jitter: bool,
    /// What the peer's policy does to the stream, if it holds it back.
    #[serde(skip)]
    held: Option<PolicyState>,
    /// The kind of output the peer's receiver announces, which the sink
    /// shows unless overridden here.
    #[serde(default)]
    output: OutputType,
    /// The peer's receiver we connected, to disconnect it when the sender
    /// goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receiver: Option<PeerReceiver>,
}

/// How a sender's connection to a peer's receiver stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Being connected.
    Connecting,
    /// The receiver plays the sender.
    #[default]
    Connected,
    /// The peer is offline or doesn't answer; the sender keeps sending, so
    /// that the stream plays again when the peer is back.
    Unreachable,
    /// The receiver plays another sender, or none: someone else connected
    /// it. The sender pauses until the receiver is connected to it again.
    Disconnected,
}

/// The peer's receiver a sender is connected to.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PeerLink {
    /// Base URL of the peer's API.
    pub api: String,
    #[schema(value_type = String)]
    pub id: Uuid,
    pub state: PeerState,
    /// What the peer's policy does to the stream: `muted` while another
    /// stream plays there, `ducked` under a high-priority one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, example = "muted")]
    pub policy: Option<PolicyState>,
    /// The receiver warns that the packets arrive with a wide spread.
    #[serde(skip)]
    pub jitter: bool,
}

/// A sender connected to a peer's receiver.
pub struct Connected {
    pub id: SenderId,
    pub name: String,
    pub destination: SocketAddr,
    pub receiver: Uuid,
}

impl Connections {
    pub fn new(path: PathBuf) -> Self {
        Self {
            senders: BTreeMap::new(),
            receivers: BTreeMap::new(),
            free_since: BTreeMap::new(),
            links: BTreeMap::new(),
            path,
            generation: 0,
        }
    }

    pub fn sender_active(&self, id: SenderId) -> Option<&SenderParams> {
        self.senders.get(&id).map(|p| &p.active)
    }

    pub fn receiver_active(&self, id: ReceiverId) -> Option<&ReceiverParams> {
        self.receivers.get(&id).map(|p| &p.active)
    }

    /// The receivers advertised.
    pub fn receiver_ids(&self) -> Vec<ReceiverId> {
        self.receivers.keys().copied().collect()
    }

    /// The type each sender's peer announces.
    pub fn peer_types(&self) -> BTreeMap<SenderId, OutputType> {
        self.links.iter().map(|(&id, l)| (id, l.output)).collect()
    }

    /// The peers' receivers the senders are connected to.
    pub fn peer_links(&self) -> BTreeMap<SenderId, PeerLink> {
        self.links
            .iter()
            .filter_map(|(&id, l)| {
                let r = l.receiver.as_ref()?;
                Some((
                    id,
                    PeerLink {
                        api: r.api.clone(),
                        id: r.id,
                        state: l.state,
                        policy: l.held,
                        jitter: l.jitter,
                    },
                ))
            })
            .collect()
    }

    /// The senders routed here.
    pub fn sender_ids(&self) -> Vec<SenderId> {
        self.senders.keys().copied().collect()
    }

    /// The priority each sender announces.
    pub fn sender_priority(&self, id: SenderId) -> Priority {
        self.links.get(&id).map(|l| l.priority).unwrap_or_default()
    }

    /// The priority of each connected receiver's sender, from its SDP
    /// file; normal if it doesn't say.
    pub fn receiver_priorities(&self) -> BTreeMap<ReceiverId, Priority> {
        self.receivers
            .iter()
            .filter(|(_, p)| p.active.master_enable)
            .filter_map(|(&id, p)| {
                let sdp = sdp::parse(p.active.transport_file.data.as_deref()?).ok()?;
                Some((id, sdp.priority))
            })
            .collect()
    }

    /// Who receiver `id` is connected to: the name in the sender's SDP
    /// file, else its address; `None` if no sender is connected.
    pub fn receiver_sender(&self, id: ReceiverId) -> Option<String> {
        let active = self.receiver_active(id).filter(|a| a.master_enable)?;
        let leg = active.transport_params.first()?;
        let named = active
            .transport_file
            .data
            .as_deref()
            .and_then(|d| sdp::parse(d).ok())
            .map(|s| s.name)
            .filter(|n| !n.is_empty() && n != "-");
        named.or_else(|| leg.source_ip.or(leg.multicast_ip).map(|ip| ip.to_string()))
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
                        link: self.links.get(&id).cloned().unwrap_or_default(),
                    })
                })
                .collect(),
            receivers: self
                .receivers
                .iter()
                .filter(|(_, p)| connected(&p.active))
                .map(|(&id, p)| (id, p.requested.clone()))
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&saved).expect("routes serialize");
        if let Err(e) = crate::home::write_atomic(&self.path, &json) {
            tracing::warn!(error = %e, path = %self.path.display(), "saving routes");
        }
    }
}

/// Sets up the receivers' connections and restarts the senders saved
/// before the last stop.
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

    if node.receiver.enabled {
        for (id, params) in saved.receivers {
            if id >= ReceiverId::from(node.receiver.max) {
                tracing::warn!(id, "dropping a saved receiver beyond [receiver] max");
                continue;
            }
            match apply_receiver(node, &mut engine, id, &params) {
                Ok(active) => {
                    conns.receivers.insert(id, Port::new(params, active));
                }
                Err((_, e)) => {
                    tracing::warn!(id, error = %e, "restoring a receiver's connection")
                }
            }
        }
        balance(node, &mut conns, &mut engine);
    }

    for s in saved.senders {
        let peer = s.peer;
        let started = engine
            .insert_sender(
                s.id,
                &sender_config(node, s.id, s.name, peer, sink_type(node, s.id, &s.link)),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
            .and_then(|()| apply_sender(&mut engine, s.id, &s.requested));
        match started {
            Ok(active) => {
                tracing::info!(id = s.id, %peer, "sender restored");
                conns.senders.insert(s.id, Port::new(s.requested, active));
                conns.links.insert(s.id, s.link);
            }
            Err((_, e)) => {
                engine.remove_sender(s.id);
                tracing::warn!(id = s.id, %peer, error = %e, "restoring sender");
            }
        }
    }
}

fn sender_config(
    node: &Node,
    id: SenderId,
    name: String,
    peer: SocketAddr,
    remote: OutputType,
) -> SenderConfig {
    SenderConfig {
        name: outputs::sender_sink(id),
        description: name,
        format: nmos::FORMAT,
        encoding: nmos::ENCODING,
        peer,
        remote,
        block_frames: node.block_frames,
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

/// Starts a sender to `peer` and activates that destination, without
/// telling the peer.
pub fn add_sender(
    node: &Node,
    peer: SocketAddr,
    name: String,
    priority: Priority,
) -> Result<SenderId, Fail> {
    let link = Link {
        named: true,
        priority,
        ..Link::default()
    };
    start_sender(node, peer, name, link)
}

/// Connects this node to a peer, both ends: picks a free receiver there,
/// starts a sender to it and connects the receiver to the sender. If the
/// receiver refuses, the sender goes again.
pub fn connect(
    node: &Node,
    peer: &str,
    name: Option<String>,
    force: bool,
    priority: Priority,
) -> Result<Connected, Fail> {
    let found = peer::resolve(node, peer)?;
    let ours: Vec<(SenderId, Uuid)> = node
        .connections
        .lock()
        .unwrap()
        .senders
        .keys()
        .map(|&n| (n, uuid_of(node, PortId::Sender(n))))
        .collect();
    let target = peer::find_receiver(&found, &ours, force)?;
    let named = name.is_some();
    let name = name
        .or_else(|| found.name.clone())
        .unwrap_or_else(|| peer.to_owned());
    let destination = SocketAddr::new(found.ip, target.port);
    let to = PeerReceiver {
        api: found.api.clone(),
        id: target.receiver,
    };
    let link = Link {
        named,
        priority,
        state: PeerState::Connecting,
        jitter: false,
        held: None,
        output: target.output,
        receiver: Some(to.clone()),
    };
    let id = start_sender(node, destination, name.clone(), link)?;
    let uuid = uuid_of(node, PortId::Sender(id));
    let connected = nmos::sdp(node, id)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no SDP file".into()))
        .and_then(|sdp| peer::connect(&to, uuid, &sdp));
    if let Err(e) = connected {
        stop_sender(node, id)?;
        return Err(e);
    }
    if let Some(link) = node.connections.lock().unwrap().links.get_mut(&id) {
        link.state = PeerState::Connected;
    }
    Ok(Connected {
        id,
        name,
        destination,
        receiver: target.receiver,
    })
}

fn start_sender(node: &Node, peer: SocketAddr, name: String, link: Link) -> Result<SenderId, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    let id = engine.next_sender_id();
    let remote = sink_type(node, id, &link);
    engine
        .insert_sender(id, &sender_config(node, id, name, peer, remote))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let params = SenderParams {
        receiver_id: link.receiver.as_ref().map(|r| r.id),
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
    conns.links.insert(id, link);
    conns.save(Some(&engine));
    Ok(id)
}

/// Stops a sender and disconnects the peer's receiver we connected to it;
/// false if there is no such sender. The sender goes even if the peer
/// cannot be reached.
pub fn remove_sender(node: &Node, id: SenderId) -> Result<bool, Fail> {
    let uuid = uuid_of(node, PortId::Sender(id));
    let link = node.connections.lock().unwrap().links.get(&id).cloned();
    let removed = stop_sender(node, id)?;
    if let Some(to) = link.and_then(|l| l.receiver)
        && let Err((_, e)) = peer::release(&to, uuid)
    {
        tracing::warn!(id, error = %e, "disconnecting the peer's receiver");
    }
    Ok(removed)
}

/// The type a sender's sink shows: the user's, else the peer's.
fn sink_type(node: &Node, id: SenderId, link: &Link) -> OutputType {
    node.outputs
        .lock()
        .unwrap()
        .get(&outputs::sender_sink(id))
        .unwrap_or(link.output)
}

/// Shows sender `id`'s sink with the type it should have now.
pub fn update_sink_type(node: &Node, id: SenderId) {
    let conns = node.connections.lock().unwrap();
    let Some(link) = conns.links.get(&id) else {
        return;
    };
    let remote = sink_type(node, id, link);
    if let Some(engine) = &node.engine {
        engine.lock().unwrap().set_sender_remote(id, remote);
    }
}

/// How often a peer's receiver is asked anyway, also when discovery
/// doesn't see the peer.
const RECHECK: Duration = Duration::from_secs(30);

/// What the peer watch last saw of a sender's peer.
#[derive(Default)]
struct Seen {
    /// The peer as discovery announced it: online, name, receiver counter.
    announced: Option<(bool, String, Option<u8>)>,
    asked: Option<Instant>,
}

/// Follows the peers' receivers the senders are connected to: their output
/// type goes to the sender's sink, their name too unless the user named the
/// sink, and a sender pauses while its receiver plays something else.
///
/// Each peer announces a receiver change counter over mDNS; it is asked
/// again when that, its name or its presence changes, and every 30 seconds
/// anyway, as mDNS may miss an update and doesn't see every peer.
pub async fn watch_peers(node: Arc<Node>) {
    let mut seen: BTreeMap<SenderId, Seen> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let node = node.clone();
        match tokio::task::spawn_blocking(move || {
            check_peers(&node, &mut seen);
            seen
        })
        .await
        {
            Ok(s) => seen = s,
            Err(_) => seen = BTreeMap::new(),
        }
    }
}

/// Drops free receivers that are no longer needed.
fn tend_receivers(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let mut conns = node.connections.lock().unwrap();
    balance(node, &mut conns, &mut engine.lock().unwrap());
}

fn check_peers(node: &Node, seen: &mut BTreeMap<SenderId, Seen>) {
    tend_receivers(node);
    let links: Vec<(SenderId, PeerReceiver)> = {
        let conns = node.connections.lock().unwrap();
        seen.retain(|id, _| conns.links.contains_key(id));
        conns
            .links
            .iter()
            .filter(|(_, l)| l.state != PeerState::Connecting)
            .filter_map(|(&id, l)| Some((id, l.receiver.clone()?)))
            .collect()
    };
    if links.is_empty() {
        return;
    }
    let nodes = node
        .discovery
        .as_ref()
        .map(|d| d.nodes())
        .unwrap_or_default();
    for (id, to) in links {
        let found = nodes.iter().find(|n| peer::serves(n, &to.api));
        let announced = found.map(|n| (n.online, n.name.clone(), n.receivers));
        let last = seen.entry(id).or_default();
        let due = announced != last.announced || last.asked.is_none_or(|t| t.elapsed() >= RECHECK);
        if !due {
            continue;
        }
        last.announced = announced;
        last.asked = Some(Instant::now());
        let playing = match found {
            Some(n) if !n.online => {
                Err((StatusCode::BAD_GATEWAY, format!("{} is offline", n.name)))
            }
            _ => peer::playing(&to),
        };
        update_link(node, id, &to, playing, found.map(|n| n.name.as_str()));
    }
}

/// Takes in what a peer's receiver said, or that it couldn't be asked.
fn update_link(
    node: &Node,
    id: SenderId,
    to: &PeerReceiver,
    playing: Result<peer::Playing, Fail>,
    name: Option<&str>,
) {
    let ours = uuid_of(node, PortId::Sender(id));
    let mut conns = node.connections.lock().unwrap();
    let Some(link) = conns
        .links
        .get_mut(&id)
        .filter(|l| l.receiver.as_ref() == Some(to) && l.state != PeerState::Connecting)
    else {
        return;
    };
    let was = link.state;
    let mut changed = false;
    match playing {
        Err((_, e)) => {
            if was != PeerState::Unreachable {
                tracing::warn!(id, error = %e, "peer unreachable");
            }
            link.state = PeerState::Unreachable;
            link.jitter = false;
            link.held = None;
        }
        Ok(p) => {
            link.jitter = p.jitter;
            if p.held != link.held {
                tracing::info!(id, held = ?p.held, "the peer's policy changed what it does to the stream");
                link.held = p.held;
            }
            link.state = if p.sender == Some(ours) {
                PeerState::Connected
            } else {
                PeerState::Disconnected
            };
            if p.output != link.output {
                tracing::info!(id, output = %p.output, "peer's output changed");
                link.output = p.output;
                changed = true;
            }
        }
    }
    let state = link.state;
    let rename = name.filter(|_| !link.named).map(str::to_owned);
    let remote = sink_type(node, id, link);
    let Some(engine) = &node.engine else {
        return;
    };
    let mut engine = engine.lock().unwrap();
    if state != was {
        match state {
            PeerState::Disconnected => {
                tracing::warn!(id, "the peer's receiver plays another sender; pausing")
            }
            PeerState::Connected if was == PeerState::Disconnected => {
                tracing::info!(id, "the peer's receiver plays the sender again; resuming")
            }
            PeerState::Connected if was == PeerState::Unreachable => {
                tracing::info!(id, "peer reachable again")
            }
            _ => {}
        }
        engine.pause_sender(id, state == PeerState::Disconnected);
    }
    if changed {
        engine.set_sender_remote(id, remote);
    }
    let current = engine
        .status()
        .senders
        .into_iter()
        .find(|(n, _)| *n == id)
        .map(|(_, s)| s.name);
    if let Some(name) = rename.filter(|n| current.as_ref().is_some_and(|c| c != n)) {
        tracing::info!(id, %name, "peer renamed; renaming its sink");
        engine.rename_sender(id, &name);
        changed = true;
    }
    if changed {
        conns.save(Some(&engine));
    }
}

/// Stops a sender, here only.
fn stop_sender(node: &Node, id: SenderId) -> Result<bool, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    if let Some(mut port) = conns.senders.remove(&id) {
        port.cancel();
    }
    conns.links.remove(&id);
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

/// Whether receiver parameters connect a sender: by ID, source or group.
fn connected(p: &ReceiverParams) -> bool {
    p.sender_id.is_some()
        || p.transport_params
            .first()
            .is_some_and(|l| l.source_ip.is_some() || l.multicast_ip.is_some())
}

/// The parameters of a free receiver.
fn free_params() -> ReceiverParams {
    ReceiverParams {
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
    }
}

/// The port receiver `id` listens on unless its connection asks for
/// another.
fn receiver_port(node: &Node, id: ReceiverId) -> u16 {
    // The config keeps the sum below 65536.
    (u32::from(node.receiver.port) + 2 * id) as u16
}

/// The NMOS ID of receiver `id`.
pub fn receiver_uuid(node: &Node, id: ReceiverId) -> Uuid {
    resource_id(node.id, &format!("receiver/{id}"))
}

/// Keeps one free receiver: adds the lowest free slot if none is free and
/// there is room, and removes free receivers beyond the lowest once they
/// have been free for [`FREE_GRACE`].
fn balance(node: &Node, conns: &mut Connections, engine: &mut Engine) {
    if !node.receiver.enabled {
        return;
    }
    let free: Vec<ReceiverId> = conns
        .receivers
        .iter()
        .filter(|(_, p)| !connected(&p.active))
        .map(|(&id, _)| id)
        .collect();
    conns.free_since.retain(|id, _| free.contains(id));
    for &id in &free {
        conns.free_since.entry(id).or_insert_with(Instant::now);
    }
    for &id in free.iter().skip(1) {
        let idle = conns.free_since[&id].elapsed() >= FREE_GRACE;
        if idle && conns.receivers[&id].pending.is_none() {
            conns.receivers.remove(&id);
            conns.free_since.remove(&id);
            engine.remove_receiver(id);
            tracing::debug!(id, "free receiver removed");
        }
    }
    if !free.is_empty() {
        return;
    }
    let Some(id) =
        (0..ReceiverId::from(node.receiver.max)).find(|id| !conns.receivers.contains_key(id))
    else {
        return;
    };
    let params = free_params();
    match apply_receiver(node, engine, id, &params) {
        Ok(active) => {
            conns.receivers.insert(id, Port::new(params, active));
            conns.free_since.insert(id, Instant::now());
            tracing::debug!(id, "free receiver added");
        }
        Err((_, e)) => tracing::warn!(id, error = %e, "adding a free receiver"),
    }
}

/// Runs receiver `id` as `p` says: starts it for a connection (or, with
/// `accept = "any"`, also while free), stops it otherwise, and sets its
/// port and filter. Returns `p` with every `auto` resolved.
fn apply_receiver(
    node: &Node,
    engine: &mut Engine,
    id: ReceiverId,
    p: &ReceiverParams,
) -> Result<ReceiverParams, Fail> {
    let leg = &p.transport_params[0];
    let port = leg
        .destination_port
        .value()
        .unwrap_or(receiver_port(node, id));
    let stream = match &p.transport_file.data {
        Some(data) => Some(sdp::parse(data).map_err(bad)?),
        None => None,
    };
    // Without a source or group, any stream to the port would play.
    let connected = leg.source_ip.is_some() || leg.multicast_ip.is_some();
    // Of the same family as the stream, if it says.
    let family = leg.multicast_ip.or(leg.source_ip).map(|ip| ip.is_ipv4());
    let interface_ip = leg.interface_ip.value().or_else(|| {
        let ips = nmos::local_ips();
        let fits = |ip: &&IpAddr| family.is_none_or(|v4| ip.is_ipv4() == v4);
        ips.iter().find(fits).or(ips.first()).copied()
    });
    let resolved = |port| ReceiverParams {
        transport_params: vec![ReceiverTransport {
            interface_ip: Auto::Value(interface_ip.unwrap_or(IpAddr::from([0u16; 8]))),
            destination_port: Auto::Value(port),
            ..leg.clone()
        }],
        ..p.clone()
    };

    let runs =
        p.master_enable && leg.rtp_enabled && (connected || node.receiver.accept == Accept::Any);
    if !runs {
        engine.remove_receiver(id);
        return Ok(resolved(port));
    }
    // The audio system remembers the stream's volume and output by this.
    let from = stream
        .as_ref()
        .and_then(|s| s.node.clone())
        .or_else(|| leg.source_ip.or(leg.multicast_ip).map(|ip| ip.to_string()));
    let restore_key = from.map(|f| format!("orpheus.from.{f}"));
    // The playback stream is named after the sender.
    let name = stream
        .as_ref()
        .map(|s| s.name.clone())
        .filter(|n| connected && !n.is_empty() && n != "-");
    let listen = SocketAddr::from(([0u16; 8], port));
    let failed = |e: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("receiving on port {port}: {e}"),
        )
    };
    if engine.receiver_restore_key(id) != Some(restore_key.as_deref()) {
        engine
            .start_receiver(
                id,
                &ReceiverConfig {
                    name: format!("orpheus.receive.{id}"),
                    description: name.clone().unwrap_or_else(|| "Orpheus".into()),
                    format: FORMAT,
                    encoding: nmos::ENCODING,
                    listen,
                    link_offset_ms: node.receiver.link_offset_ms,
                    block_frames: node.block_frames,
                    gain: 1.0,
                    restore_key,
                },
            )
            .map_err(|e| failed(e.to_string()))?;
        tracing::info!(id, %listen, "receiver running");
    }
    engine
        .set_receiver_port(id, port)
        .map_err(|e| failed(e.to_string()))?;
    // Joined on that interface: without a default route, the system has
    // no choice to make.
    let group = leg.multicast_ip.map(|address| Group {
        address,
        interface: interface_ip,
    });
    engine
        .set_receiver_filter(
            id,
            Filter {
                enabled: true,
                source: leg.source_ip,
                payload_type: stream.as_ref().map(|s| s.payload_type),
                group,
            },
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("joining {group:?}: {e}"),
            )
        })?;
    engine.set_receiver_description(id, name.as_deref());
    Ok(resolved(port))
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
        "receivers" => conns
            .receivers
            .keys()
            .find(|&&n| receiver_uuid(node, n) == uuid)
            .map(|&n| PortId::Receiver(n)),
        _ => None,
    }
    .ok_or_else(not_found)
}

fn uuid_of(node: &Node, port: PortId) -> Uuid {
    match port {
        PortId::Sender(n) => resource_id(node.id, &format!("sender/{n}")),
        PortId::Receiver(n) => receiver_uuid(node, n),
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
        "receivers" => conns
            .receivers
            .keys()
            .map(|&n| PortId::Receiver(n))
            .collect(),
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
        PortId::Receiver(_) => json!(["constraints/", "staged/", "active/", "transporttype/"]),
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
        PortId::Receiver(_) => json!([{
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
        PortId::Receiver(n) => conns.receivers.get(&n).map(|p| json!(receiver(p))),
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
        Ok(PortId::Receiver(_)) => {
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
        PortId::Receiver(n) => {
            let p = conns
                .receivers
                .get_mut(&n)
                .ok_or((StatusCode::NOT_FOUND, "gone".into()))?;
            let staged = staged_patch(p, &body, |s| {
                let mut new = is05::patch(s, body.clone())?;
                from_transport_file(&mut new, &body)?;
                Ok(new)
            })?;
            check_receiver(&staged).map_err(bad)?;
            let reply = stage_or_activate(node, &mut conns.generation, p, port, staged)?;
            let mut engine = node.engine.as_ref().map(|e| e.lock().unwrap());
            if let Some(engine) = engine.as_deref_mut() {
                balance(node, conns, engine);
            }
            conns.save(engine.as_deref());
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
    fn activate(node: &Node, id: PortId, p: &Self) -> Result<Self, Fail> {
        let PortId::Receiver(n) = id else {
            unreachable!("receiver parameters for a sender")
        };
        apply_receiver(node, &mut *lock_engine(node)?, n, p)
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
        PortId::Receiver(n) => conns
            .receivers
            .get_mut(&n)
            .map(|p| fire_port(node, id, generation, p)),
    };
    match result {
        Some(Ok(true)) => {
            let mut engine = node.engine.as_ref().map(|e| e.lock().unwrap());
            if let Some(engine) = engine.as_deref_mut() {
                balance(node, &mut conns, engine);
            }
            conns.save(engine.as_deref());
        }
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
