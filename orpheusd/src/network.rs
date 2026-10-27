//! Which network links are wireless.
//!
//! Wi-Fi delays and reorders packets by milliseconds, sends multicast at its
//! slowest rate and holds it back for sleeping clients. Streams and PTP
//! work best wired, so the node warns about Wi-Fi on its own paths and
//! announces its own link, so that its peers can warn too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

use orpheus_clock::ptp::PtpTransport;
use orpheus_discovery::Link;

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

/// Where the other Orpheus nodes are for PTP: one address each, of
/// `transport`, a global IPv6 address before a link-local one. PTP asks
/// those whose multicast doesn't arrive for Announce by unicast.
pub fn ptp_peers(transport: PtpTransport, nodes: &[orpheus_discovery::Node]) -> Vec<IpAddr> {
    nodes
        .iter()
        .filter(|n| n.online && n.id.is_some())
        .filter_map(|n| {
            let ips = n
                .addresses
                .iter()
                .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok());
            match transport {
                // Not resolved yet: PTP isn't running.
                PtpTransport::Auto => None,
                PtpTransport::Ipv4 => ips.into_iter().find(IpAddr::is_ipv4),
                PtpTransport::Ipv6 => {
                    let v6: Vec<IpAddr> = ips.filter(IpAddr::is_ipv6).collect();
                    v6.iter()
                        .find(|ip| matches!(ip, IpAddr::V6(v) if !v.is_unicast_link_local()))
                        .or(v6.first())
                        .copied()
                }
            }
        })
        .collect()
}

/// Whether the Orpheus node at `address`, as PTP gives its master's,
/// announces that it is on Wi-Fi.
pub fn announces_wifi(address: &str, nodes: &[orpheus_discovery::Node]) -> bool {
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    nodes.iter().any(|n| {
        n.online
            && n.link == Some(Link::Wifi)
            && n.addresses
                .iter()
                .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok())
                .any(|ip| ip.to_canonical() == address.to_canonical())
    })
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
    fn ptp_peers_one_address_each() {
        let nodes = [
            node("WIRED", &["10.0.0.2", "fe80::2%eth0"], Some(Link::Wired)),
            node(
                "WIFI",
                &["10.0.0.3", "2001:db8::3", "fe80::3%wlan0"],
                Some(Link::Wifi),
            ),
            node("LOCAL", &["fe80::4%eth0"], None),
        ];
        let ip = |a: &str| a.parse::<IpAddr>().unwrap();
        assert_eq!(
            ptp_peers(PtpTransport::Ipv4, &nodes),
            [ip("10.0.0.2"), ip("10.0.0.3")]
        );
        // A global IPv6 address over a link-local one.
        assert_eq!(
            ptp_peers(PtpTransport::Ipv6, &nodes),
            [ip("fe80::2"), ip("2001:db8::3"), ip("fe80::4")]
        );
        // Not to nodes that went offline, nor to other NMOS nodes.
        let mut gone = nodes.clone();
        gone[0].online = false;
        gone[1].id = None;
        assert!(ptp_peers(PtpTransport::Ipv4, &gone).is_empty());
    }

    #[test]
    fn master_on_wifi() {
        let nodes = [
            node("WIRED", &["10.0.0.2", "fe80::2%eth0"], Some(Link::Wired)),
            node("WIFI", &["10.0.0.3", "fe80::3%wlan0"], Some(Link::Wifi)),
        ];
        assert!(announces_wifi("10.0.0.3", &nodes));
        assert!(announces_wifi("fe80::3", &nodes));
        assert!(!announces_wifi("10.0.0.2", &nodes));
        // Not an Orpheus node, or gone.
        assert!(!announces_wifi("10.0.0.9", &nodes));
        let mut gone = nodes.clone();
        gone[1].online = false;
        assert!(!announces_wifi("10.0.0.3", &gone));
    }
}
