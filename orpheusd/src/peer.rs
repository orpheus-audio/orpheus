//! Other nodes, as a client: finding a node by name, picking a free
//! receiver on it and connecting that receiver to one of our senders
//! through its IS-05 API.
//!
//! Everything here blocks on the network; call it without holding locks.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use axum::http::StatusCode;
use orpheus_core::{DeviceId, OutputType};
use orpheus_nmos::is04::{TAG_FORM_FACTOR, TAG_STATE, TAG_WARNINGS};
use orpheus_nmos::{Uuid, caps, is05, resource_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Node;
use crate::config::RTP_PORT;
use crate::peers::Known;
use crate::policy::State;

/// A failed request: status and message.
pub type Fail = (StatusCode, String);

/// The port other nodes' APIs listen on unless discovery says otherwise.
const API_PORT: u16 = 7480;

/// How long a peer may take to answer.
const TIMEOUT: Duration = Duration::from_secs(3);

/// A node to connect to.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Base URL of its API, without a trailing slash.
    pub api: String,
    /// The address to send to.
    pub ip: IpAddr,
    /// Its name, if discovery knows it.
    pub name: Option<String>,
    /// Its Orpheus device ID, if discovery knows it.
    pub node: Option<String>,
}

/// One of a peer's receivers, chosen to play a sender of ours.
#[derive(Debug, Clone)]
pub struct Target {
    pub receiver: Uuid,
    /// The UDP port it listens on.
    pub port: u16,
    /// The kind of output it plays on.
    pub output: OutputType,
}

/// A receiver on another node that a sender of ours is connected to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerReceiver {
    /// Base URL of the node's API.
    pub api: String,
    pub id: Uuid,
    /// The node's Orpheus device ID, to find it again when its address
    /// changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

impl PeerReceiver {
    /// Whether it is on the node `peer`.
    pub fn on(&self, peer: &Peer) -> bool {
        self.api == peer.api || (self.node.is_some() && self.node == peer.node)
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .http_status_as_error(false)
        .build()
        .into()
}

fn gateway(api: &str, e: impl std::fmt::Display) -> Fail {
    (StatusCode::BAD_GATEWAY, format!("{api}: {e}"))
}

/// A request that got no response.
fn unreachable(api: &str, e: ureq::Error) -> Fail {
    match e {
        ureq::Error::Timeout(_) => gateway(api, format!("no answer within {TIMEOUT:?}")),
        e => gateway(api, e),
    }
}

/// Finds a node: by the name or ID it announces, by host name or address
/// (its API on port 7480), or by the URL of its API.
pub fn resolve(node: &Node, peer: &str) -> Result<Peer, Fail> {
    let bad = |e: String| (StatusCode::BAD_REQUEST, e);
    let unknown = |e: String| (StatusCode::UNPROCESSABLE_ENTITY, e);
    if let Some(rest) = peer
        .strip_prefix("http://")
        .or_else(|| peer.strip_prefix("https://"))
    {
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, _) = crate::api::split_host_port(authority)
            .ok_or_else(|| bad(format!("bad URL {peer:?}")))?;
        let api = peer.trim_end_matches('/').to_owned();
        return Ok(Peer {
            node: announced_at(node, &api),
            api,
            ip: lookup(host).map_err(unknown)?,
            name: None,
        });
    }

