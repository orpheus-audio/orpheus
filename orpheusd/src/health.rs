//! Warnings about the paths streams and the clock take: Wi-Fi at either
//! end, multicast over Wi-Fi, Wi-Fi power saving, packets that arrive with
//! a wide spread, and a slow path to the PTP grandmaster; and outputs whose
//! wires add up to more than they can play.
//!
//! Wi-Fi power saving the node turns off itself where it may and can; it
//! warns where it can't, or where something keeps turning it on again.
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
use crate::i18n::{self, Arg, Text};
use crate::network::{self, own_link, wireless_to};
use crate::peer;
use crate::wifi;

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
/// announces, PTP's unicast peers and Wi-Fi power saving. None of them is
/// urgent; PTP keeps its multicast meanwhile.
const NETWORK_CHECK: Duration = Duration::from_secs(30);
/// How long the node warns after it had to turn Wi-Fi power saving off
/// again because something turned it back on.
const POWER_SAVE_BACK: Duration = Duration::from_secs(600);

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
    /// access point holds its packets until it wakes, and the node can't
    /// turn it off; or something keeps turning it on again.
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

/// Why a warning is there, where its kind has several reasons, so that a
/// user interface can offer the fitting remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// `power_save`: the node may not turn it off, without
    /// `CAP_NET_ADMIN`.
    NoCapability,
    /// `power_save`: the driver can't turn it off.
    Unsupported,
    /// `power_save`: turning it off failed otherwise.
    Failed,
    /// `power_save`: something turned it on again, and the node turned it
    /// off again.
    TurnedOnAgain,
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
    /// Why, for kinds with several reasons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
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
    /// What is wrong and what to do, for people, in the node's language
    /// (`[ui] language`).
    pub message: String,
    /// The message, to be formatted in any language.
    #[serde(skip)]
    pub text: Text,
}

/// What tells warnings apart: all but the message.
type Key<'a> = (
    WarningKind,
    Option<Reason>,
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
            self.reason,
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
    /// Per Wi-Fi interface, when the node turned power saving off, and when
    /// it last had to do so again.
    power_save_off: BTreeMap<String, (Instant, Option<Instant>)>,
}

