//! Warnings about the paths streams and the clock take: Wi-Fi at either
//! end, multicast over Wi-Fi, Wi-Fi power saving, packets that arrive with
//! a wide spread, and a slow path to the PTP grandmaster; and outputs whose
//! wires add up to more than they can play.
//!
//! Wi-Fi is seen directly on this node's own interfaces and through the
//! link each peer announces over mDNS. Spread is measured by the receiver;
//! its warning goes into the receiver's IS-04 resource, so that the
//! sending node warns too. It is not checked while the PTP clock settles
//! or for ten seconds after it stepped, as moving the clock
//! moves the arrival times measured with it. Wi-Fi further along the way (a wired node behind
//! a wireless bridge) shows only as spread.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orpheus_clock::ptp::PtpState;
use orpheus_discovery::Link;
use orpheus_engine::{ReceiverId, SenderId};
use serde::Serialize;
use utoipa::ToSchema;

use crate::api::Node;
use crate::connection::PeerState;
use crate::network::{self, own_link, wireless_to};
use crate::peer;

/// Spread of arrival times above which the receiver warns. A wired
/// network stays well below; Wi-Fi often reaches 5 to 50 ms.
pub const JITTER_LIMIT_MS: f64 = 3.0;
/// The receiver warns when the spread was above the limit in this many of
/// the last [`JITTER_WINDOW`] seconds.
const JITTER_HITS: usize = 3;
const JITTER_WINDOW: usize = 10;
/// Mean path delay to the PTP grandmaster above which the clock warns.
/// Wired, with software timestamps, it is tens to a few hundred µs.
pub const PTP_DELAY_LIMIT_MS: f64 = 0.5;
/// The clock warns when it stepped this often within [`STEPS_WINDOW`]. One
/// or two steps after a start are normal; jitter above the step threshold
/// makes it step over and over and never settle.
const STEPS_LIMIT: u64 = 3;
const STEPS_WINDOW: Duration = Duration::from_secs(60);
/// An output warns for this long after it last reached full scale.
const CLIP_WINDOW: Duration = Duration::from_secs(10);
/// How often the network is looked at again: the node's own link, which it
/// announces, and Wi-Fi power saving. Neither is urgent.
const NETWORK_CHECK: Duration = Duration::from_secs(30);

/// What is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WarningKind {
    /// The stream leaves this node over Wi-Fi, or the clock follows a
    /// grandmaster over Wi-Fi.
    Wifi,
    /// Other nodes would follow this node's clock over Wi-Fi.
    WifiGrandmaster,
    /// A multicast stream goes over Wi-Fi, where the access point sends it
    /// at its lowest rate to every client.
    MulticastWifi,
    /// The node at the other end announces that it is on Wi-Fi.
    PeerWifi,
    /// Power saving is on for a Wi-Fi interface the node uses, so the
    /// access point holds its packets until it wakes.
    PowerSave,
    /// The receiver's packets arrive with a wide spread.
    Jitter,
    /// The peer's receiver reports a wide spread for this sender's packets.
    PeerJitter,
    /// The path to the PTP grandmaster is slow.
    PtpDelay,
    /// The PTP clock keeps stepping instead of settling.
    PtpSteps,
    /// PTP is not running; the clock runs on by itself.
    PtpDown,
    /// The receiver's stream plays nowhere: the default output is a sink
    /// to another node.
    Held,
    /// The route wants a receiver on the peer, but the peer has no free
    /// one.
    PeerBusy,
    /// None of the peer's receivers can play the route's stream.
    PeerIncompatible,
    /// What plays on an output reached full scale: the sum of its wires
    /// and anything else on it clips.
    Clipping,
}

/// What a warning is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Clock,
    Receiver,
    Sender,
    Output,
    Interface,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Warning {
    pub kind: WarningKind,
    pub on: Subject,
    /// The route, for warnings on a sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub sender: Option<SenderId>,
    /// The receiver, for warnings on a receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverId>,
    /// The output, by machine name, for warnings on an output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The network interface, for warnings on an interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// What is wrong and what to do, for people.
    pub message: String,
}

