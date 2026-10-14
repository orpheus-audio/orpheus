//! Warnings about the paths streams and the clock take: Wi-Fi at either
//! end, multicast over Wi-Fi, packets that arrive with a wide spread, and a
//! slow path to the PTP grandmaster.
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
/// How often the node's own link is checked for announcing it.
const LINK_CHECK: Duration = Duration::from_secs(5);

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
    /// The receiver's packets arrive with a wide spread.
    Jitter,
    /// The peer's receiver reports a wide spread for this sender's packets.
    PeerJitter,
    /// The path to the PTP grandmaster is slow.
    PtpDelay,
    /// The PTP clock keeps stepping instead of settling.
    PtpSteps,
}

/// What a warning is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Clock,
    Receiver,
    Sender,
}

#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Warning {
    pub kind: WarningKind,
    pub on: Subject,
    /// The sender, for warnings on a sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<SenderId>,
    /// The receiver, for warnings on a receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverId>,
    /// What is wrong and what to do, for people.
    pub message: String,
}

impl Warning {
    fn key(&self) -> (WarningKind, Subject, Option<SenderId>, Option<ReceiverId>) {
        (self.kind, self.on, self.sender, self.receiver)
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
    link_checked: Option<Instant>,
}

/// Checks once a second, logs warnings as they come and go, and keeps the
/// node's announced link current.
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
    if state.link_checked.is_none_or(|t| t.elapsed() >= LINK_CHECK) {
        state.link_checked = Some(Instant::now());
        if let Some(d) = &node.discovery
            && let Err(e) = d.set_link(own_link())
        {
            tracing::warn!(error = %e, "announcing the node's link");
        }
    }
    let nodes = node
        .discovery
        .as_ref()
        .map(|d| d.nodes())
        .unwrap_or_default();
    let mut found = clock(node, state);
    found.extend(receivers(node, state, &nodes));
    found.extend(senders(node, &nodes));

    let mut health = node.health.lock().unwrap();
    for w in &found {
        if !health.warnings.iter().any(|o| o.key() == w.key()) {
            tracing::warn!(on = ?w.on, sender = w.sender, receiver = w.receiver, "{}", w.message);
        }
    }
    for w in &health.warnings {
        if !found.iter().any(|n| n.key() == w.key()) {
            tracing::info!(
                on = ?w.on,
                sender = w.sender,
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
        message,
    };
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
        if !s.enabled {
            continue;
        }
        let warn = |kind, message| Warning {
            kind,
            on: Subject::Sender,
            sender: Some(id),
            receiver: None,
            message,
        };
        let dest = s.peer.ip();
        let name = &s.name;
        if dest.is_multicast() {
            if let Some(i) = wireless_to(dest) {
                found.push(warn(
                    WarningKind::MulticastWifi,
                    format!(
                        "sender {id} ({name}) sends to multicast group {dest} over Wi-Fi \
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
                format!("sender {id} sends to {name} over Wi-Fi ({i}); expect dropouts"),
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
                format!("sender {id}: {} is on Wi-Fi; expect dropouts", peer.name),
            ));
        }
        if link.is_some_and(|l| l.jitter) {
            found.push(warn(
                WarningKind::PeerJitter,
                format!(
                    "sender {id}: {name} reports the packets arriving spread over more \
                     than {JITTER_LIMIT_MS} ms: Wi-Fi or a congested link on the way"
                ),
            ));
        }
    }
    found
}
