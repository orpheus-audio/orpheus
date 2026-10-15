//! The IS-05 Connection API (`/x-nmos/connection/v1.1/…`): how the senders
//! and the receiver are routed.
//!
//! Each port has staged parameters, which controllers patch, and active
//! ones, which the engine runs. Staged parameters become active at once or
//! at a scheduled time. `POST /x-orpheus/v1/routes` creates a sender and
//! activates its destination the same way; unless asked to only send, it
//! also connects a free receiver on the peer to it, through the peer's
//! IS-05 API, and disconnects it again when the sender goes.
//!
//! A sender serves a route, and is named by the route's ID: its sink, its
//! NMOS resources and the source its SDP file announces. A disconnected
//! route is remembered (`[routes]` limits how long), and connecting the
//! same peer with the same priority again brings it back under its ID, so
//! that applications find the same sink and receivers the same source.
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
use orpheus_core::{Layout, OutputType};
use orpheus_engine::{
    Engine, Filter, Group, ReceiverConfig, ReceiverId, Route, SenderConfig, SenderId, Volume,
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
use crate::config::{Accept, Claim, RTP_PORT, RoutesConfig};
use crate::nmos::FORMAT;
use crate::nmos::{self, error};
use crate::outputs;
use crate::peer::{self, PeerReceiver};
use crate::peers::Peers;
use crate::policy::State as PolicyState;

mod claim;

pub use claim::run as claim_on_play;

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
    /// The routes remembered while they don't run, most recent first.
    cached: Vec<Cached>,
    limits: RoutesConfig,
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
    #[serde(default)]
    cached: Vec<Cached>,
    receivers: BTreeMap<ReceiverId, ReceiverParams>,
}

/// A route remembered while it doesn't run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cached {
    pub id: SenderId,
    /// Name of the sink.
    pub name: String,
    /// The user named the sink; otherwise it follows the peer's name.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    named: bool,
    #[serde(default)]
    pub priority: Priority,
    /// The peer's API; absent if the route only sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<String>,
    /// The peer's Orpheus device ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Where it sent to.
    pub peer: SocketAddr,
    /// When it last ran, in seconds since the Unix epoch.
    pub used: u64,
    /// The sink's volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<Volume>,
}

impl Cached {
    /// Whether it is the route to `peer`'s receivers with `priority`.
    fn connects(&self, peer: &peer::Peer, priority: Priority) -> bool {
        self.priority == priority
            && (self.api.as_deref() == Some(&peer.api)
                || (self.api.is_some() && self.node.is_some() && self.node == peer.node))
    }

    /// Whether it is the route that only sends to `peer` with `priority`.
    fn sends(&self, peer: SocketAddr, priority: Priority) -> bool {
        self.priority == priority && self.api.is_none() && self.peer == peer
    }
}

/// Seconds since the Unix epoch.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
    /// Another sender took the peer's receiver, and this one has not
    /// played since: it moves to a free receiver as soon as there is one.
    #[serde(skip)]
    displaced: bool,
    /// The kind of output the peer's receiver announces, which the sink
    /// shows unless overridden here.
    #[serde(default)]
    output: OutputType,
    /// The peer's receiver we connected, to disconnect it when the sender
    /// goes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    receiver: Option<PeerReceiver>,
    /// The sink's volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    volume: Option<Volume>,
    /// When this node last claimed or let go of the peer's receiver, or
    /// started waiting for one; what the peer said before that is out of
    /// date.
    #[serde(skip)]
    since: Option<Instant>,
    /// Since when the peer has been unreachable.
    #[serde(skip)]
    unreachable_since: Option<Instant>,
    /// The peer's receiver change counter (`ver_rcv`) when none of its
    /// receivers fit, if discovery knew it.
    #[serde(skip)]
    unfit_at: Option<u8>,
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
    /// With `[sender] claim = "on_play"`: nothing has played into the sink
    /// for a while, and the peer's receiver was let go. The sender pauses
    /// until audio plays again, then claims a free receiver.
    Idle,
    /// The sender wants a receiver, but the peer has no free one: another
    /// sender took its receiver, or all were taken when audio started. It
    /// pauses and tries again every few seconds.
    Busy,
    /// None of the peer's receivers can play the stream (their
    /// capabilities don't fit). It pauses and tries again when the peer's
    /// receivers change.
    Incompatible,
    /// The peer has been unreachable for a while and isn't pinned: the
    /// sink is gone until the peer is back, so that applications play
    /// elsewhere meanwhile.
    Offline,
}

