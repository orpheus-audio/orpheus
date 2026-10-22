//! Which network links are wireless.
//!
//! Wi-Fi delays and reorders packets by milliseconds, sends multicast at its
//! slowest rate and holds it back for sleeping clients. Streams and PTP
//! work best wired, so the node warns about Wi-Fi on its own paths and
//! announces its own link, so that its peers can warn too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use orpheus_clock::ptp::{PtpTransport, Unicast};
use orpheus_discovery::Link;
use socket2::{Domain, Protocol, Socket, Type};

use crate::nmos;

/// Whether `interface` is a wireless interface. For testing on one machine,
/// `ORPHEUS_DEBUG_WIRELESS` names interfaces to count as wireless,
/// separated by commas, or `*` for all.
pub fn is_wireless(interface: &str) -> bool {
    if let Ok(list) = std::env::var("ORPHEUS_DEBUG_WIRELESS") {
        return list
            .split(',')
            .any(|i| i.trim() == "*" || i.trim() == interface);
    }
    let dir = Path::new("/sys/class/net").join(interface);
    dir.join("wireless").exists() || dir.join("phy80211").exists()
}

/// Whether power saving is on for the wireless `interface`, as nl80211
/// reports it (`iw dev <if> get power_save`); `None` if it can't tell. A
/// dozing client gets its packets only when it wakes, every 100 ms or so,
/// which no buffer or clock filter makes up for.
pub fn power_save(interface: &str) -> Option<bool> {
    let index: u32 =
        std::fs::read_to_string(Path::new("/sys/class/net").join(interface).join("ifindex"))
            .ok()?
            .trim()
            .parse()
            .ok()?;
    let socket = Socket::new(
        Domain::from(AF_NETLINK),
        Type::DGRAM,
        Some(Protocol::from(NETLINK_GENERIC)),
    )
    .ok()?;
    socket
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .ok()?;
    let family = genl_request(
        &socket,
        GENL_ID_CTRL,
        CTRL_CMD_GETFAMILY,
        1,
        CTRL_ATTR_FAMILY_NAME,
        b"nl80211\0",
    )?;
    let family = u16::from_ne_bytes(attribute(&family, CTRL_ATTR_FAMILY_ID)?.try_into().ok()?);
    let reply = genl_request(
        &socket,
        family,
        NL80211_CMD_GET_POWER_SAVE,
        0,
        NL80211_ATTR_IFINDEX,
        &index.to_ne_bytes(),
    )?;
    let state = u32::from_ne_bytes(attribute(&reply, NL80211_ATTR_PS_STATE)?.try_into().ok()?);
    Some(state != 0)
}

const AF_NETLINK: i32 = 16;
const NETLINK_GENERIC: i32 = 16;
const NLM_F_REQUEST: u16 = 1;
const NLMSG_ERROR: u16 = 2;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NL80211_CMD_GET_POWER_SAVE: u8 = 62;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_PS_STATE: u16 = 93;
/// The netlink and generic netlink headers before a message's attributes.
const HEADERS: usize = 16 + 4;

/// Sends a generic netlink request with one attribute; the attributes of
/// the reply.
fn genl_request(
    socket: &Socket,
    family: u16,
    command: u8,
    version: u8,
    attr: u16,
    value: &[u8],
) -> Option<Vec<u8>> {
    let attr_len = 4 + value.len();
    let len = HEADERS + attr_len.next_multiple_of(4);
    let mut m = Vec::with_capacity(len);
    m.extend((len as u32).to_ne_bytes());
    m.extend(family.to_ne_bytes());
    m.extend(NLM_F_REQUEST.to_ne_bytes());
    m.extend(1u32.to_ne_bytes()); // sequence
    m.extend(0u32.to_ne_bytes()); // port: the kernel picks it
    m.extend([command, version, 0, 0]);
    m.extend((attr_len as u16).to_ne_bytes());
    m.extend(attr.to_ne_bytes());
    m.extend(value);
    m.resize(len, 0);
    socket.send(&m).ok()?;
    let mut reply = vec![0; 8192];
    let n = std::io::Read::read(&mut &*socket, &mut reply).ok()?;
    reply.truncate(n);
    let kind = u16::from_ne_bytes(reply.get(4..6)?.try_into().ok()?);
    let len = u32::from_ne_bytes(reply.get(..4)?.try_into().ok()?) as usize;
    if kind == NLMSG_ERROR || len < HEADERS || len > reply.len() {
        return None;
    }
    Some(reply[HEADERS..len].to_vec())
}

/// The value of attribute `kind` among netlink `attributes`.
fn attribute(attributes: &[u8], kind: u16) -> Option<&[u8]> {
    let mut rest = attributes;
    while rest.len() >= 4 {
        let len = u16::from_ne_bytes([rest[0], rest[1]]) as usize;
        // The top bits flag nesting and byte order.
        let k = u16::from_ne_bytes([rest[2], rest[3]]) & 0x3fff;
        if len < 4 || len > rest.len() {
            return None;
        }
        if k == kind {
            return Some(&rest[4..len]);
        }
        rest = &rest[len.next_multiple_of(4).min(rest.len())..];
    }
    None
}