    let id: Option<DeviceId> = peer.parse().ok();
    let known: Vec<Known> = match &node.discovery {
        Some(d) => node.peers.lock().unwrap().list(d.nodes()),
        None => vec![],
    };
    let online = || known.iter().filter(|k| k.node.online);
    // An alias first: it is this node's own name for the peer.
    let found = online()
        .find(|k| {
            k.alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(peer))
        })
        .or_else(|| {
            online().find(|k| {
                let n = &k.node;
                let host = n.host.trim_end_matches('.').trim_end_matches(".local");
                n.name.eq_ignore_ascii_case(peer)
                    || (id.is_some() && n.id.as_deref().and_then(|i| i.parse().ok()) == id)
                    || host.eq_ignore_ascii_case(peer.trim_end_matches(".local"))
                    || n.addresses.iter().any(|a| a == peer)
            })
        });
    if let Some(k) = found {
        let (ip, api) =
            address(&k.node).ok_or_else(|| gateway(k.shown(), "announces no usable address"))?;
        return Ok(Peer {
            api,
            ip,
            name: Some(k.shown().to_owned()),
            node: k.node.id.clone(),
        });
    }

    let (host, port) =
        crate::api::split_host_port(peer).ok_or_else(|| bad(format!("bad peer {peer:?}")))?;
    if port.is_some() {
        return Err(bad(format!(
            "{peer}: the peer's receiver chooses the port; give one only to send without connecting it"
        )));
    }
    let ip = lookup(host).map_err(unknown)?;
    let api = api_at(ip);
    Ok(Peer {
        node: announced_at(node, &api),
        api,
        ip,
        name: None,
    })
}

/// The URL of the API of the node at `ip`, if it listens on the usual port.
pub fn api_at(ip: IpAddr) -> String {
    format!("http://{}", SocketAddr::new(ip, API_PORT))
}

/// Where to reach a node discovery sees: its first usable address and the
/// URL of its API there.
pub fn address(n: &orpheus_discovery::Node) -> Option<(IpAddr, String)> {
    // Addresses come IPv4 first, link-local IPv6 last.
    let ip = n.addresses.iter().find_map(|a| a.parse::<IpAddr>().ok())?;
    Some((ip, format!("http://{}", SocketAddr::new(ip, n.port))))
}

/// The device ID of the node discovery sees serving `api`.
fn announced_at(node: &Node, api: &str) -> Option<String> {
    let d = node.discovery.as_ref()?;
    d.nodes().into_iter().find(|n| serves(n, api))?.id
}

/// A host's address, IPv4 preferred.
fn lookup(host: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    let found: Vec<SocketAddr> = (host, RTP_PORT)
        .to_socket_addrs()
        .map_err(|e| format!("resolving {host}: {e}"))?
        .collect();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or(found.first())
        .map(SocketAddr::ip)
        .ok_or_else(|| format!("{host} has no address"))
}

/// GETs JSON from the peer.
fn get(api: &str, path: &str) -> Result<Value, Fail> {
    let url = format!("{api}{path}");
    let mut r = agent().get(&url).call().map_err(|e| unreachable(api, e))?;
    if !r.status().is_success() {
        return Err(gateway(&url, r.status()));
    }
    r.body_mut().read_json().map_err(|e| gateway(&url, e))
}

/// PATCHes a receiver's staged parameters, activating them.
fn patch_receiver(api: &str, receiver: Uuid, body: &Value) -> Result<(), Fail> {
    let url = format!(
        "{api}/x-nmos/connection/{}/single/receivers/{receiver}/staged",
        is05::API_VERSION
    );
    let mut r = agent()
        .patch(&url)
        .send_json(body)
        .map_err(|e| unreachable(api, e))?;
    if r.status().is_success() {
        return Ok(());
    }
    let status = r.status();
    let msg = r
        .body_mut()
        .read_json::<Value>()
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| status.to_string());
    Err((StatusCode::BAD_GATEWAY, format!("{api} refused: {msg}")))
}