/// Checks once a second, logs warnings as they come and go, and keeps the
/// node's announced link and PTP's unicast peers current, every
/// [`NETWORK_CHECK`].
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
        if let Some(ptp) = &node.ptp {
            let s = ptp.status();
            let wireless = !s.interface.is_empty() && network::is_wireless(&s.interface);
            ptp.set_unicast(network::ptp_unicast(wireless, s.transport, &nodes));
        }
        state.power_save = power_save(node, &mut state.power_save_off);
    }
    let mut found = clock(node, state);
    found.extend(receivers(node, state, &nodes));
    found.extend(senders(node, &nodes));
    found.extend(held(node));
    found.extend(clipping(node));
    found.extend(state.power_save.iter().cloned());

    let texts = i18n::texts(&node.settings().ui.language);
    for w in &mut found {
        w.message = texts.text(&w.text);
    }
    // The log stays in English, to be searched and quoted.
    let english = i18n::texts("en");
    let mut health = node.health.lock().unwrap();
    for w in &found {
        if !health.warnings.iter().any(|o| o.key() == w.key()) {
            tracing::warn!(on = ?w.on, sender = w.sender.as_ref().map(tracing::field::display), receiver = w.receiver, "{}", english.text(&w.text));
        }
    }
    for w in &health.warnings {
        if !found.iter().any(|n| n.key() == w.key()) {
            tracing::info!(
                on = ?w.on,
                sender = w.sender.as_ref().map(tracing::field::display),
                receiver = w.receiver,
                "resolved: {}",
                english.text(&w.text)
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
    let warn = |kind, text| Warning {
        kind,
        reason: None,
        on: Subject::Clock,
        sender: None,
        receiver: None,
        output: None,
        interface: None,
        message: String::new(),
        text,
    };
    if !s.running {
        let why: Arg = match &s.error {
            Some(e) => e.as_str().into(),
            None => Text::new("ptp-not-started").into(),
        };
        let id = if s.holdover {
            "warning-ptp-holdover"
        } else {
            "warning-ptp-down"
        };
        return vec![warn(WarningKind::PtpDown, Text::new(id).arg("why", why))];
    }
    let mut found = vec![];
    let following = matches!(s.state, PtpState::Slave | PtpState::Uncalibrated);
    let wireless = network::is_wireless(&s.interface);
    let cause = || {
        if wireless {
            wifi_cause(&s.interface)
        } else {
            Text::new("cause-unknown")
        }
    };
    if wireless {
        // Wi-Fi hurts the nodes that follow, so a grandmaster on Wi-Fi is
        // fine on its own but a problem for every other node.
        found.push(if following {
            warn(
                WarningKind::Wifi,
                Text::new("warning-ptp-wifi").arg("interface", s.interface.as_str()),
            )
        } else {
            let role = if matches!(s.state, PtpState::Master) {
                "master"
            } else {
                "other"
            };
            warn(
                WarningKind::WifiGrandmaster,
                Text::new("warning-wifi-grandmaster")
                    .arg("role", role)
                    .arg("interface", s.interface.as_str()),
            )
        });
    }
    if let Some(delay) = s.mean_delay_ns.filter(|_| following) {
        let ms = delay as f64 / 1e6;
        if ms > PTP_DELAY_LIMIT_MS {
            found.push(warn(
                WarningKind::PtpDelay,
                Text::new("warning-ptp-delay")
                    .arg("ms", Arg::Decimal(ms, 1))
                    .arg("limit", Arg::Decimal(PTP_DELAY_LIMIT_MS, 1))
                    .arg("cause", cause()),
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
            Text::new("warning-ptp-steps")
                .arg("count", recent)
                .arg("cause", cause()),
        ));
    }
    found
}

/// Turns power saving off on the Wi-Fi interfaces the node uses, PTP's
/// and the default route's, and warns where it can't or where something
/// keeps turning it on again. Checked again and again: laptops often turn
/// it on when they go on battery, NetworkManager when it reconnects.
fn power_save(
    node: &Node,
    turned_off: &mut BTreeMap<String, (Instant, Option<Instant>)>,
) -> Vec<Warning> {
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
    turned_off.retain(|i, _| interfaces.contains(i));
    let mut found = vec![];
    for i in interfaces {
        let warning = if wifi::power_save(&i) == Some(true) {
            match wifi::power_save_off(&i) {
                Ok(()) => {
                    let now = Instant::now();
                    match turned_off.get_mut(&i) {
                        Some((_, again)) => *again = Some(now),
                        None => {
                            turned_off.insert(i.clone(), (now, None));
                        }
                    }
                    tracing::info!(interface = %i, "turned Wi-Fi power saving off");
                    None
                }
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    let exe = std::env::current_exe()
                        .map_or_else(|_| "path/to/orpheusd".into(), |p| p.display().to_string());
                    Some((
                        Reason::NoCapability,
                        Text::new("warning-power-save-no-capability").arg("exe", exe),
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => Some((
                    Reason::Unsupported,
                    Text::new("warning-power-save-unsupported"),
                )),
                Err(e) => Some((
                    Reason::Failed,
                    Text::new("warning-power-save-failed").arg("error", e.to_string()),
                )),
            }
        } else {
            None
        };
        let back = turned_off
            .get(&i)
            .and_then(|(_, again)| *again)
            .filter(|t| t.elapsed() < POWER_SAVE_BACK);
        let warning = warning.or_else(|| {
            back.map(|_| {
                (
                    Reason::TurnedOnAgain,
                    Text::new("warning-power-save-turned-on-again"),
                )
            })
        });
        if let Some((reason, text)) = warning {
            found.push(Warning {
                kind: WarningKind::PowerSave,
                reason: Some(reason),
                on: Subject::Interface,
                sender: None,
                receiver: None,
                output: None,
                message: String::new(),
                text: text.arg("interface", i.as_str()),
                interface: Some(i),
            });
        }
    }
    found
}

/// The likely cause of a slow or unsteady path when this node's end of it
/// is the Wi-Fi `interface`, with what the interface reports of its link.
fn wifi_cause(interface: &str) -> Text {
    let Some(station) = wifi::station(interface) else {
        return Text::new("cause-wifi").arg("interface", interface);
    };
    let mbit = |r: f64| Arg::Decimal(r, 0);
    let link = match (station.signal_dbm, station.tx_mbit) {
        (Some(s), Some(r)) => Text::new("wifi-link").arg("dbm", s).arg("mbit", mbit(r)),
        (Some(s), None) => Text::new("wifi-link-signal").arg("dbm", s),
        (None, Some(r)) => Text::new("wifi-link-rate").arg("mbit", mbit(r)),
        (None, None) => return Text::new("cause-wifi").arg("interface", interface),
    };
    let id = if station.weak() {
        "cause-wifi-weak"
    } else {
        "cause-wifi-link"
    };
    Text::new(id).arg("interface", interface).arg("link", link)
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
        let warn = |kind, text| Warning {
            kind,
            reason: None,
            on: Subject::Receiver,
            sender: None,
            receiver: Some(id),
            output: None,
            interface: None,
            message: String::new(),
            text,
        };
        let from: Arg = match name {
            Some(n) => n.into(),
            None => Text::new("the-sender").into(),
        };
        if let Some(leg) = &leg {
            if let Some(group) = leg.multicast_ip {
                if let Some(i) = wireless_to(group) {
                    found.push(warn(
                        WarningKind::MulticastWifi,
                        Text::new("warning-receiver-multicast-wifi")
                            .arg("receiver", id)
                            .arg("group", group.to_string())
                            .arg("interface", i),
                    ));
                }
            } else if let Some(source) = leg.source_ip
                && let Some(i) = wireless_to(source)
            {
                found.push(warn(
                    WarningKind::Wifi,
                    Text::new("warning-receiver-wifi")
                        .arg("receiver", id)
                        .arg("from", from.clone())
                        .arg("interface", i),
                ));
            }
            if let Some(peer) = leg
                .source_ip
                .and_then(|ip| node_at(nodes, ip))
                .filter(|n| n.link == Some(Link::Wifi))
            {
                found.push(warn(
                    WarningKind::PeerWifi,
                    Text::new("warning-receiver-peer-wifi")
                        .arg("peer", peer.name.as_str())
                        .arg("receiver", id),
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
            let cause = leg
                .as_ref()
                .and_then(|l| l.source_ip)
                .and_then(wireless_to)
                .map_or_else(|| Text::new("cause-unknown"), |i| wifi_cause(&i));
            found.push(warn(
                WarningKind::Jitter,
                Text::new("warning-jitter")
                    .arg("from", from)
                    .arg("ms", Arg::Decimal(worst, 1))
                    .arg("limit", Arg::Decimal(JITTER_LIMIT_MS, 1))
                    .arg("cause", cause),
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
            reason: None,
            on: Subject::Receiver,
            sender: None,
            receiver: Some(id),
            output: None,
            interface: None,
            message: String::new(),
            text: Text::new("warning-held").arg("receiver", id),
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
                reason: None,
                on: Subject::Output,
                sender: None,
                receiver: None,
                message: String::new(),
                text: Text::new("warning-clipping").arg("output", name),
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
        let warn = |kind, text| Warning {
            kind,
            reason: None,
            on: Subject::Sender,
            sender: Some(id),
            receiver: None,
            output: None,
            interface: None,
            message: String::new(),
            text,
        };
        match links.get(&id).map(|l| l.state) {
            Some(PeerState::Busy) => found.push(warn(
                WarningKind::PeerBusy,
                Text::new("warning-peer-busy").arg("route", s.name.as_str()),
            )),
            Some(PeerState::Incompatible) => found.push(warn(
                WarningKind::PeerIncompatible,
                Text::new("warning-peer-incompatible").arg("route", s.name.as_str()),
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
                    Text::new("warning-sender-multicast-wifi")
                        .arg("route", name.as_str())
                        .arg("group", dest.to_string())
                        .arg("interface", i),
                ));
            }
            continue;
        }
        if let Some(i) = wireless_to(dest) {
            found.push(warn(
                WarningKind::Wifi,
                Text::new("warning-sender-wifi")
                    .arg("route", name.as_str())
                    .arg("interface", i),
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
                Text::new("warning-sender-peer-wifi")
                    .arg("route", name.as_str())
                    .arg("peer", peer.name.as_str()),
            ));
        }
        if link.is_some_and(|l| l.jitter) {
            found.push(warn(
                WarningKind::PeerJitter,
                Text::new("warning-peer-jitter")
                    .arg("route", name.as_str())
                    .arg("limit", Arg::Decimal(JITTER_LIMIT_MS, 1)),
            ));
        }
    }
    found
}
