//! Which network links are wireless.
//!
//! Wi-Fi delays and reorders packets by milliseconds, sends multicast at its
//! slowest rate and holds it back for sleeping clients. Streams and PTP
//! work best wired, so the node warns about Wi-Fi on its own paths and
//! announces its own link, so that its peers can warn too.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;

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

/// The interface the kernel would send to `ip` through.
pub fn interface_to(ip: IpAddr) -> Option<String> {
    nmos::route(SocketAddr::new(ip, 9))?.1.map(|i| i.name)
}

/// Whether traffic to `ip` leaves through a wireless interface.
pub fn wireless_to(ip: IpAddr) -> Option<String> {
    interface_to(ip).filter(|i| is_wireless(i))
}

/// This node's own link: that of the default route (IPv4, else IPv6).
/// Without a default route, Wi-Fi if every interface is wireless.
pub fn own_link() -> Option<Link> {
    // Documentation addresses: never local, so they take the default route.
    let default = [
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    ]
    .into_iter()
    .find_map(interface_to);
    let link = |wireless| if wireless { Link::Wifi } else { Link::Wired };
    if let Some(interface) = default {
        return Some(link(is_wireless(&interface)));
    }
    let names = nmos::interface_names();
    (!names.is_empty()).then(|| link(names.iter().all(|i| is_wireless(i))))
}