/// Picks a receiver on `peer` whose capabilities (BCP-004-01) take our
/// stream of `channels` channels and that has no sender, or with `force`,
/// one that plays another node's sender. `ours` are our senders, whose
/// receivers are never taken.
pub fn find_receiver(
    peer: &Peer,
    ours: &[Uuid],
    force: bool,
    channels: usize,
) -> Result<Target, Fail> {
    let receivers = get(&peer.api, "/x-nmos/node/v1.3/receivers")?;
    let receivers = receivers.as_array().cloned().unwrap_or_default();
    let audio: Vec<&Value> = receivers
        .iter()
        .filter(|r| {
            r["format"] == "urn:x-nmos:format:audio"
                && r["transport"]
                    .as_str()
                    .is_some_and(|t| t.starts_with(is05::TRANSPORT_RTP))
        })
        .collect();
    let name = peer.name.as_deref().unwrap_or(&peer.api);
    if audio.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{name} has no audio receiver over RTP"),
        ));
    }
    let stream = crate::nmos::stream(channels);
    let checked: Vec<_> = audio
        .iter()
        .map(|r| (*r, caps::check(&r["caps"], &stream)))
        .collect();
    let fitting: Vec<&Value> = checked
        .iter()
        .filter(|(_, fit)| fit.is_ok())
        .map(|(r, _)| *r)
        .collect();
    if fitting.is_empty() {
        let why = checked.iter().find_map(|(_, fit)| fit.clone().err());
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{name} has no receiver for {}/{}/{}: {}",
                stream.media_type.trim_start_matches("audio/"),
                stream.sample_rate,
                stream.channel_count,
                why.unwrap_or_default()
            ),
        ));
    }
    let chosen = match fitting.iter().find(|r| holder(r).is_none()) {
        Some(r) => r,
        None if !force => {
            return Err((
                StatusCode::CONFLICT,
                format!("{name} is playing another sender; force takes it over"),
            ));
        }
        None => fitting
            .iter()
            .find(|r| holder(r).is_none_or(|h| !ours.contains(&h)))
            .ok_or((
                StatusCode::CONFLICT,
                format!("all of {name}'s receivers play senders of this node"),
            ))?,
    };
    let receiver: Uuid = chosen["id"]
        .as_str()
        .and_then(|i| i.parse().ok())
        .ok_or_else(|| gateway(&peer.api, "receiver without an ID"))?;
    let output = form_factor(chosen);
    let active = get(
        &peer.api,
        &format!(
            "/x-nmos/connection/{}/single/receivers/{receiver}/active",
            is05::API_VERSION
        ),
    )?;
    let port = active["transport_params"][0]["destination_port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(RTP_PORT);
    Ok(Target {
        receiver,
        port,
        output,
    })
}

/// The sender a receiver resource is connected to.
fn holder(r: &Value) -> Option<Uuid> {
    let s = &r["subscription"];
    (s["active"] != false)
        .then(|| s["sender_id"].as_str()?.parse::<Uuid>().ok())
        .flatten()
}

/// The output type a receiver resource announces.
fn form_factor(receiver: &Value) -> OutputType {
    receiver["tags"][TAG_FORM_FACTOR][0]
        .as_str()
        .and_then(|t| serde_json::from_value(json!(t)).ok())
        .unwrap_or_default()
}

/// What a peer's receiver says about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playing {
    /// The kind of output it plays on.
    pub output: OutputType,
    /// The sender it is connected to, if any.
    pub sender: Option<Uuid>,
    /// Whether it warns that packets arrive with a wide spread.
    pub jitter: bool,
    /// Whether its policy mutes or ducks the stream.
    pub held: Option<State>,
    /// Why it plays the stream nowhere, if it parked it.
    pub parked: Option<Parked>,
}

/// Why a peer's receiver plays its stream nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Parked {
    /// The output it played on went: switched off, unplugged, out of reach.
    Lost,
    /// Its user removed the stream's last wire.
    Unwired,
}

/// Asks a peer's receiver what it plays and on what.
pub fn playing(of: &PeerReceiver) -> Result<Playing, Fail> {
    let r = get(&of.api, &format!("/x-nmos/node/v1.3/receivers/{}", of.id))?;
    let state = r["tags"][TAG_STATE][0].as_str();
    Ok(Playing {
        output: form_factor(&r),
        sender: holder(&r),
        jitter: r["tags"][TAG_WARNINGS]
            .as_array()
            .is_some_and(|w| w.iter().any(|w| w == "jitter")),
        held: match state {
            Some("muted") => Some(State::Muted),
            Some("ducked") => Some(State::Ducked),
            _ => None,
        },
        parked: match state {
            Some("lost") => Some(Parked::Lost),
            Some("parked") => Some(Parked::Unwired),
            _ => None,
        },
    })
}

