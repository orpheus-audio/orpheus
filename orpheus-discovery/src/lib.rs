//! mDNS/DNS-SD discovery for Orpheus nodes.
//!
//! Every node announces its HTTP API as an NMOS node (`_nmos-node._tcp`,
//! IS-04 peer-to-peer discovery) and browses for the others. NMOS nodes
//! that are not Orpheus nodes show up too, without an Orpheus ID.
//!
//! TXT keys: the IS-04 ones (`api_proto`, `api_ver`, `api_auth` and the
//! `ver_*` resource counters), plus `x-orpheus-id`, `x-orpheus-ver` (the
//! orpheusd version) and `x-orpheus-link` (`wired` or `wifi`, the node's
//! own network link, so that peers can warn about it). The name is the instance name, so TXT stays
//! ASCII, which some NMOS tools expect.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mdns_sd::{DaemonEvent, IfKind, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use orpheus_core::DeviceId;
use orpheus_nmos::Counters;

pub use mdns_sd::Error;

/// DNS-SD service type of an NMOS node API.
pub const SERVICE_TYPE: &str = "_nmos-node._tcp.local.";

/// Longest DNS label is 63 bytes; leave room for a conflict suffix " (2)".
const MAX_INSTANCE: usize = 56;

/// What this node announces.
#[derive(Debug, Clone)]
pub struct Announcement {
    pub id: DeviceId,
    pub name: String,
    /// TCP port of the HTTP API.
    pub port: u16,
    /// orpheusd version.
    pub version: String,
}

/// What kind of network link a node has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum Link {
    Wired,
    Wifi,
}

impl Link {
    fn as_str(self) -> &'static str {
        match self {
            Link::Wired => "wired",
            Link::Wifi => "wifi",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "wired" => Some(Link::Wired),
            "wifi" => Some(Link::Wifi),
            _ => None,
        }
    }
}

/// A node seen on the network.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Node {
    /// Orpheus device ID; absent for other NMOS nodes.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    #[cfg_attr(feature = "openapi", schema(example = "B73AQ0P0"))]
    pub id: Option<String>,
    /// Announced name; `Name (2)` if another node had the name first.
    pub name: String,
    /// orpheusd version; absent for other NMOS nodes.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub version: Option<String>,
    /// mDNS host name.
    #[cfg_attr(feature = "openapi", schema(example = "orpheus-b73aq0p0.local."))]
    pub host: String,
    /// Addresses, IPv4 first, link-local IPv6 last (with `%interface`).
    #[cfg_attr(feature = "openapi", schema(example = json!(["192.168.1.20", "fe80::1%eth0"])))]
    pub addresses: Vec<String>,
    /// TCP port of the node's HTTP API.
    pub port: u16,
    /// False once the node said goodbye or its announcement expired.
    pub online: bool,
    /// The node's receiver change counter (`ver_rcv`): it changes whenever
    /// one of its receivers does, for example what it plays or on what.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub receivers: Option<u8>,
    /// The node's own network link, as it announces it.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub link: Option<Link>,
}

/// Announces this node and keeps a list of the others. Says goodbye on drop.
pub struct Discovery {
    daemon: ServiceDaemon,
    me: Announcement,
    /// Our instance's full name; changes if mDNS renames it after a conflict.
    fullname: Arc<Mutex<String>>,
    /// What the TXT record says beyond the fixed keys.
    announced: Mutex<(Counters, Option<Link>)>,
    registry: Arc<Mutex<Registry>>,
}