/// What tells warnings apart: all but the message.
type Key<'a> = (
    WarningKind,
    Subject,
    Option<SenderId>,
    Option<ReceiverId>,
    Option<&'a str>,
    Option<&'a str>,
);

impl Warning {
    fn key(&self) -> Key<'_> {
        (
            self.kind,
            self.on,
            self.sender,
            self.receiver,
            self.output.as_deref(),
            self.interface.as_deref(),
        )
    }
}

/// The current warnings. Lock it last.
#[derive(Default)]
pub struct Health {
    warnings: Vec<Warning>,
}

impl Health {
    pub fn warnings(&self) -> Vec<Warning> {
        self.warnings.clone()
    }

    /// Whether receiver `id`'s packets arrive with a wide spread, which
    /// its IS-04 resource tells the sending node.
    pub fn receiver_jitter(&self, id: ReceiverId) -> bool {
        self.warnings
            .iter()
            .any(|w| w.kind == WarningKind::Jitter && w.receiver == Some(id))
    }
}

/// What the checks remember between rounds.
#[derive(Default)]
struct State {
    /// Each receiver's spread over the last seconds, in µs.
    jitter: BTreeMap<ReceiverId, VecDeque<i64>>,
    /// The PTP step counter over the last minute.
    steps: VecDeque<(Instant, u64)>,
    /// When the PTP clock was last unsettled or stepped.
    unsteady: Option<Instant>,
    network_checked: Option<Instant>,
    /// The power saving warnings of the last network check.
    power_save: Vec<Warning>,
}

/// Checks once a second, logs warnings as they come and go, and keeps the
/// node's announced link current, every [`NETWORK_CHECK`].
pub async fn watch(node: Arc<Node>) {
    let mut state = State::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let node = node.clone();
        match tokio::task::spawn_blocking(move || {
            check(&node, &mut state);
            state
        })
        .await
        {
            Ok(s) => state = s,
            Err(_) => state = State::default(),
        }
    }
}

fn check(node: &Node, state: &mut State) {
    let nodes = node
        .discovery
        .as_ref()
        .map(|d| d.nodes())
        .unwrap_or_default();
    if state
        .network_checked
        .is_none_or(|t| t.elapsed() >= NETWORK_CHECK)
    {
        state.network_checked = Some(Instant::now());
        if let Some(d) = &node.discovery
            && let Err(e) = d.set_link(own_link())
        {
            tracing::warn!(error = %e, "announcing the node's link");
        }
        state.power_save = power_save(node);
    }
    let mut found = clock(node, state);
    found.extend(receivers(node, state, &nodes));
    found.extend(senders(node, &nodes));
    found.extend(held(node));
    found.extend(clipping(node));
    found.extend(state.power_save.iter().cloned());

    let mut health = node.health.lock().unwrap();
    for w in &found {
        if !health.warnings.iter().any(|o| o.key() == w.key()) {
            tracing::warn!(on = ?w.on, sender = w.sender.as_ref().map(tracing::field::display), receiver = w.receiver, "{}", w.message);
        }
    }
    for w in &health.warnings {
        if !found.iter().any(|n| n.key() == w.key()) {
            tracing::info!(
                on = ?w.on,
                sender = w.sender.as_ref().map(tracing::field::display),
                receiver = w.receiver,
                "resolved: {}",
                w.message
            );
        }
    }
    health.warnings = found;
}

