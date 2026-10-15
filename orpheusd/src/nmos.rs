//! The IS-04 Node API (`/x-nmos/node/v1.3/…`) and the glue from the
//! engine's state to NMOS resources.
//!
//! The resources are rebuilt from the engine every second and on every
//! request; the [`Store`] keeps their versions and counts changes, which
//! discovery announces.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use orpheus_clock::Timescale;
use orpheus_core::rtp::Encoding;
use orpheus_core::{AudioFormat, DeviceId};
use orpheus_engine::sender::PAYLOAD_TYPE;
use orpheus_engine::{ReceiverId, SenderId};
use orpheus_nmos::caps::{AudioConstraints, Stream};
use orpheus_nmos::is04::{self, *};
use orpheus_nmos::is05;
use orpheus_nmos::is05::ReceiverParams;
use orpheus_nmos::sdp::{self, RefClock};
use orpheus_nmos::{Snapshot, Uuid, resource_id};

use crate::api::{ClockSource, Node};

const CLOCK: &str = "clk0";
/// What every sender sends and the receiver plays.
pub const FORMAT: AudioFormat = AudioFormat::AES67;
pub const ENCODING: Encoding = Encoding::L24;
pub const PTIME_MS: u32 = 1;

/// What every sender sends, as receiver capabilities see it.
pub fn stream() -> Stream<'static> {
    Stream {
        media_type: media_type(),
        sample_rate: FORMAT.rate,
        channel_count: FORMAT.channels,
        sample_depth: 8 * ENCODING.bytes_per_sample() as u8,
        packet_time_ms: PTIME_MS.into(),
    }
}

fn media_type() -> &'static str {
    match ENCODING {
        Encoding::L16 => "audio/L16",
        Encoding::L24 => "audio/L24",
    }
}

/// When the receivers' capabilities last changed: they change only with
/// the program, so when it started.
static CAPS_VERSION: LazyLock<Version> = LazyLock::new(now);

pub fn router() -> Router<Arc<Node>> {
    let list = |items: &'static [&'static str]| get(move || async move { Json(items) });
    Router::new()
        .route(
            "/x-nmos",
            list(&["node/", "connection/", "channelmapping/"]),
        )
        .route("/x-nmos/node", list(&["v1.3/"]))
        .route(
            "/x-nmos/node/v1.3",
            list(&[
                "self/",
                "sources/",
                "flows/",
                "devices/",
                "senders/",
                "receivers/",
            ]),
        )
        .route("/x-nmos/node/v1.3/self", get(node_self))
        .route("/x-nmos/node/v1.3/{kind}", get(resources))
        .route("/x-nmos/node/v1.3/{kind}/{id}", get(resource))
        .route("/x-nmos/node/v1.3/receivers/{id}/target", put(target))
}

/// Keeps the resources and the announced change counters current.
pub async fn refresh_loop(node: Arc<Node>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let node = node.clone();
        let _ = tokio::task::spawn_blocking(move || refresh(&node)).await;
    }
}

/// The current TAI time, for resource versions and activations.
pub fn now() -> Version {
    Version::from_tai_nanos(Timescale::tai().now())
}

/// Rebuilds the resources; if any changed, announces the new counters.
pub fn refresh(node: &Node) -> Snapshot {
    let fresh = snapshot(node);
    let now = now();
    let mut store = node.nmos.lock().unwrap();
    let changed = store.update(fresh, now);
    if changed {
        tracing::debug!(counters = ?store.counters(), "NMOS resources changed");
    }
    if changed
        && let Some(d) = &node.discovery
        && let Err(e) = d.set_counters(store.counters())
    {
        tracing::warn!(error = %e, "announcing resource changes");
    }
    store.snapshot().expect("just updated").clone()
}

/// An NMOS error response.
pub fn error(code: StatusCode, msg: String) -> Response {
    let body = crate::api::ErrorBody {
        code: code.as_u16(),
        error: msg,
        debug: None,
    };
    (code, Json(body)).into_response()
}

async fn node_self(State(node): State<Arc<Node>>) -> Json<is04::Node> {
    Json(refresh(&node).node)
}

/// All resources of one type, or the one with `id`.
fn respond<T: Resource>(items: Vec<T>, kind: &str, id: Option<&str>) -> Response {
    let Some(id) = id else {
        return Json(items).into_response();
    };
    let wanted = id.parse::<Uuid>().ok();
    match items.into_iter().find(|r| Some(r.base().id) == wanted) {
        Some(r) => Json(r).into_response(),
        None => error(StatusCode::NOT_FOUND, format!("no {kind} {id}")),
    }
}