impl PeerState {
    /// Whether the sender holds its packets back in this state.
    fn pauses(self) -> bool {
        matches!(
            self,
            Self::Disconnected | Self::Idle | Self::Busy | Self::Incompatible
        )
    }
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
    pub fn new(path: PathBuf, limits: RoutesConfig) -> Self {
        Self {
            senders: BTreeMap::new(),
            receivers: BTreeMap::new(),
            free_since: BTreeMap::new(),
            links: BTreeMap::new(),
            cached: Vec::new(),
            limits,
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

    /// A random route ID that no running or remembered route has.
    fn new_id(&self) -> SenderId {
        loop {
            let id = SenderId::generate().expect("OS random source");
            if !self.senders.contains_key(&id) && self.cached.iter().all(|c| c.id != id) {
                return id;
            }
        }
    }

    /// The routes remembered while they don't run, most recent first.
    pub fn cached(&self) -> &[Cached] {
        &self.cached
    }

    /// Forgets cached routes beyond the limits, oldest first; routes to
    /// pinned nodes stay.
    fn evict(&mut self, peers: &Peers) {
        let max_age = u64::from(self.limits.max_age_days) * 86_400;
        let max_entries = self.limits.max_entries as usize;
        let now = unix_now();
        let pinned = |c: &Cached| c.node.as_deref().is_some_and(|n| peers.pinned(n));
        self.cached.sort_by_key(|c| std::cmp::Reverse(c.used));
        let mut kept = 0;
        self.cached.retain(|c| {
            if pinned(c) {
                return true;
            }
            kept += 1;
            kept <= max_entries && now.saturating_sub(c.used) <= max_age
        });
    }

    /// The senders routed here.
    pub fn sender_ids(&self) -> Vec<SenderId> {
        self.senders.keys().copied().collect()
    }

    /// The Orpheus device ID of the peer sender `id` is connected to.
    pub fn sender_node(&self, id: SenderId) -> Option<String> {
        self.links.get(&id)?.receiver.as_ref()?.node.clone()
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

    /// Who receiver `id` is connected to: this node's alias for the
    /// sending node, else the name in the sender's SDP file, else its
    /// address; `None` if no sender is connected.
    pub fn receiver_sender(&self, id: ReceiverId, peers: &Peers) -> Option<String> {
        let active = self.receiver_active(id).filter(|a| a.master_enable)?;
        let leg = active.transport_params.first()?;
        let stream = active
            .transport_file
            .data
            .as_deref()
            .and_then(|d| sdp::parse(d).ok());
        stream
            .as_ref()
            .and_then(|s| stream_name(s, peers))
            .or_else(|| leg.source_ip.or(leg.multicast_ip).map(|ip| ip.to_string()))
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
            cached: self.cached.clone(),
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

/// The name a received stream is shown by: this node's alias for the
/// sending node, else the name in its SDP file, if it has one.
fn stream_name(s: &sdp::Stream, peers: &Peers) -> Option<String> {
    let alias = s.node.as_deref().and_then(|n| peers.alias(n));
    alias
        .map(str::to_owned)
        .or_else(|| Some(s.name.clone()).filter(|n| !n.is_empty() && n != "-"))
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
    conns.cached = saved.cached;
    conns.evict(&node.peers.lock().unwrap());
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
                &sender_config(
                    node,
                    s.id,
                    s.name,
                    peer,
                    sink_type(node, s.id, &s.link),
                    s.link.volume.clone(),
                ),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
            .and_then(|()| apply_sender(&mut engine, s.id, &s.requested));
        match started {
            Ok(active) => {
                tracing::info!(id = %s.id, %peer, "sender restored");
                conns.senders.insert(s.id, Port::new(s.requested, active));
                conns.links.insert(s.id, s.link);
            }
            Err((_, e)) => {
                engine.remove_sender(s.id);
                tracing::warn!(id = %s.id, %peer, error = %e, "restoring sender");
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
    volume: Option<Volume>,
) -> SenderConfig {
    SenderConfig {
        name: outputs::sender_sink(id),
        description: name,
        format: nmos::FORMAT,
        encoding: nmos::ENCODING,
        peer,
        remote,
        block_frames: node.block_frames,
        volume,
        fade_in_ms: node.settings().sender.fade_in_ms,
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
/// telling the peer. A route that only sent there with that priority
/// before comes back under its ID.
pub fn add_sender(
    node: &Node,
    peer: SocketAddr,
    name: String,
    priority: Priority,
) -> Result<SenderId, Fail> {
    let (id, volume) = {
        let conns = node.connections.lock().unwrap();
        conns
            .cached
            .iter()
            .find(|c| c.sends(peer, priority))
            .map_or_else(|| (conns.new_id(), None), |c| (c.id, c.volume.clone()))
    };
    let link = Link {
        named: true,
        priority,
        volume,
        ..Link::default()
    };
    start_sender(node, id, peer, name, link)
}

/// Connects this node to a peer, both ends: picks a free receiver there,
/// starts a sender to it and connects the receiver to the sender. If the
/// receiver refuses, the sender goes again. A second connection to the
/// same peer is refused unless it has another priority. A route to the
/// peer with that priority remembered from before comes back under its ID,
/// and with its name unless another is given.
pub fn connect(
    node: &Node,
    peer: &str,
    name: Option<String>,
    force: bool,
    priority: Priority,
) -> Result<Connected, Fail> {
    let found = peer::resolve(node, peer)?;
    let shown = found.name.clone().unwrap_or_else(|| peer.to_owned());
    let (ours, cached, id) = {
        let conns = node.connections.lock().unwrap();
        let same = conns.links.iter().find(|(_, l)| {
            l.priority == priority && l.receiver.as_ref().is_some_and(|r| r.on(&found))
        });
        if let Some((n, _)) = same {
            return Err((
                StatusCode::CONFLICT,
                format!("already connected to {shown} as route {n}"),
            ));
        }
        let ours: Vec<Uuid> = conns
            .senders
            .keys()
            .map(|&n| uuid_of(node, PortId::Sender(n)))
            .collect();
        let cached = conns
            .cached
            .iter()
            .find(|c| c.connects(&found, priority))
            .cloned();
        let id = cached.as_ref().map_or_else(|| conns.new_id(), |c| c.id);
        (ours, cached, id)
    };
    let target = peer::find_receiver(&found, &ours, force)?;
    let (named, name) = match (name, &cached) {
        (Some(n), _) => (true, n),
        (None, Some(c)) if c.named => (true, c.name.clone()),
        (None, _) => (false, shown),
    };
    let to = PeerReceiver {
        api: found.api.clone(),
        id: target.receiver,
        node: found.node.clone(),
    };
    let link = Link {
        named,
        priority,
        state: PeerState::Connecting,
        jitter: false,
        held: None,
        displaced: false,
        output: target.output,
        receiver: Some(to.clone()),
        volume: cached.as_ref().and_then(|c| c.volume.clone()),
        since: Some(Instant::now()),
        unreachable_since: None,
        unfit_at: None,
    };
    start_sender(
        node,
        id,
        SocketAddr::new(found.ip, target.port),
        name.clone(),
        link,
    )?;
    let attached = attach(node, id, &to).and_then(|took| {
        if took {
            return Ok(target);
        }
        // Another sender connected the receiver at the same moment.
        tracing::info!(
            %id,
            "another sender took the peer's receiver; trying another"
        );
        let again = peer::find_receiver(&found, &ours, false)?;
        let to = PeerReceiver {
            id: again.receiver,
            ..to.clone()
        };
        retarget(node, id, to.clone(), SocketAddr::new(found.ip, again.port))?;
        match attach(node, id, &to)? {
            true => Ok(again),
            false => Err((
                StatusCode::CONFLICT,
                format!("other senders took {name}'s free receivers meanwhile; try again"),
            )),
        }
    });
    let target = match attached {
        Ok(t) => t,
        Err(e) => {
            stop_sender(node, id, cached.is_some())?;
            return Err(e);
        }
    };
    if let Some(link) = node.connections.lock().unwrap().links.get_mut(&id) {
        link.state = PeerState::Connected;
    }
    Ok(Connected {
        id,
        name,
        destination: SocketAddr::new(found.ip, target.port),
        receiver: target.receiver,
    })
}

/// Connects the peer's receiver `to` to sender `id`, then asks it whether
/// it took: another sender may have connected it at the same moment. A
/// receiver that cannot be asked counts as connected; the peer watch asks
/// again.
fn attach(node: &Node, id: SenderId, to: &PeerReceiver) -> Result<bool, Fail> {
    let uuid = uuid_of(node, PortId::Sender(id));
    let sdp =
        nmos::sdp(node, id).ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no SDP file".into()))?;
    peer::connect(to, uuid, &sdp)?;
    Ok(peer::playing(to).map_or(true, |p| p.sender == Some(uuid)))
}

/// Points sender `id` at the peer's receiver `to` at `destination`, until
/// [`attach`] connects that receiver.
fn retarget(
    node: &Node,
    id: SenderId,
    to: PeerReceiver,
    destination: SocketAddr,
) -> Result<(), Fail> {
    let mut conns = node.connections.lock().unwrap();
    let conns = &mut *conns;
    let mut engine = lock_engine(node)?;
    let port = conns
        .senders
        .get_mut(&id)
        .ok_or((StatusCode::NOT_FOUND, format!("no sender {id}")))?;
    port.cancel();
    if let Err(e) = engine.show_sender(id) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    let mut params = port.requested.clone();
    params.receiver_id = Some(to.id);
    let leg = &mut params.transport_params[0];
    leg.destination_ip = Auto::Value(destination.ip());
    leg.destination_port = Auto::Value(destination.port());
    let mut active = apply_sender(&mut engine, id, &params)?;
    active.activation = Activation {
        mode: Some(Mode::ActivateImmediate),
        requested_time: None,
        activation_time: Some(nmos::now()),
    };
    port.active = active;
    port.staged = params.clone();
    port.requested = params;
    touch(node, PortId::Sender(id));
    if let Some(link) = conns.links.get_mut(&id) {
        link.receiver = Some(to);
        link.state = PeerState::Connecting;
        link.jitter = false;
        link.held = None;
        link.unreachable_since = None;
    }
    conns.save(Some(&engine));
    Ok(())
}

/// Connects sender `id` to the peer's receiver `to` at `destination`
/// instead of the one it had, and resumes it. If that fails, the sender
/// pauses and waits for a free receiver ([`PeerState::Busy`]).
fn move_to(node: &Node, id: SenderId, to: PeerReceiver, destination: SocketAddr) -> bool {
    let moved = retarget(node, id, to.clone(), destination).and_then(|()| attach(node, id, &to));
    let ok = matches!(moved, Ok(true));
    if let Err((_, e)) = &moved {
        tracing::warn!(%id, error = %e, "connecting the peer's receiver");
    }
    if let Some(link) = node.connections.lock().unwrap().links.get_mut(&id) {
        link.displaced &= !ok;
        link.since = Some(Instant::now());
        link.state = if ok {
            PeerState::Connected
        } else {
            PeerState::Busy
        };
    }
    if let Some(engine) = &node.engine {
        engine.lock().unwrap().pause_sender(id, !ok);
    }
    ok
}

/// Starts the sender of route `id`, taking it from the cache if it was
/// there.
fn start_sender(
    node: &Node,
    id: SenderId,
    peer: SocketAddr,
    name: String,
    link: Link,
) -> Result<SenderId, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    let remote = sink_type(node, id, &link);
    engine
        .insert_sender(
            id,
            &sender_config(node, id, name, peer, remote, link.volume.clone()),
        )
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
    conns.cached.retain(|c| c.id != id);
    conns.save(Some(&engine));
    Ok(id)
}

/// Disconnects route `id`: stops its sender and disconnects the peer's
/// receiver we connected to it, even if the peer cannot be reached. The
/// route is remembered, unless `forget`, which also drops a route that
/// doesn't run. False if there is no such route.
pub fn remove_sender(node: &Node, id: SenderId, forget: bool) -> Result<bool, Fail> {
    let uuid = uuid_of(node, PortId::Sender(id));
    let link = node.connections.lock().unwrap().links.get(&id).cloned();
    let removed = stop_sender(node, id, !forget)?;
    if let Some(to) = link.and_then(|l| l.receiver)
        && let Err((_, e)) = peer::release(&to, uuid)
    {
        tracing::warn!(%id, error = %e, "disconnecting the peer's receiver");
    }
    let mut conns = node.connections.lock().unwrap();
    let cached = conns.cached.len();
    if forget {
        conns.cached.retain(|c| c.id != id);
    }
    let dropped = conns.cached.len() < cached;
    if dropped {
        conns.save(node.engine.as_ref().map(|e| e.lock().unwrap()).as_deref());
    }
    Ok(removed || dropped)
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
/// doesn't see the peer, and how often a receiver's sender is asked whether
/// it still sends.
const RECHECK: Duration = Duration::from_secs(30);

/// What the peer watch remembers between rounds.
#[derive(Default)]
struct Watch {
    /// What it last saw of each sender's peer.
    peers: BTreeMap<SenderId, Seen>,
    /// When each receiver's sender was last asked.
    sources: BTreeMap<ReceiverId, Instant>,
}

/// What the peer watch last saw of a sender's peer.
#[derive(Default)]
struct Seen {
    /// The peer as discovery announced it: online, name, receiver counter.
    announced: Option<(bool, String, Option<u8>)>,
    asked: Option<Instant>,
}

/// Follows the peers' receivers the senders are connected to: their output
/// type goes to the sender's sink, their name too unless the user named the
/// sink, and a sender pauses while its receiver plays something else. A
/// sender whose receiver another sender took moves to a free receiver of
/// the peer if it has one, and a sender follows its peer to a new address.
///
/// Each peer announces a receiver change counter over mDNS; it is asked
/// again when that, its name or its presence changes, and every 30 seconds
/// anyway, as mDNS may miss an update and doesn't see every peer.
///
/// The other way round, receivers connected to another Orpheus node's
/// sender are freed once that sender is gone or sends elsewhere: its node
/// may have dropped it while this node was offline.
pub async fn watch_peers(node: Arc<Node>) {
    let mut watch = Watch::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let node = node.clone();
        match tokio::task::spawn_blocking(move || {
            check_peers(&node, &mut watch);
            watch
        })
        .await
        {
            Ok(w) => watch = w,
            Err(_) => watch = Watch::default(),
        }
    }
}

/// Applies changed receiver settings (`accept`, `link_offset_ms`) to every
/// receiver: starts or stops free ones and restarts running streams with
/// the new link offset.
pub fn reapply_receivers(node: &Node) {
    let Some(engine) = &node.engine else {
        return;
    };
    let conns = node.connections.lock().unwrap();
    let mut engine = engine.lock().unwrap();
    for (&id, port) in &conns.receivers {
        if let Err((_, e)) = apply_receiver(node, &mut engine, id, &port.active) {
            tracing::warn!(id, error = %e, "applying the settings to a receiver");
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

fn check_peers(node: &Node, watch: &mut Watch) {
    tend_receivers(node);
    let nodes = node
        .discovery
        .as_ref()
        .map(|d| d.nodes())
        .unwrap_or_default();
    node.peers.lock().unwrap().remember(&nodes);
    check_sources(node, &mut watch.sources, &nodes);
    let links: Vec<(SenderId, PeerReceiver)> = {
        let conns = node.connections.lock().unwrap();
        watch.peers.retain(|id, _| conns.links.contains_key(id));
        conns
            .links
            .iter()
            .filter(|(_, l)| l.state != PeerState::Connecting)
            .filter_map(|(&id, l)| Some((id, l.receiver.clone()?)))
            .collect()
    };
    for (id, to) in links {
        let by_id = to.node.as_ref().and_then(|dev| {
            nodes
                .iter()
                .find(|n| n.online && n.id.as_ref() == Some(dev))
        });
        let found = by_id.or_else(|| nodes.iter().find(|n| peer::serves(n, &to.api)));
        if let Some(n) = by_id.filter(|n| !peer::serves(n, &to.api))
            && let Some((ip, api)) = peer::address(n)
            && moved(destination_of(node, id).map(|d| d.ip()), ip, || {
                peer::playing(&to).is_ok()
            })
        {
            follow(node, id, PeerReceiver { api, ..to }, ip, &n.name);
            continue;
        }
        let announced = found.map(|n| (n.online, n.name.clone(), n.receivers));
        let last = watch.peers.entry(id).or_default();
        let due = announced != last.announced || last.asked.is_none_or(|t| t.elapsed() >= RECHECK);
        if !due {
            continue;
        }
        last.announced = announced;
        last.asked = Some(Instant::now());
        if source_moved(node, id) {
            tracing::info!(%id, "this node's address changed; telling the peer");
            if let Some(dest) = destination_of(node, id) {
                move_to(node, id, to, dest);
            }
            continue;
        }
        let asked = Instant::now();
        let playing = match found {
            Some(n) if !n.online => {
                Err((StatusCode::BAD_GATEWAY, format!("{} is offline", n.name)))
            }
            _ => peer::playing(&to),
        };
        let shown = found.map(|n| shown_name(node, n));
        if update_link(node, id, &to, asked, playing, shown.as_deref()) {
            tracing::info!(%id, "the peer's receiver still plays the idle sender; letting it go");
            claim::release(node, id, &to);
        }
    }
}

/// The name a node is shown by here: the alias, else its announced name.
fn shown_name(node: &Node, n: &orpheus_discovery::Node) -> String {
    n.id.as_deref()
        .and_then(|id| node.peers.lock().unwrap().alias(id).map(str::to_owned))
        .unwrap_or_else(|| n.name.clone())
}

/// Shows the node with device ID `dev` by the name it has now, after its
/// alias changed: in the sinks to it that the user didn't name, and in the
/// streams received from it.
pub fn rename_peer(node: &Node, dev: &str) {
    let Some(engine) = &node.engine else {
        return;
    };
    let announced = node.discovery.as_ref().and_then(|d| {
        d.nodes()
            .into_iter()
            .find(|n| n.id.as_deref() == Some(dev))
            .map(|n| n.name)
    });
    let conns = node.connections.lock().unwrap();
    let mut engine = engine.lock().unwrap();
    let peers = node.peers.lock().unwrap();
    let name = peers.alias(dev).map(str::to_owned).or(announced);
    let mut renamed = false;
    if let Some(name) = &name {
        for (&id, link) in &conns.links {
            let to_dev = link.receiver.as_ref().and_then(|r| r.node.as_deref()) == Some(dev);
            if to_dev && !link.named {
                tracing::info!(%id, %name, "renaming the sink to the peer");
                engine.rename_sender(id, name);
                renamed = true;
            }
        }
    }
    for (&id, port) in &conns.receivers {
        let Some(stream) = port
            .active
            .transport_file
            .data
            .as_deref()
            .and_then(|d| sdp::parse(d).ok())
            .filter(|s| s.node.as_deref() == Some(dev))
        else {
            continue;
        };
        engine.set_receiver_description(id, stream_name(&stream, &peers).as_deref());
    }
    drop(peers);
    if renamed {
        conns.save(Some(&engine));
    }
}

/// Where sender `id` sends to.
fn destination_of(node: &Node, id: SenderId) -> Option<SocketAddr> {
    let engine = node.engine.as_ref()?.lock().unwrap();
    let (_, s) = engine
        .status()
        .senders
        .into_iter()
        .find(|(n, _)| *n == id)?;
    Some(s.peer)
}

/// Whether sender `id` would now send from another address than the one
/// its peer's receiver expects.
fn source_moved(node: &Node, id: SenderId) -> bool {
    let (requested, active) = {
        let conns = node.connections.lock().unwrap();
        let Some(p) = conns.senders.get(&id) else {
            return false;
        };
        let leg = |p: &SenderParams| p.transport_params.first().map(|l| l.source_ip);
        (leg(&p.requested), leg(&p.active))
    };
    let Some(Auto::Value(was)) = active.filter(|_| requested == Some(Auto::Auto)) else {
        return false;
    };
    destination_of(node, id)
        .and_then(nmos::route)
        .is_some_and(|(now, _)| now != was)
}

/// Follows sender `id`'s peer to its new address.
/// Whether a peer that no longer announces the address `current` it is
/// reached at moved to `announced`. mDNS may first announce only some of a
/// node's addresses, IPv6 before IPv4, so a less preferred address counts
/// only once the peer stops answering at the current one.
fn moved(current: Option<IpAddr>, announced: IpAddr, answers: impl FnOnce() -> bool) -> bool {
    let rank = orpheus_discovery::address_rank;
    current.is_none_or(|c| rank(&announced) <= rank(&c)) || !answers()
}

fn follow(node: &Node, id: SenderId, to: PeerReceiver, ip: IpAddr, name: &str) {
    let Some(mut dest) = destination_of(node, id) else {
        return;
    };
    tracing::info!(%id, %ip, "{name} moved to another address; following it");
    dest = SocketAddr::new(ip, dest.port());
    move_to(node, id, to, dest);
}

/// Frees receivers whose Orpheus sender is gone or sends elsewhere, asking
/// each sender's node every [`RECHECK`]. A node that cannot be reached
/// keeps its receivers: it may be back soon.
fn check_sources(
    node: &Node,
    asked: &mut BTreeMap<ReceiverId, Instant>,
    nodes: &[orpheus_discovery::Node],
) {
    let connected: Vec<(ReceiverId, Uuid, String, Option<IpAddr>)> = {
        let conns = node.connections.lock().unwrap();
        conns
            .receivers
            .iter()
            .filter(|(_, p)| p.active.master_enable && p.pending.is_none())
            .filter_map(|(&id, p)| {
                let sender = p.active.sender_id?;
                let from = sdp::parse(p.active.transport_file.data.as_deref()?)
                    .ok()?
                    .node?;
                let source = p.active.transport_params.first()?.source_ip;
                Some((id, sender, from, source))
            })
            .collect()
    };
    asked.retain(|id, _| connected.iter().any(|c| c.0 == *id));
    for (id, sender, from, source) in connected {
        if asked.get(&id).is_some_and(|t| t.elapsed() < RECHECK) {
            continue;
        }
        asked.insert(id, Instant::now());
        let Ok(dev) = from.parse() else {
            continue;
        };
        let api = nodes
            .iter()
            .find(|n| n.online && n.id.as_deref() == Some(&from))
            .and_then(peer::address)
            .map(|(_, api)| api)
            .or_else(|| source.map(peer::api_at));
        let Some(api) = api else {
            continue;
        };
        match peer::still_sends(&api, dev, sender, receiver_uuid(node, id)) {
            Ok(true) => {}
            Ok(false) => free_receiver(node, id, sender),
            Err((_, e)) => tracing::debug!(id, error = %e, "asking the receiver's sender"),
        }
    }
}

/// Disconnects receiver `id` from `sender`, unless something else
/// connected it meanwhile.
fn free_receiver(node: &Node, id: ReceiverId, sender: Uuid) {
    let Some(engine) = &node.engine else {
        return;
    };
    let mut conns = node.connections.lock().unwrap();
    let conns = &mut *conns;
    let mut engine = engine.lock().unwrap();
    let Some(port) = conns
        .receivers
        .get_mut(&id)
        .filter(|p| p.active.sender_id == Some(sender) && p.pending.is_none())
    else {
        return;
    };
    let params = free_params();
    match apply_receiver(node, &mut engine, id, &params) {
        Ok(mut active) => {
            tracing::info!(
                id,
                "the receiver's sender is gone or sends elsewhere; freeing it"
            );
            active.activation = Activation {
                mode: Some(Mode::ActivateImmediate),
                requested_time: None,
                activation_time: Some(nmos::now()),
            };
            port.active = active;
            port.staged = params.clone();
            port.requested = params;
            touch(node, PortId::Receiver(id));
            balance(node, conns, &mut engine);
            conns.save(Some(&engine));
        }
        Err((_, e)) => tracing::warn!(id, error = %e, "freeing a receiver"),
    }
}

/// How long a peer may be unreachable before the sink to it goes, unless
/// the peer is pinned.
const OFFLINE_AFTER: Duration = Duration::from_secs(60);

/// Takes in what a peer's receiver said at `asked`, or that it couldn't be
/// asked. True if the receiver still plays the sender although it is idle,
/// so that it should be let go.
fn update_link(
    node: &Node,
    id: SenderId,
    to: &PeerReceiver,
    asked: Instant,
    playing: Result<peer::Playing, Fail>,
    name: Option<&str>,
) -> bool {
    let ours = uuid_of(node, PortId::Sender(id));
    let on_play = node.settings().sender.claim == Claim::OnPlay;
    let mut conns = node.connections.lock().unwrap();
    let Some(link) = conns.links.get_mut(&id).filter(|l| {
        l.receiver.as_ref() == Some(to)
            && l.state != PeerState::Connecting
            && l.since.is_none_or(|t| t < asked)
    }) else {
        return false;
    };
    let Some(engine) = &node.engine else {
        return false;
    };
    let mut engine = engine.lock().unwrap();
    let was = link.state;
    let mut changed = false;
    let mut stale = false;
    match playing {
        Err((_, e)) => {
            if !matches!(was, PeerState::Unreachable | PeerState::Offline) {
                tracing::warn!(%id, error = %e, "peer unreachable");
                link.state = PeerState::Unreachable;
            }
            link.jitter = false;
            link.held = None;
            let since = *link.unreachable_since.get_or_insert(asked);
            let pinned = to
                .node
                .as_deref()
                .is_some_and(|n| node.peers.lock().unwrap().pinned(n));
            if link.state == PeerState::Unreachable && !pinned && since.elapsed() >= OFFLINE_AFTER {
                tracing::info!(%id, "peer offline; removing its sink until it is back");
                engine.hide_sender(id);
                link.state = PeerState::Offline;
            } else if link.state == PeerState::Offline && pinned {
                show(&mut engine, id);
                link.state = PeerState::Unreachable;
            }
        }
        Ok(p) => {
            link.unreachable_since = None;
            if engine.sender_hidden(id) {
                tracing::info!(%id, "peer back; restoring its sink");
                show(&mut engine, id);
            }
            link.jitter = p.jitter;
            if p.held != link.held {
                tracing::info!(%id, held = ?p.held, "the peer's policy changed what it does to the stream");
                link.held = p.held;
            }
            // A receiver that plays nothing was released on purpose, unless
            // the sender was displaced before.
            match p.sender {
                Some(s) if s == ours => link.displaced = false,
                Some(_) => link.displaced = true,
                None => {}
            }
            link.state = if p.sender == Some(ours) {
                stale = was == PeerState::Idle;
                if stale { was } else { PeerState::Connected }
            } else if matches!(
                was,
                PeerState::Idle | PeerState::Busy | PeerState::Incompatible
            ) {
                was
            } else if link.displaced {
                link.since = None;
                PeerState::Busy
            } else if on_play {
                PeerState::Idle
            } else {
                PeerState::Disconnected
            };
            if p.output != link.output {
                tracing::info!(%id, output = %p.output, "peer's output changed");
                link.output = p.output;
                changed = true;
            }
        }
    }
    let state = link.state;
    let rename = name.filter(|_| !link.named).map(str::to_owned);
    let remote = sink_type(node, id, link);
    if state != was {
        match state {
            PeerState::Disconnected => {
                tracing::warn!(
                    %id,
                    "the peer's receiver plays another sender or none; pausing"
                )
            }
            PeerState::Busy => {
                tracing::warn!(
                    %id,
                    "another sender took the peer's receiver; waiting for a free one"
                )
            }
            PeerState::Idle => {
                tracing::info!(%id, "the peer's receiver plays another sender or none; idle")
            }
            PeerState::Connected if was == PeerState::Disconnected => {
                tracing::info!(%id, "the peer's receiver plays the sender again; resuming")
            }
            PeerState::Connected if matches!(was, PeerState::Unreachable | PeerState::Offline) => {
                tracing::info!(%id, "peer reachable again")
            }
            _ => {}
        }
        // Unreachable, it goes on as it was.
        if !matches!(state, PeerState::Unreachable | PeerState::Offline) {
            engine.pause_sender(id, state.pauses());
        }
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
        tracing::info!(%id, %name, "peer renamed; renaming its sink");
        engine.rename_sender(id, &name);
        changed = true;
    }
    if changed {
        conns.save(Some(&engine));
    }
    stale
}

/// Brings back the sink of sender `id`, hidden while its peer was offline.
fn show(engine: &mut Engine, id: SenderId) {
    if let Err(e) = engine.show_sender(id) {
        tracing::warn!(%id, error = %e, "restoring the sink");
    }
}

/// Stops a sender, here only; with `keep`, its route is remembered.
fn stop_sender(node: &Node, id: SenderId, keep: bool) -> Result<bool, Fail> {
    let mut conns = node.connections.lock().unwrap();
    let mut engine = lock_engine(node)?;
    let port = conns.senders.remove(&id);
    let link = conns.links.remove(&id);
    let name = engine
        .status()
        .senders
        .into_iter()
        .find(|(n, _)| *n == id)
        .map(|(_, s)| s.name);
    if let Some(mut port) = port {
        port.cancel();
        if keep
            && let (Some(link), Some(name), Some(peer)) = (link, name, destination(&port.active))
        {
            conns.cached.retain(|c| c.id != id);
            conns.cached.insert(
                0,
                Cached {
                    id,
                    name,
                    named: link.named,
                    priority: link.priority,
                    api: link.receiver.as_ref().map(|r| r.api.clone()),
                    node: link.receiver.and_then(|r| r.node),
                    peer,
                    used: unix_now(),
                    volume: engine.sender_volume(id).or(link.volume),
                },
            );
            conns.evict(&node.peers.lock().unwrap());
        }
    }
    let removed = engine.remove_sender(id);
    conns.save(Some(&engine));
    Ok(removed)
}

/// Notes the volume of each running sender's sink, as the user set it.
pub fn follow_sinks(node: &Node) {
    let mut conns = node.connections.lock().unwrap();
    let Ok(engine) = lock_engine(node) else {
        return;
    };
    let mut changed = false;
    for (&id, link) in &mut conns.links {
        if let Some(v) = engine.sender_volume(id)
            && !link.volume.as_ref().is_some_and(|old| old.same(&v))
        {
            link.volume = Some(v);
            changed = true;
        }
    }
    if changed {
        conns.save(Some(&engine));
    }
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

/// The machine name of receiver `id`'s stream in the audio system.
pub fn receiver_stream(id: ReceiverId) -> String {
    format!("orpheus.receive.{id}")
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

    let settings = node.settings().receiver;
    let runs = p.master_enable && leg.rtp_enabled && (connected || settings.accept == Accept::Any);
    if !runs {
        engine.remove_receiver(id);
        return Ok(resolved(port));
    }
    // What is sent, as the sender names it, else the session, else where
    // it comes from: the receivers remember its output and volume by this.
    let key = stream
        .as_ref()
        .and_then(|s| {
            s.source_id
                .as_ref()
                .map(|id| format!("orpheus.source.{id}"))
                .or_else(|| s.session.as_ref().map(|o| format!("orpheus.session.{o}")))
        })
        .or_else(|| {
            leg.source_ip
                .or(leg.multicast_ip)
                .map(|ip| format!("orpheus.from.{ip}"))
        });
    // The playback stream is named after the sender.
    let name = stream
        .as_ref()
        .filter(|_| connected)
        .and_then(|s| stream_name(s, &node.peers.lock().unwrap()));
    let listen = SocketAddr::from(([0u16; 8], port));
    let failed = |e: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("receiving on port {port}: {e}"),
        )
    };
    let layout = stream_layout(node, key.as_deref(), stream.as_ref());
    let started = engine.receiver_key(id) == Some(key.as_deref())
        && engine.receiver_link_offset(id) == Some(settings.link_offset_ms)
        && engine.receiver_layout(id) == Some(&layout);
    if !started {
        let remembered = key.as_deref().map(|k| {
            let shown = name
                .clone()
                .or_else(|| leg.source_ip.or(leg.multicast_ip).map(|ip| ip.to_string()))
                .unwrap_or_default();
            let from = stream.as_ref().and_then(|s| s.node.as_deref());
            node.sources.lock().unwrap().played(k, &shown, from)
        });
        let output = remembered.as_ref().and_then(|s| s.output.as_deref());
        let placement = crate::sources::resolve(
            output,
            settings.default_output.as_deref(),
            &engine.outputs(),
        );
        engine
            .start_receiver(
                id,
                &ReceiverConfig {
                    name: receiver_stream(id),
                    description: name.clone().unwrap_or_else(|| "Orpheus".into()),
                    format: FORMAT,
                    layout,
                    encoding: nmos::ENCODING,
                    listen,
                    link_offset_ms: settings.link_offset_ms,
                    block_frames: node.block_frames,
                    gain: 1.0,
                    key,
                    placement: placement.clone(),
                    volume: remembered.and_then(|s| s.volume),
                },
            )
            .map_err(|e| failed(e.to_string()))?;
        node.sources.lock().unwrap().place(id, placement);
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

/// What the channels of a stream carry: as set for its source, else as
/// its SDP file says, else what its channel count suggests.
fn stream_layout(node: &Node, key: Option<&str>, stream: Option<&sdp::Stream>) -> Layout {
    let channels = usize::from(FORMAT.channels);
    key.and_then(|k| node.sources.lock().unwrap().get(k)?.layout.clone())
        .filter(|l| l.len() == channels)
        .or_else(|| stream.and_then(|s| s.layout.clone()))
        .unwrap_or_else(|| Layout::default_for(channels))
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

    fn cached(n: u64, node: Option<&str>, used: u64) -> Cached {
        Cached {
            id: SenderId::from_bits(n),
            name: format!("route {n}"),
            named: false,
            priority: Priority::Normal,
            api: Some(format!("http://10.0.0.{n}:7480")),
            node: node.map(Into::into),
            peer: "10.0.0.2:5004".parse().unwrap(),
            used,
            volume: None,
        }
    }

    #[test]
    fn remembered_routes_match_peer_and_priority() {
        let c = cached(1, Some("NODE1"), 0);
        let peer = |api: &str, node: Option<&str>| peer::Peer {
            api: api.into(),
            ip: "10.0.0.9".parse().unwrap(),
            name: None,
            node: node.map(Into::into),
        };
        assert!(c.connects(&peer("http://10.0.0.1:7480", None), Priority::Normal));
        // The peer moved to another address.
        assert!(c.connects(
            &peer("http://10.0.0.9:7480", Some("NODE1")),
            Priority::Normal
        ));
        assert!(!c.connects(&peer("http://10.0.0.1:7480", None), Priority::High));
        assert!(!c.connects(
            &peer("http://10.0.0.9:7480", Some("NODE2")),
            Priority::Normal
        ));
        assert!(!c.connects(&peer("http://10.0.0.9:7480", None), Priority::Normal));
        assert!(!c.sends(c.peer, Priority::Normal));

        let only = Cached {
            api: None,
            node: None,
            ..c
        };
        assert!(only.sends(only.peer, Priority::Normal));
        assert!(!only.sends("10.0.0.3:5004".parse().unwrap(), Priority::Normal));
        assert!(!only.connects(&peer("http://10.0.0.1:7480", None), Priority::Normal));
    }

    #[test]
    fn peers_move_to_addresses_ranked_as_well_or_when_gone() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        let v4 = Some(ip("10.0.0.1"));
        let never = || -> bool { panic!("asked although the address ranks as well") };
        assert!(moved(v4, ip("10.0.0.2"), never));
        assert!(moved(None, ip("2001:db8::1"), never));
        assert!(moved(Some(ip("2001:db8::1")), ip("10.0.0.2"), never));
        // Only the IPv6 address announced so far: stay while IPv4 answers.
        assert!(!moved(v4, ip("2001:db8::1"), || true));
        assert!(moved(v4, ip("2001:db8::1"), || false));
        assert!(!moved(Some(ip("2001:db8::1")), ip("fe80::1"), || true));
    }

    #[test]
    fn eviction_keeps_recent_and_pinned_routes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peers.json");
        std::fs::write(&path, r#"{"PINNED": {"pinned": true}}"#).unwrap();
        let peers = Peers::load(path);
        let now = unix_now();
        let day = 86_400;
        let mut conns = Connections::new(
            dir.path().join("routes.json"),
            RoutesConfig {
                max_age_days: 30,
                max_entries: 2,
            },
        );
        conns.cached = vec![
            cached(1, None, now - 40 * day),
            cached(2, Some("PINNED"), now - 400 * day),
            cached(3, None, now - day),
            cached(4, None, now - 2 * day),
            cached(5, None, now - 3 * day),
        ];
        conns.evict(&peers);
        let ids: Vec<u64> = conns.cached.iter().map(|c| c.id.bits()).collect();
        // Most recent first; too old or beyond two gone, pinned kept.
        assert_eq!(ids, [3, 4, 2]);
    }

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