fn clock(node: &Node, state: &mut State) -> Vec<Warning> {
    let Some(ptp) = &node.ptp else {
        return vec![];
    };
    let s = ptp.status();
    let warn = |kind, message| Warning {
        kind,
        on: Subject::Clock,
        sender: None,
        receiver: None,
        output: None,
        interface: None,
        message,
    };
    if !s.running {
        let why = s.error.as_deref().unwrap_or("not started");
        let message = if s.holdover {
            format!(
                "PTP stopped ({why}): the clock keeps the grandmaster's last time and rate \
                 and slowly drifts from the other nodes; PTP starts again by itself"
            )
        } else {
            format!(
                "PTP is not running ({why}): the node runs on the system clock and plays \
                 out of step with the other nodes; PTP starts by itself once it can"
            )
        };
        return vec![warn(WarningKind::PtpDown, message)];
    }
    let mut found = vec![];
    let following = matches!(s.state, PtpState::Slave | PtpState::Uncalibrated);
    if network::is_wireless(&s.interface) {
        // Wi-Fi hurts the nodes that follow, so a grandmaster on Wi-Fi is
        // fine on its own but a problem for every other node.
        found.push(if following {
            warn(
                WarningKind::Wifi,
                format!(
                    "PTP runs over Wi-Fi ({}): the clock may not settle, and rooms drift apart",
                    s.interface
                ),
            )
        } else {
            let role = if matches!(s.state, PtpState::Master) {
                "is"
            } else {
                "may become"
            };
            warn(
                WarningKind::WifiGrandmaster,
                format!(
                    "this node {role} the PTP grandmaster over Wi-Fi ({}): nodes that follow \
                     its clock may not settle; give a wired node a lower ptp.priority1",
                    s.interface
                ),
            )
        });
    }
    if let Some(delay) = s.mean_delay_ns.filter(|_| following) {
        let ms = delay as f64 / 1e6;
        if ms > PTP_DELAY_LIMIT_MS {
            found.push(warn(
                WarningKind::PtpDelay,
                format!(
                    "the path to the PTP grandmaster takes {ms:.1} ms, wired it is well \
                     below {PTP_DELAY_LIMIT_MS} ms: Wi-Fi or a congested link on the way"
                ),
            ));
        }
    }

    let now = Instant::now();
    if !s.settled || state.steps.back().is_some_and(|(_, n)| *n != s.steps) {
        state.unsteady = Some(now);
    }
    state.steps.push_back((now, s.steps));
    while state
        .steps
        .front()
        .is_some_and(|(t, _)| now - *t > STEPS_WINDOW)
    {
        state.steps.pop_front();
    }
    let recent = s.steps - state.steps.front().map_or(s.steps, |(_, n)| *n);
    if recent >= STEPS_LIMIT {
        found.push(warn(
            WarningKind::PtpSteps,
            format!(
                "the PTP clock stepped {recent} times in the last minute instead of \
                 settling: the path to the grandmaster is slow or unsteady (Wi-Fi?)"
            ),
        ));
    }
    found
}

/// Wi-Fi interfaces the node uses, PTP's and the default route's, with
/// power saving on. Checked again and again: laptops often turn it on when
/// they go on battery.
fn power_save(node: &Node) -> Vec<Warning> {
    let mut interfaces: Vec<String> = node
        .ptp
        .as_ref()
        .map(|p| p.status().interface)
        .into_iter()
        .chain(network::default_interface())
        .filter(|i| !i.is_empty() && network::is_wireless(i))
        .collect();
    interfaces.sort();
    interfaces.dedup();
    interfaces
        .into_iter()
        .filter(|i| network::power_save(i) == Some(true))
        .map(|i| Warning {
            kind: WarningKind::PowerSave,
            on: Subject::Interface,
            sender: None,
            receiver: None,
            output: None,
            message: format!(
                "Wi-Fi power saving is on for {i}: the access point holds this node's packets \
                 until it wakes, up to 100 ms and more, so the clock and streams stutter; \
                 turn it off (NetworkManager: wifi.powersave = 2, or `iw dev {i} set \
                 power_save off`)"
            ),
            interface: Some(i),
        })
        .collect()
}