fn by_kind(node: &Node, kind: &str, id: Option<&str>) -> Response {
    let s = refresh(node);
    match kind {
        "devices" => respond(s.devices, kind, id),
        "sources" => respond(s.sources, kind, id),
        "flows" => respond(s.flows, kind, id),
        "senders" => respond(s.senders, kind, id),
        "receivers" => respond(s.receivers, kind, id),
        _ => error(StatusCode::NOT_FOUND, format!("no resource type {kind}")),
    }
}

async fn resources(State(node): State<Arc<Node>>, Path(kind): Path<String>) -> Response {
    by_kind(&node, &kind, None)
}

async fn resource(
    State(node): State<Arc<Node>>,
    Path((kind, id)): Path<(String, String)>,
) -> Response {
    by_kind(&node, &kind, Some(&id))
}

/// IS-04's own connection management, deprecated in favour of IS-05.
async fn target(State(node): State<Arc<Node>>, Path(id): Path<String>) -> Response {
    let known = refresh(&node)
        .receivers
        .iter()
        .any(|r| r.base.id.to_string() == id);
    if known {
        error(
            StatusCode::NOT_IMPLEMENTED,
            "use IS-05 to connect receivers".into(),
        )
    } else {
        error(StatusCode::NOT_FOUND, format!("no receivers {id}"))
    }
}