impl Discovery {
    pub fn start(me: &Announcement) -> Result<Self, Error> {
        let daemon = ServiceDaemon::new()?;
        // Another node on the same host is in another network namespace, so
        // loopback only ever finds ourselves.
        daemon.disable_interface(vec![IfKind::LoopbackV4, IfKind::LoopbackV6])?;

        let info = service_info(me, instance_name(&me.name), Counters::default(), None)?;
        let fullname = Arc::new(Mutex::new(info.get_fullname().to_owned()));
        daemon.register(info)?;

        let events = daemon.monitor()?;
        let ours = fullname.clone();
        std::thread::Builder::new()
            .name("mdns-monitor".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    match event {
                        DaemonEvent::NameChange(change) => {
                            let mut ours = ours.lock().unwrap();
                            if *ours == change.original {
                                tracing::warn!(name = %change.new_name, "mDNS name taken; renamed");
                                *ours = change.new_name;
                            }
                        }
                        DaemonEvent::Error(e) => tracing::warn!(error = %e, "mDNS"),
                        _ => {}
                    }
                }
            })
            .expect("spawning mDNS monitor thread");

        let registry = Arc::new(Mutex::new(Registry::new(me.id.to_string())));
        let events = daemon.browse(SERVICE_TYPE)?;
        let reg = registry.clone();
        std::thread::Builder::new()
            .name("mdns-browse".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(s) => {
                            reg.lock().unwrap().resolved(&s.fullname, node(&s));
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            reg.lock().unwrap().removed(&fullname);
                        }
                        _ => {}
                    }
                }
            })
            .expect("spawning mDNS browse thread");

        Ok(Self {
            daemon,
            me: me.clone(),
            fullname,
            announced: Mutex::new((Counters::default(), None)),
            registry,
        })
    }

    /// Announces new resource change counters, so that NMOS controllers
    /// know to fetch the resources again.
    pub fn set_counters(&self, counters: Counters) -> Result<(), Error> {
        let mut announced = self.announced.lock().unwrap();
        announced.0 = counters;
        self.announce(*announced)
    }

    /// Announces the node's own network link.
    pub fn set_link(&self, link: Option<Link>) -> Result<(), Error> {
        let mut announced = self.announced.lock().unwrap();
        if announced.1 == link {
            return Ok(());
        }
        announced.1 = link;
        self.announce(*announced)
    }

    fn announce(&self, (counters, link): (Counters, Option<Link>)) -> Result<(), Error> {
        let name = instance(&self.fullname.lock().unwrap());
        self.daemon
            .register(service_info(&self.me, &name, counters, link)?)
    }

    /// Other nodes, online first, then by name.
    pub fn nodes(&self) -> Vec<Node> {
        self.registry.lock().unwrap().nodes()
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let fullname = self.fullname.lock().unwrap().clone();
        if let Ok(done) = self.daemon.unregister(&fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
    }
}

fn service_info(
    me: &Announcement,
    instance: &str,
    c: Counters,
    link: Option<Link>,
) -> Result<ServiceInfo, Error> {
    let id = me.id.to_string();
    let ver = |n: u8| n.to_string();
    let mut txt: Vec<(&str, String)> = vec![
        ("api_proto", "http".into()),
        ("api_ver", "v1.3".into()),
        ("api_auth", "false".into()),
        ("ver_slf", ver(c.slf)),
        ("ver_src", ver(c.src)),
        ("ver_flw", ver(c.flw)),
        ("ver_dvc", ver(c.dvc)),
        ("ver_snd", ver(c.snd)),
        ("ver_rcv", ver(c.rcv)),
        ("x-orpheus-id", id.clone()),
        ("x-orpheus-ver", me.version.clone()),
    ];
    if let Some(link) = link {
        txt.push(("x-orpheus-link", link.as_str().into()));
    }
    let host = format!("orpheus-{}.local.", id.to_lowercase());
    Ok(ServiceInfo::new(SERVICE_TYPE, instance, &host, (), me.port, &txt[..])?.enable_addr_auto())
}