/// The node `ip` belongs to, if discovery sees it.
fn node_at(nodes: &[orpheus_discovery::Node], ip: IpAddr) -> Option<&orpheus_discovery::Node> {
    let ip = ip.to_canonical();
    nodes.iter().find(|n| {
        n.addresses
            .iter()
            .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok())
            .any(|a| a == ip)
    })
}

fn receivers(node: &Node, state: &mut State, nodes: &[orpheus_discovery::Node]) -> Vec<Warning> {
    let running = node
        .engine
        .as_ref()
        .map(|e| e.lock().unwrap().status().receivers)
        .unwrap_or_default();
    state
        .jitter
        .retain(|id, _| running.iter().any(|(r, _)| r == id));
    let clock_moved = state
        .unsteady
        .is_some_and(|t| t.elapsed() < Duration::from_secs(JITTER_WINDOW as u64));
    let mut found = vec![];
    for (id, status) in running {
        let (leg, name) = {
            let conns = node.connections.lock().unwrap();
            let leg = conns
                .receiver_active(id)
                .filter(|a| a.master_enable)
                .and_then(|a| a.transport_params.first().cloned());
            (leg, conns.receiver_sender(id, &node.peers.lock().unwrap()))
        };
        let warn = |kind, message| Warning {
            kind,
            on: Subject::Receiver,
            sender: None,
            receiver: Some(id),
            output: None,
            interface: None,
            message,
        };
        let from = name.unwrap_or_else(|| "the sender".into());
        if let Some(leg) = &leg {
            if let Some(group) = leg.multicast_ip {
                if let Some(i) = wireless_to(group) {
                    found.push(warn(
                        WarningKind::MulticastWifi,
                        format!(
                            "receiver {id} joins multicast group {group} over Wi-Fi ({i}), \
                             which the access point sends at its lowest rate to every \
                             client; use a wired link or unicast"
                        ),
                    ));
                }
            } else if let Some(source) = leg.source_ip
                && let Some(i) = wireless_to(source)
            {
                found.push(warn(
                    WarningKind::Wifi,
                    format!(
                        "receiver {id} gets {from} over Wi-Fi ({i}); expect dropouts \
                         unless link_offset_ms is raised"
                    ),
                ));
            }
            if let Some(peer) = leg
                .source_ip
                .and_then(|ip| node_at(nodes, ip))
                .filter(|n| n.link == Some(Link::Wifi))
            {
                found.push(warn(
                    WarningKind::PeerWifi,
                    format!("{} sends to receiver {id} from Wi-Fi", peer.name),
                ));
            }
        }

        // A clock that moves shifts the arrival times it measures.
        if clock_moved {
            state.jitter.remove(&id);
            continue;
        }
        let jitter = state.jitter.entry(id).or_default();
        jitter.push_back(status.jitter_us.unwrap_or(0));
        while jitter.len() > JITTER_WINDOW {
            jitter.pop_front();
        }
        let limit = (JITTER_LIMIT_MS * 1000.0) as i64;
        let hits = jitter.iter().filter(|&&j| j > limit).count();
        if hits >= JITTER_HITS {
            let worst = jitter.iter().max().copied().unwrap_or(0) as f64 / 1000.0;
            found.push(warn(
                WarningKind::Jitter,
                format!(
                    "packets from {from} arrive spread over up to {worst:.1} ms, wired it \
                     is well below {JITTER_LIMIT_MS} ms: Wi-Fi or a congested link on the \
                     way; raise link_offset_ms if `late` rises"
                ),
            ));
        }
    }
    found
}