/// The node's resources as they are now, with versions unset.
fn snapshot(node: &Node) -> Snapshot {
    let dev = node.id;
    let node_id = resource_id(dev, "node");
    let device_id = resource_id(dev, "device");
    let interfaces = interfaces();
    let names: Vec<String> = interfaces.iter().map(|i| i.name.clone()).collect();
    let endpoints = endpoints(node.listen);
    let href = endpoints
        .first()
        .map(|e| match e.host.parse::<IpAddr>() {
            Ok(IpAddr::V6(ip)) => format!("http://[{ip}]:{}/", e.port),
            _ => format!("http://{}:{}/", e.host, e.port),
        })
        .unwrap_or_else(|| {
            format!(
                "http://{}.local:{}/",
                crate::config::hostname(),
                node.listen.port()
            )
        });

    let clock = match node.clock() {
        (source, Some(p)) if source != ClockSource::System => Clock::Ptp {
            name: CLOCK.into(),
            traceable: false,
            version: "IEEE1588-2008",
            gmid: gmid(&p.grandmaster),
            locked: p.settled,
        },
        _ => Clock::Internal { name: CLOCK.into() },
    };

    let engine = node
        .engine
        .as_ref()
        .map(|e| e.lock().unwrap().status())
        .unwrap_or_default();
    let conns = node.connections.lock().unwrap();
    let receivers: Vec<(ReceiverId, ReceiverParams)> = conns
        .receiver_ids()
        .into_iter()
        .filter_map(|id| Some((id, conns.receiver_active(id)?.clone())))
        .collect();
    let sender_active: Vec<_> = engine
        .senders
        .iter()
        .map(|(n, _)| conns.sender_active(*n).cloned())
        .collect();
    drop(conns);
    let format = FORMAT;
    let media_type = media_type();
    let mut snap = Snapshot {
        node: is04::Node {
            base: Base {
                description: "Orpheus node".into(),
                ..Base::new(node_id, &node.name)
            },
            href: href.clone(),
            hostname: Some(crate::config::hostname()),
            api: Api {
                versions: vec![API_VERSION.into()],
                endpoints,
            },
            caps: Empty {},
            services: vec![],
            clocks: vec![clock],
            interfaces: interfaces
                .into_iter()
                .map(|i| Interface {
                    chassis_id: None,
                    port_id: mac(i.mac),
                    name: i.name,
                })
                .collect(),
        },
        devices: vec![],
        sources: vec![],
        flows: vec![],
        senders: vec![],
        receivers: vec![],
    };

    let view = crate::outputs::View::of(node);
    for (id, active) in receivers {
        let mut base = Base {
            description: format!("receiver {id}"),
            ..Base::new(resource_id(dev, &format!("receiver/{id}")), &node.name)
        };
        base.tags.insert(
            TAG_FORM_FACTOR.into(),
            vec![view.receiver_type(node, id).as_str().into()],
        );
        if node.health.lock().unwrap().receiver_jitter(id) {
            base.tags.insert(TAG_WARNINGS.into(), vec!["jitter".into()]);
        }
        if let Some(tag) = node.mixer.lock().unwrap().state(id).and_then(|s| s.tag()) {
            base.tags.insert(TAG_STATE.into(), vec![tag.into()]);
        }
        snap.receivers.push(Receiver {
            base,
            format: FORMAT_AUDIO,
            caps: ReceiverCaps {
                media_types: vec![media_type],
                // Receivers play exactly what the senders send.
                constraint_sets: vec![AudioConstraints::exactly(&stream())],
                version: *CAPS_VERSION,
            },
            device_id,
            transport: TRANSPORT_RTP,
            interface_bindings: names.clone(),
            subscription: ReceiverSubscription {
                // Parked receivers follow no sender.
                sender_id: active.sender_id.filter(|_| active.master_enable),
                active: active.master_enable,
            },
        });
    }

    for ((n, s), active) in engine.senders.iter().zip(sender_active) {
        let uuid = resource_id(dev, &format!("sender/{n}"));
        // The SDP file announces it too, so receivers know the route.
        let source_id = source_uuid(dev, *n);
        let flow_id = resource_id(dev, &format!("flow/{n}"));
        let base = |id| Base {
            description: format!("to {}", s.peer),
            ..Base::new(id, &s.name)
        };
        let channels = format.channels as usize;
        snap.sources.push(Source {
            base: base(source_id),
            format: FORMAT_AUDIO,
            caps: Empty {},
            device_id,
            parents: vec![],
            clock_name: Some(CLOCK.into()),
            channels: (0..channels)
                .map(|i| Channel {
                    label: channel_symbol(channels, i)
                        .map_or_else(|| format!("Channel {}", i + 1), str::to_owned),
                    symbol: channel_symbol(channels, i),
                })
                .collect(),
        });
        snap.flows.push(Flow {
            base: base(flow_id),
            format: FORMAT_AUDIO,
            source_id,
            device_id,
            parents: vec![],
            media_type,
            sample_rate: Rational {
                numerator: format.rate,
                denominator: 1,
            },
            bit_depth: stream().sample_depth,
        });
        snap.senders.push(Sender {
            base: base(uuid),
            flow_id: Some(flow_id),
            transport: TRANSPORT_RTP,
            device_id,
            manifest_href: Some(format!(
                "{href}x-nmos/connection/{}/single/senders/{uuid}/transportfile",
                is05::API_VERSION
            )),
            interface_bindings: route(s.peer)
                .and_then(|(_, i)| i)
                .map_or_else(|| names.clone(), |i| vec![i.name]),
            subscription: SenderSubscription {
                receiver_id: active
                    .as_ref()
                    .filter(|a| a.master_enable)
                    .and_then(|a| a.receiver_id),
                active: active.as_ref().is_none_or(|a| a.master_enable),
            },
        });
    }

    snap.devices.push(Device {
        base: Base {
            description: "Orpheus".into(),
            ..Base::new(device_id, &node.name)
        },
        kind: DEVICE_GENERIC,
        node_id,
        senders: snap.senders.iter().map(|s| s.base.id).collect(),
        receivers: snap.receivers.iter().map(|r| r.base.id).collect(),
        controls: vec![
            Control {
                href: format!("{href}x-nmos/connection/{}/", is05::API_VERSION),
                kind: CONTROL_CONNECTION,
            },
            Control {
                href: format!(
                    "{href}x-nmos/channelmapping/{}/",
                    crate::mapping::API_VERSION
                ),
                kind: crate::mapping::CONTROL,
            },
        ],
    });
    snap
}

/// The NMOS source of route `id`: what is sent, the same for as long as
/// the route is remembered.
pub fn source_uuid(device: DeviceId, id: SenderId) -> Uuid {
    resource_id(device, &format!("source/{id}"))
}