/// Where PTP messages also go by unicast: to the other Orpheus nodes that
/// announce Wi-Fi, by one address of `transport`. Multicast reaches the
/// rest; from a node on Wi-Fi too, which sends it to the access point like
/// unicast. `ptp_wireless`: PTP runs over Wi-Fi here.
pub fn ptp_unicast(
    ptp_wireless: bool,
    transport: PtpTransport,
    nodes: &[orpheus_discovery::Node],
) -> Unicast {
    let peers = nodes
        .iter()
        .filter(|n| n.online && n.id.is_some() && n.link == Some(Link::Wifi))
        .filter_map(|n| {
            let ips = n
                .addresses
                .iter()
                .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok());
            match transport {
                // Not resolved yet: PTP isn't running.
                PtpTransport::Auto => None,
                PtpTransport::Ipv4 => ips.into_iter().find(IpAddr::is_ipv4),
                // A global address, else a link-local one.
                PtpTransport::Ipv6 => {
                    let v6: Vec<IpAddr> = ips.filter(IpAddr::is_ipv6).collect();
                    v6.iter()
                        .find(|ip| matches!(ip, IpAddr::V6(v) if !v.is_unicast_link_local()))
                        .or(v6.first())
                        .copied()
                }
            }
        })
        .collect();
    Unicast {
        peers,
        wireless: ptp_wireless,
    }
}

/// The interface the kernel would send to `ip` through.
pub fn interface_to(ip: IpAddr) -> Option<String> {
    nmos::route(SocketAddr::new(ip, 9))?.1.map(|i| i.name)
}

/// Whether traffic to `ip` leaves through a wireless interface.
pub fn wireless_to(ip: IpAddr) -> Option<String> {
    interface_to(ip).filter(|i| is_wireless(i))
}

/// The interface of the default route (IPv4, else IPv6).
pub fn default_interface() -> Option<String> {
    // Documentation addresses: never local, so they take the default route.
    [
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    ]
    .into_iter()
    .find_map(interface_to)
}

/// This node's own link: that of the default route (IPv4, else IPv6).
/// Without a default route, Wi-Fi if every interface is wireless.
pub fn own_link() -> Option<Link> {
    let link = |wireless| if wireless { Link::Wifi } else { Link::Wired };
    if let Some(interface) = default_interface() {
        return Some(link(is_wireless(&interface)));
    }
    let names = nmos::interface_names();
    (!names.is_empty()).then(|| link(names.iter().all(|i| is_wireless(i))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(kind: u16, value: &[u8]) -> Vec<u8> {
        let mut a = ((4 + value.len()) as u16).to_ne_bytes().to_vec();
        a.extend(kind.to_ne_bytes());
        a.extend(value);
        a.resize(a.len().next_multiple_of(4), 0);
        a
    }

    #[test]
    fn finds_attributes() {
        let mut attrs = attr(CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        attrs.extend(attr(3, &[1, 2, 3])); // padded to 8 bytes
        // Nested: the flag bit doesn't change the kind.
        attrs.extend(attr(NL80211_ATTR_PS_STATE | 0x8000, &1u32.to_ne_bytes()));
        assert_eq!(
            attribute(&attrs, CTRL_ATTR_FAMILY_NAME),
            Some(&b"nl80211\0"[..])
        );
        assert_eq!(attribute(&attrs, 3), Some(&[1, 2, 3][..]));
        assert_eq!(
            attribute(&attrs, NL80211_ATTR_PS_STATE),
            Some(&1u32.to_ne_bytes()[..])
        );
        assert_eq!(attribute(&attrs, 7), None);
        // A length beyond the end.
        assert_eq!(attribute(&[200, 0, 1, 0, 0, 0], 1), None);
    }

    #[test]
    fn no_power_save_without_wifi() {
        assert_eq!(power_save("lo"), None);
        assert_eq!(power_save("no-such-interface"), None);
    }

    fn node(id: &str, addresses: &[&str], link: Option<Link>) -> orpheus_discovery::Node {
        orpheus_discovery::Node {
            id: Some(id.into()),
            name: id.into(),
            version: None,
            host: format!("{id}.local."),
            addresses: addresses.iter().map(|a| a.to_string()).collect(),
            port: 7480,
            online: true,
            receivers: None,
            link,
        }
    }

    #[test]
    fn ptp_unicast_reaches_wifi() {
        let nodes = [
            node("WIRED", &["10.0.0.2", "fe80::2%eth0"], Some(Link::Wired)),
            node(
                "WIFI",
                &["10.0.0.3", "2001:db8::3", "fe80::3%wlan0"],
                Some(Link::Wifi),
            ),
            node("UNKNOWN", &["fe80::4%eth0"], None),
        ];
        let ip = |a: &str| a.parse::<IpAddr>().unwrap();
        // Only the peer on Wi-Fi, wherever this node is.
        for wireless in [false, true] {
            let u = ptp_unicast(wireless, PtpTransport::Ipv4, &nodes);
            assert_eq!(u.peers, [ip("10.0.0.3")]);
            assert_eq!(u.wireless, wireless);
        }
        // A global IPv6 address over a link-local one.
        let u = ptp_unicast(true, PtpTransport::Ipv6, &nodes);
        assert_eq!(u.peers, [ip("2001:db8::3")]);
        let mut local = nodes.clone();
        local[1].addresses = vec!["fe80::3%wlan0".into()];
        let u = ptp_unicast(true, PtpTransport::Ipv6, &local);
        assert_eq!(u.peers, [ip("fe80::3")]);
        // Not to nodes that went offline, nor to other NMOS nodes.
        let mut gone = nodes.clone();
        gone[1].online = false;
        assert!(
            ptp_unicast(true, PtpTransport::Ipv4, &gone)
                .peers
                .is_empty()
        );
        let mut other = nodes.clone();
        other[1].id = None;
        assert!(
            ptp_unicast(true, PtpTransport::Ipv4, &other)
                .peers
                .is_empty()
        );
    }

    /// nl80211 answers for every Wi-Fi interface of this machine, if any.
    #[test]
    fn reads_power_save() {
        for entry in std::fs::read_dir("/sys/class/net").unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            if Path::new("/sys/class/net")
                .join(&name)
                .join("phy80211")
                .exists()
            {
                assert!(power_save(&name).is_some(), "{name}");
            }
        }
    }
}