/// Streams waiting for an output.
fn held(node: &Node) -> Vec<Warning> {
    let ids = node.connections.lock().unwrap().receiver_ids();
    let sources = node.sources.lock().unwrap();
    ids.into_iter()
        .filter(|&id| sources.held(id))
        .map(|id| Warning {
            kind: WarningKind::Held,
            on: Subject::Receiver,
            sender: None,
            receiver: Some(id),
            output: None,
            interface: None,
            message: format!(
                "receiver {id} plays nowhere: the default output is a sink to another node, \
                 and streams are passed on only when told; choose an output with \
                 `orpheusctl play {id} <output>` or set [receiver] default_output"
            ),
        })
        .collect()
}

/// Outputs that reached full scale lately.
fn clipping(node: &Node) -> Vec<Warning> {
    let Some(engine) = &node.engine else {
        return vec![];
    };
    let (levels, outputs) = {
        let e = engine.lock().unwrap();
        (e.levels(), e.outputs())
    };
    levels
        .outputs
        .into_iter()
        .filter(|o| o.clipped.is_some_and(|t| t.elapsed() < CLIP_WINDOW))
        .map(|o| {
            let name = outputs
                .iter()
                .find(|d| d.name == o.name)
                .map_or(o.name.as_str(), |d| d.description.as_str());
            Warning {
                kind: WarningKind::Clipping,
                on: Subject::Output,
                sender: None,
                receiver: None,
                message: format!(
                    "{name} reached full scale: what plays on it adds up to more than it can \
                     play, and clips; turn its wires down (`orpheusctl wire-gain`)"
                ),
                output: Some(o.name),
                interface: None,
            }
        })
        .collect()
}

fn senders(node: &Node, nodes: &[orpheus_discovery::Node]) -> Vec<Warning> {
    let links = node.connections.lock().unwrap().peer_links();
    let Some(senders) = node
        .engine
        .as_ref()
        .map(|e| e.lock().unwrap().status().senders)
    else {
        return vec![];
    };
    let mut found = vec![];
    for (id, s) in senders {
        let warn = |kind, message| Warning {
            kind,
            on: Subject::Sender,
            sender: Some(id),
            receiver: None,
            output: None,
            interface: None,
            message,
        };
        match links.get(&id).map(|l| l.state) {
            Some(PeerState::Busy) => found.push(warn(
                WarningKind::PeerBusy,
                format!(
                    "route {}: the peer has no free receiver; it waits for one",
                    s.name
                ),
            )),
            Some(PeerState::Incompatible) => found.push(warn(
                WarningKind::PeerIncompatible,
                format!(
                    "route {}: none of the peer's receivers can play the stream",
                    s.name
                ),
            )),
            _ => {}
        }
        if !s.enabled {
            continue;
        }
        let dest = s.peer.ip();
        let name = &s.name;
        if dest.is_multicast() {
            if let Some(i) = wireless_to(dest) {
                found.push(warn(
                    WarningKind::MulticastWifi,
                    format!(
                        "route {name} sends to multicast group {dest} over Wi-Fi \
                         ({i}), which the access point sends at its lowest rate to every \
                         client; use a wired link or unicast"
                    ),
                ));
            }
            continue;
        }
        if let Some(i) = wireless_to(dest) {
            found.push(warn(
                WarningKind::Wifi,
                format!("route {name} goes out over Wi-Fi ({i}); expect dropouts"),
            ));
        }
        let link = links.get(&id);
        let peer = match link {
            Some(l) => nodes.iter().find(|n| peer::serves(n, &l.api)),
            None => node_at(nodes, dest),
        };
        if let Some(peer) = peer.filter(|n| n.link == Some(Link::Wifi)) {
            found.push(warn(
                WarningKind::PeerWifi,
                format!("route {name}: {} is on Wi-Fi; expect dropouts", peer.name),
            ));
        }
        if link.is_some_and(|l| l.jitter) {
            found.push(warn(
                WarningKind::PeerJitter,
                format!(
                    "route {name}: the peer reports the packets arriving spread over more \
                     than {JITTER_LIMIT_MS} ms: Wi-Fi or a congested link on the way"
                ),
            ));
        }
    }
    found
}