/// The SDP file of sender `id`, or `None` if there is no such sender.
pub fn sdp(node: &Node, id: SenderId) -> Option<String> {
    let snap = refresh(node);
    let (active, priority) = {
        let conns = node.connections.lock().unwrap();
        (conns.sender_active(id)?.clone(), conns.sender_priority(id))
    };
    let node_id = node.id.to_string();
    let source_id = source_uuid(node.id, id).to_string();
    let uuid = resource_id(node.id, &format!("sender/{id}"));
    let sender = snap.senders.iter().find(|s| s.base.id == uuid)?;
    let leg = active.transport_params.first()?;
    let destination = SocketAddr::new(leg.destination_ip.value()?, leg.destination_port.value()?);
    let source = leg.source_ip.value()?;
    let interface = interfaces().into_iter().find(|i| i.addrs.contains(&source));
    let clock = match node.clock() {
        (source, Some(p)) if source != ClockSource::System => RefClock::Ptp {
            gmid: gmid(&p.grandmaster),
            domain: p.domain,
        },
        _ => RefClock::LocalMac(mac(interface
            .or_else(|| interfaces().into_iter().next())
            .map_or([0; 6], |i| i.mac))),
    };
    let first = u64::from_be_bytes(uuid.as_bytes()[..8].try_into().unwrap());
    let session = sdp::Session {
        // NTP-style numbers fit 63 bits.
        id: first >> 1,
        // The clock lives in the node resource.
        version: sender.base.version.max(snap.node.base.version).tai_nanos(),
        // Names the sender for receivers, as AES67 devices do.
        name: &node.name,
        source,
        destination,
        payload_type: PAYLOAD_TYPE,
        encoding: ENCODING,
        format: FORMAT,
        ptime_ms: PTIME_MS,
        clock,
        // RTP timestamps count media-clock frames since the epoch.
        media_clock_offset: 0,
        node: Some(&node_id),
        source_id: Some(&source_id),
        priority,
    };
    Some(session.render())
}

/// `728e2c.fffe.867f85` → `72-8e-2c-ff-fe-86-7f-85`.
fn gmid(identity: &str) -> String {
    let hex: Vec<char> = identity.chars().filter(char::is_ascii_hexdigit).collect();
    let pairs: Vec<String> = hex
        .chunks(2)
        .map(|c| c.iter().collect::<String>().to_lowercase())
        .collect();
    pairs.join("-")
}

pub struct NetInterface {
    pub name: String,
    mac: [u8; 6],
    addrs: Vec<IpAddr>,
}

/// Network interfaces with a MAC address (no loopback, no tunnels), by
/// name.
fn interfaces() -> Vec<NetInterface> {
    let mut found: Vec<NetInterface> = timestamped_socket::interface::interfaces()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(name, data)| {
            let mac = data.mac().filter(|m| *m != [0; 6])?;
            Some(NetInterface {
                name: name.to_string(),
                mac,
                addrs: data.ips().collect(),
            })
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Where the API can be reached: the listen address, or if it listens on
/// all addresses, every interface's, IPv4 first. Link-local IPv6 is left
/// out, since it is useless without the interface.
fn endpoints(listen: SocketAddr) -> Vec<Endpoint> {
    let port = listen.port();
    let ips: Vec<IpAddr> = if listen.ip().is_unspecified() {
        let mut ips: Vec<IpAddr> = interfaces()
            .into_iter()
            .flat_map(|i| i.addrs)
            .filter(|ip| match ip {
                IpAddr::V4(_) => true,
                IpAddr::V6(v6) => !v6.is_unicast_link_local(),
            })
            .collect();
        ips.sort_by_key(IpAddr::is_ipv6);
        ips
    } else {
        vec![listen.ip()]
    };
    ips.into_iter()
        .map(|ip| Endpoint {
            host: ip.to_string(),
            port,
            protocol: "http",
        })
        .collect()
}

/// Names of this node's interfaces (with a MAC address).
pub fn interface_names() -> Vec<String> {
    interfaces().into_iter().map(|i| i.name).collect()
}

/// Addresses of this node's interfaces.
pub fn local_ips() -> Vec<IpAddr> {
    interfaces().into_iter().flat_map(|i| i.addrs).collect()
}

/// The address and interface the kernel would send to `peer` from.
pub fn route(peer: SocketAddr) -> Option<(IpAddr, Option<NetInterface>)> {
    let bind: SocketAddr = match peer {
        SocketAddr::V4(_) => ([0, 0, 0, 0], 0).into(),
        SocketAddr::V6(_) => ([0u16; 8], 0).into(),
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect(peer).ok()?;
    let local = socket.local_addr().ok()?.ip();
    let interface = interfaces().into_iter().find(|i| i.addrs.contains(&local));
    Some((local, interface))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grandmaster_ids() {
        assert_eq!(gmid("728e2c.fffe.867f85"), "72-8e-2c-ff-fe-86-7f-85");
    }
}