/// Whether a node's sender `sender` still sends to our receiver `ours`:
/// false once the sender is gone or connected to another receiver.
/// `node` is the sending node's device ID; the node at `api` must be it.
pub fn still_sends(api: &str, node: DeviceId, sender: Uuid, ours: Uuid) -> Result<bool, Fail> {
    let this = get(api, "/x-nmos/node/v1.3/self")?;
    if this["id"].as_str().and_then(|i| i.parse().ok()) != Some(resource_id(node, "node")) {
        return Err(gateway(api, format!("not node {node}")));
    }
    let url = format!("{api}/x-nmos/node/v1.3/senders/{sender}");
    let mut r = agent().get(&url).call().map_err(|e| unreachable(api, e))?;
    if r.status() == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !r.status().is_success() {
        return Err(gateway(&url, r.status()));
    }
    let s: Value = r.body_mut().read_json().map_err(|e| gateway(&url, e))?;
    let receiver = s["subscription"]["receiver_id"].as_str();
    Ok(receiver.is_none_or(|r| r.parse() == Ok(ours)))
}

/// Whether discovery's `node` is the node whose API is at `api`.
pub fn serves(node: &orpheus_discovery::Node, api: &str) -> bool {
    node.addresses
        .iter()
        .filter_map(|a| a.parse::<IpAddr>().ok())
        .any(|ip| api == format!("http://{}", SocketAddr::new(ip, node.port)))
}

/// Connects a peer's receiver to our sender `sender`, described by `sdp`.
pub fn connect(to: &PeerReceiver, sender: Uuid, sdp: &str) -> Result<(), Fail> {
    patch_receiver(
        &to.api,
        to.id,
        &json!({
            "sender_id": sender,
            "master_enable": true,
            "activation": {"mode": "activate_immediate"},
            "transport_file": {"data": sdp, "type": is05::TRANSPORT_FILE_SDP},
        }),
    )
}

/// Disconnects a peer's receiver from our sender `sender`, unless it has
/// moved on to another sender meanwhile.
pub fn release(from: &PeerReceiver, sender: Uuid) -> Result<(), Fail> {
    let active = get(
        &from.api,
        &format!(
            "/x-nmos/connection/{}/single/receivers/{}/active",
            is05::API_VERSION,
            from.id
        ),
    )?;
    if active["sender_id"].as_str().and_then(|s| s.parse().ok()) != Some(sender) {
        return Ok(());
    }
    patch_receiver(
        &from.api,
        from.id,
        &json!({
            "sender_id": null,
            "master_enable": true,
            "activation": {"mode": "activate_immediate"},
            "transport_file": {"data": null, "type": null},
            "transport_params": [{"source_ip": null, "multicast_ip": null}],
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receivers_belong_to_their_node_at_any_address() {
        let peer = |api: &str, node: Option<&str>| Peer {
            api: api.into(),
            ip: [10, 0, 0, 2].into(),
            name: None,
            node: node.map(Into::into),
        };
        let r = |node: Option<&str>| PeerReceiver {
            api: "http://10.0.0.2:7480".into(),
            id: Uuid::nil(),
            node: node.map(Into::into),
        };
        assert!(r(None).on(&peer("http://10.0.0.2:7480", None)));
        assert!(r(Some("B73AQ0P0")).on(&peer("http://10.0.0.9:7480", Some("B73AQ0P0"))));
        assert!(!r(Some("B73AQ0P0")).on(&peer("http://10.0.0.9:7480", Some("C00000P0"))));
        assert!(!r(None).on(&peer("http://10.0.0.9:7480", None)));
    }
}
