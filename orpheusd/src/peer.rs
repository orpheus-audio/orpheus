//! Other nodes, as a client: finding a node by name, picking a free
//! receiver on it and connecting that receiver to one of our senders
//! through its IS-05 API.
//!
//! Everything here blocks on the network; call it without holding locks.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use axum::http::StatusCode;
use orpheus_core::{DeviceId, OutputType};
use orpheus_nmos::is04::TAG_FORM_FACTOR;
use orpheus_nmos::{Uuid, is05};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::Node;
use crate::config::RTP_PORT;

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
    if let Some(rest) = peer
        .strip_prefix("http://")
        .or_else(|| peer.strip_prefix("https://"))
    {
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, _) = crate::api::split_host_port(authority)
            .ok_or_else(|| bad(format!("bad URL {peer:?}")))?;
        return Ok(Peer {
            api: peer.trim_end_matches('/').to_owned(),
            ip: lookup(host).map_err(bad)?,
            name: None,
        });
    }

    let id: Option<DeviceId> = peer.parse().ok();
    let found = node.discovery.as_ref().and_then(|d| {
        d.nodes().into_iter().find(|n| {
            let host = n.host.trim_end_matches('.').trim_end_matches(".local");
            n.online
                && (n.name.eq_ignore_ascii_case(peer)
                    || (id.is_some() && n.id.as_deref().and_then(|i| i.parse().ok()) == id)
                    || host.eq_ignore_ascii_case(peer.trim_end_matches(".local"))
                    || n.addresses.iter().any(|a| a == peer))
        })
    });
    if let Some(n) = found {
        // Addresses come IPv4 first, link-local IPv6 last.
        let ip = n
            .addresses
            .iter()
            .find_map(|a| a.parse::<IpAddr>().ok())
            .ok_or_else(|| bad(format!("{} has no usable address", n.name)))?;
        return Ok(Peer {
            api: format!("http://{}", SocketAddr::new(ip, n.port)),
            ip,
            name: Some(n.name),
        });
    }

    let (host, port) =
        crate::api::split_host_port(peer).ok_or_else(|| bad(format!("bad peer {peer:?}")))?;
    if port.is_some() {
        return Err(bad(format!(
            "{peer}: the peer's receiver chooses the port; give one only to send without connecting it"
        )));
    }
    let ip = lookup(host).map_err(bad)?;
    Ok(Peer {
        api: format!("http://{}", SocketAddr::new(ip, API_PORT)),
        ip,
        name: None,
    })
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

/// Picks a receiver on `peer` that plays our format and has no sender, or
/// with `force`, one that has. `ours` names our senders, to tell the user
/// when one of them already holds the receiver.
pub fn find_receiver(peer: &Peer, ours: &[(u32, Uuid)], force: bool) -> Result<Target, Fail> {
    let receivers = get(&peer.api, "/x-nmos/node/v1.3/receivers")?;
    let receivers = receivers.as_array().cloned().unwrap_or_default();
    let fits = |r: &&Value| {
        r["format"] == "urn:x-nmos:format:audio"
            && r["transport"]
                .as_str()
                .is_some_and(|t| t.starts_with(is05::TRANSPORT_RTP))
            && r["caps"]["media_types"]
                .as_array()
                .is_none_or(|m| m.iter().any(|t| t == "audio/L24"))
    };
    let holder = |r: &Value| {
        let s = &r["subscription"];
        (s["active"] != false)
            .then(|| s["sender_id"].as_str()?.parse::<Uuid>().ok())
            .flatten()
    };
    let fitting: Vec<&Value> = receivers.iter().filter(fits).collect();
    let name = peer.name.as_deref().unwrap_or(&peer.api);
    let chosen = match fitting.iter().find(|r| holder(r).is_none()) {
        Some(r) => r,
        None => {
            let first = fitting.first().ok_or((
                StatusCode::CONFLICT,
                format!("{name} has no receiver for L24/48000/2 over RTP"),
            ))?;
            let held = holder(first);
            if let Some((n, _)) = ours.iter().find(|(_, u)| Some(*u) == held) {
                return Err((
                    StatusCode::CONFLICT,
                    format!("already connected to {name} as sender {n}"),
                ));
            }
            if !force {
                return Err((
                    StatusCode::CONFLICT,
                    format!("{name} is playing another sender; force takes it over"),
                ));
            }
            first
        }
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

/// The output type a receiver resource announces.
fn form_factor(receiver: &Value) -> OutputType {
    receiver["tags"][TAG_FORM_FACTOR][0]
        .as_str()
        .and_then(|t| serde_json::from_value(json!(t)).ok())
        .unwrap_or_default()
}

/// The output type a peer's receiver announces now.
pub fn output_type(of: &PeerReceiver) -> Result<OutputType, Fail> {
    get(&of.api, &format!("/x-nmos/node/v1.3/receivers/{}", of.id)).map(|r| form_factor(&r))
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