/// The DNS-SD instance name: the node name, cut to fit one DNS label.
fn instance_name(name: &str) -> &str {
    let mut end = name.len().min(MAX_INSTANCE);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn node(s: &ResolvedService) -> Node {
    let txt = |key: &str| s.get_property_val_str(key).map(str::to_owned);
    let mut addresses: Vec<(IpAddr, String)> = s
        .get_addresses()
        .iter()
        .map(|a| (a.to_ip_addr(), a.to_string()))
        .collect();
    addresses.sort_by_key(|(ip, text)| (address_rank(ip), text.clone()));
    let id = txt("x-orpheus-id")
        .and_then(|id| id.parse::<DeviceId>().ok())
        .map(|id| id.to_string());
    Node {
        name: instance(&s.fullname),
        id,
        version: txt("x-orpheus-ver"),
        host: s.host.clone(),
        addresses: addresses.into_iter().map(|(_, text)| text).collect(),
        port: s.port,
        online: true,
        receivers: txt("ver_rcv").and_then(|v| v.parse().ok()),
        link: txt("x-orpheus-link").and_then(|l| Link::parse(&l)),
    }
}

/// IPv4 first: every receiver listens on it, also AES67 devices. Link-local
/// IPv6 last: it needs the interface.
fn address_rank(ip: &IpAddr) -> u8 {
    match ip {
        IpAddr::V4(_) => 0,
        IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80 => 2,
        IpAddr::V6(_) => 1,
    }
}

/// The instance part of `Kitchen\.1._nmos-node._tcp.local.`, unescaped.
fn instance(fullname: &str) -> String {
    let escaped = fullname
        .strip_suffix(SERVICE_TYPE)
        .and_then(|s| s.strip_suffix('.'))
        .unwrap_or(fullname);
    let mut out = Vec::with_capacity(escaped.len());
    let bytes = escaped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let digits = bytes
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(u8::is_ascii_digit));
        match (bytes[i], digits) {
            (b'\\', Some(d)) => {
                let n = d.iter().fold(0u32, |n, b| n * 10 + u32::from(b - b'0'));
                out.push(n.min(255) as u8);
                i += 4;
            }
            (b'\\', None) if i + 1 < bytes.len() => {
                out.push(bytes[i + 1]);
                i += 2;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The nodes seen so far, by DNS-SD full name. Nodes that leave stay listed
/// as offline.
struct Registry {
    own_id: String,
    nodes: HashMap<String, Node>,
}

impl Registry {
    fn new(own_id: String) -> Self {
        Self {
            own_id,
            nodes: HashMap::new(),
        }
    }

    fn resolved(&mut self, fullname: &str, node: Node) {
        if node.id.as_deref() == Some(&self.own_id) {
            return;
        }
        // An Orpheus node that was renamed comes back under a new full name.
        if node.id.is_some() {
            self.nodes.retain(|f, n| f == fullname || n.id != node.id);
        }
        let known = self.nodes.get(fullname);
        if known.is_none_or(|k| !k.online) {
            tracing::info!(name = %node.name, id = node.id.as_deref().unwrap_or("-"), addr = node.addresses.first().map(String::as_str).unwrap_or("-"), "node online");
        } else if known.is_some_and(|k| k.name != node.name) {
            tracing::info!(name = %node.name, was = %known.unwrap().name, "node renamed");
        }
        self.nodes.insert(fullname.to_owned(), node);
    }

    fn removed(&mut self, fullname: &str) {
        if let Some(node) = self.nodes.get_mut(fullname).filter(|n| n.online) {
            node.online = false;
            tracing::info!(name = %node.name, "node offline");
        }
    }

    fn nodes(&self) -> Vec<Node> {
        let mut nodes: Vec<Node> = self.nodes.values().cloned().collect();
        nodes.sort_by(|a, b| (!a.online, &a.name, &a.id).cmp(&(!b.online, &b.name, &b.id)));
        nodes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn orpheus(id: &str, name: &str) -> Node {
        Node {
            id: Some(id.into()),
            name: name.into(),
            version: Some("0.0.1".into()),
            host: format!("orpheus-{}.local.", id.to_lowercase()),
            addresses: vec!["10.0.0.1".into()],
            port: 7480,
            online: true,
            receivers: Some(0),
            link: None,
        }
    }

    #[test]
    fn instance_names() {
        assert_eq!(instance("Kitchen._nmos-node._tcp.local."), "Kitchen");
        assert_eq!(instance(r"Box\.1\\x._nmos-node._tcp.local."), r"Box.1\x");
        assert_eq!(instance(r"K\195\188che._nmos-node._tcp.local."), "Küche");
        assert_eq!(instance_name(&"ü".repeat(40)).len(), 56);
    }

    #[test]
    fn addresses_rank_ipv4_first() {
        let mut ips: Vec<IpAddr> = ["fe80::1", "fd00::1", "10.0.0.1"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        ips.sort_by_key(address_rank);
        assert_eq!(ips[0].to_string(), "10.0.0.1");
        assert_eq!(ips[2].to_string(), "fe80::1");
    }

    #[test]
    fn registry_tracks_nodes() {
        let mut r = Registry::new("AAAAAAAA".into());
        r.resolved("Self._nmos-node._tcp.local.", orpheus("AAAAAAAA", "Self"));
        r.resolved("B._nmos-node._tcp.local.", orpheus("BBBBBBBB", "B"));
        r.resolved("A._nmos-node._tcp.local.", orpheus("CCCCCCCC", "A"));
        let names: Vec<_> = r.nodes().into_iter().map(|n| n.name).collect();
        assert_eq!(names, ["A", "B"]);

        r.removed("A._nmos-node._tcp.local.");
        let nodes = r.nodes();
        assert_eq!(nodes[0].name, "B");
        assert!(!nodes[1].online);

        // Renamed: the old entry goes.
        r.resolved(
            "Kitchen._nmos-node._tcp.local.",
            orpheus("BBBBBBBB", "Kitchen"),
        );
        let names: Vec<_> = r.nodes().into_iter().map(|n| n.name).collect();
        assert_eq!(names, ["Kitchen", "A"]);
    }
}
