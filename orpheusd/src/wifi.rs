//! Wi-Fi interfaces through nl80211, the kernel's interface to every Wi-Fi
//! driver, whatever manages the network: power saving, and the signal and
//! rate of the link to the access point.
//!
//! Reading needs no privileges. Turning power saving off needs
//! `CAP_NET_ADMIN`.

use std::io;
use std::path::Path;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

/// Whether power saving is on for the wireless `interface`
/// (`iw dev <if> get power_save`); `None` if it can't tell. A dozing client
/// gets its packets only when it wakes, every 100 ms or so, which no buffer
/// or clock filter makes up for.
pub fn power_save(interface: &str) -> Option<bool> {
    let index = ifindex(interface).ok()?;
    let reply = Nl80211::open()
        .ok()?
        .request(
            NL80211_CMD_GET_POWER_SAVE,
            0,
            &[(NL80211_ATTR_IFINDEX, &index.to_ne_bytes())],
        )
        .ok()?;
    let state = attribute(reply.first()?, NL80211_ATTR_PS_STATE)?;
    Some(u32::from_ne_bytes(state.try_into().ok()?) != 0)
}

/// Turns power saving off for the wireless `interface` (`iw dev <if> set
/// power_save off`), until something turns it on again: NetworkManager
/// when it connects, TLP when the laptop goes on battery. Fails with
/// [`io::ErrorKind::PermissionDenied`] without `CAP_NET_ADMIN`, and with
/// [`io::ErrorKind::Unsupported`] where the driver can't.
pub fn power_save_off(interface: &str) -> io::Result<()> {
    let index = ifindex(interface)?;
    Nl80211::open()?.request(
        NL80211_CMD_SET_POWER_SAVE,
        0,
        &[
            (NL80211_ATTR_IFINDEX, &index.to_ne_bytes()),
            (NL80211_ATTR_PS_STATE, &NL80211_PS_DISABLED.to_ne_bytes()),
        ],
    )?;
    Ok(())
}

/// The link of a Wi-Fi client to its access point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Station {
    /// Signal strength, averaged where the driver does, in dBm. Drivers
    /// calibrate differently, by a few dB.
    pub signal_dbm: Option<i8>,
    /// The rate the client last sent at, in Mbit/s.
    pub tx_mbit: Option<f64>,
}

impl Station {
    /// Below this the access point sends slowly and the client looks for
    /// a better one (NetworkManager scans every 30 s below -65 dBm).
    pub const WEAK_DBM: i8 = -70;

    pub fn weak(&self) -> bool {
        self.signal_dbm.is_some_and(|s| s < Self::WEAK_DBM)
    }
}

/// The link of the wireless `interface` to its access point
/// (`iw dev <if> link`); `None` if it is not connected or can't tell.
pub fn station(interface: &str) -> Option<Station> {
    let index = ifindex(interface).ok()?;
    let replies = Nl80211::open()
        .ok()?
        .request(
            NL80211_CMD_GET_STATION,
            NLM_F_DUMP,
            &[(NL80211_ATTR_IFINDEX, &index.to_ne_bytes())],
        )
        .ok()?;
    // A client has one station, its access point.
    let info = attribute(replies.first()?, NL80211_ATTR_STA_INFO)?;
    let signal = attribute(info, NL80211_STA_INFO_SIGNAL_AVG)
        .or_else(|| attribute(info, NL80211_STA_INFO_SIGNAL))
        .and_then(|v| v.first())
        .map(|&b| b as i8);
    let tx_mbit = attribute(info, NL80211_STA_INFO_TX_BITRATE).and_then(|rate| {
        // In units of 100 kbit/s; the 16-bit value overflows above 6.5 Gbit/s.
        let units = match attribute(rate, NL80211_RATE_INFO_BITRATE32) {
            Some(v) => u32::from_ne_bytes(v.try_into().ok()?),
            None => u16::from_ne_bytes(
                attribute(rate, NL80211_RATE_INFO_BITRATE)?
                    .try_into()
                    .ok()?,
            ) as u32,
        };
        Some(units as f64 / 10.0)
    });
    Some(Station {
        signal_dbm: signal,
        tx_mbit,
    })
}

fn ifindex(interface: &str) -> io::Result<u32> {
    std::fs::read_to_string(Path::new("/sys/class/net").join(interface).join("ifindex"))?
        .trim()
        .parse()
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))
}

const AF_NETLINK: i32 = 16;
const NETLINK_GENERIC: i32 = 16;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const EOPNOTSUPP: i32 = 95;
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NL80211_CMD_GET_STATION: u8 = 17;
const NL80211_CMD_SET_POWER_SAVE: u8 = 61;
const NL80211_CMD_GET_POWER_SAVE: u8 = 62;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_STA_INFO: u16 = 21;
const NL80211_ATTR_PS_STATE: u16 = 93;
const NL80211_PS_DISABLED: u32 = 0;
const NL80211_STA_INFO_SIGNAL: u16 = 7;
const NL80211_STA_INFO_TX_BITRATE: u16 = 8;
const NL80211_STA_INFO_SIGNAL_AVG: u16 = 13;
const NL80211_RATE_INFO_BITRATE: u16 = 1;
const NL80211_RATE_INFO_BITRATE32: u16 = 5;
/// The netlink and generic netlink headers before a message's attributes.
const HEADERS: usize = 16 + 4;

/// A generic netlink socket with nl80211's family ID.
struct Nl80211 {
    socket: Socket,
    family: u16,
}

impl Nl80211 {
    fn open() -> io::Result<Self> {
        let socket = Socket::new(
            Domain::from(AF_NETLINK),
            Type::DGRAM,
            Some(Protocol::from(NETLINK_GENERIC)),
        )?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut ctrl = Self {
            socket,
            family: GENL_ID_CTRL,
        };
        let reply = ctrl.request(
            CTRL_CMD_GETFAMILY,
            0,
            &[(CTRL_ATTR_FAMILY_NAME, b"nl80211\0")],
        )?;
        let id = reply
            .first()
            .and_then(|r| attribute(r, CTRL_ATTR_FAMILY_ID))
            .and_then(|v| v.try_into().ok())
            .ok_or(io::ErrorKind::Unsupported)?;
        ctrl.family = u16::from_ne_bytes(id);
        Ok(ctrl)
    }

    /// Sends a request with `attributes`; the attributes of each message of
    /// the reply. A dump (`NLM_F_DUMP` in `flags`) ends with `NLMSG_DONE`,
    /// anything else with the acknowledgement, which carries the error.
    fn request(
        &self,
        command: u8,
        flags: u16,
        attributes: &[(u16, &[u8])],
    ) -> io::Result<Vec<Vec<u8>>> {
        let flags = NLM_F_REQUEST
            | if flags & NLM_F_DUMP != 0 {
                flags
            } else {
                flags | NLM_F_ACK
            };
        let mut m = Vec::new();
        m.extend(0u32.to_ne_bytes()); // length, below
        m.extend(self.family.to_ne_bytes());
        m.extend(flags.to_ne_bytes());
        m.extend(1u32.to_ne_bytes()); // sequence
        m.extend(0u32.to_ne_bytes()); // port: the kernel picks it
        m.extend([command, 1, 0, 0]);
        for (kind, value) in attributes {
            m.extend(((4 + value.len()) as u16).to_ne_bytes());
            m.extend(kind.to_ne_bytes());
            m.extend(*value);
            m.resize(m.len().next_multiple_of(4), 0);
        }
        let len = m.len() as u32;
        m[..4].copy_from_slice(&len.to_ne_bytes());
        self.socket.send(&m)?;

        let mut replies = vec![];
        let mut buf = vec![0; 32 * 1024];
        loop {
            let n = io::Read::read(&mut &self.socket, &mut buf)?;
            let mut rest = &buf[..n];
            while rest.len() >= HEADERS {
                let len = u32::from_ne_bytes(rest[..4].try_into().unwrap()) as usize;
                let kind = u16::from_ne_bytes(rest[4..6].try_into().unwrap());
                if len < 16 || len > rest.len() {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                match kind {
                    NLMSG_DONE => return Ok(replies),
                    NLMSG_ERROR => {
                        let errno = i32::from_ne_bytes(rest[16..20].try_into().unwrap());
                        return match -errno {
                            0 => Ok(replies),
                            EOPNOTSUPP => Err(io::ErrorKind::Unsupported.into()),
                            e => Err(io::Error::from_raw_os_error(e)),
                        };
                    }
                    _ => replies.push(rest[HEADERS..len].to_vec()),
                }
                rest = &rest[len.next_multiple_of(4).min(rest.len())..];
            }
        }
    }
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
    fn nothing_without_wifi() {
        assert_eq!(power_save("lo"), None);
        assert_eq!(power_save("no-such-interface"), None);
        assert_eq!(station("lo"), None);
        assert_eq!(
            power_save_off("no-such-interface").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    fn wifi_interfaces() -> Vec<String> {
        std::fs::read_dir("/sys/class/net")
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|i| {
                Path::new("/sys/class/net")
                    .join(i)
                    .join("phy80211")
                    .exists()
            })
            .collect()
    }

    /// nl80211 answers for every Wi-Fi interface of this machine, if any.
    #[test]
    fn reads_wifi() {
        for i in wifi_interfaces() {
            assert!(power_save(&i).is_some(), "{i}");
            // Connected or not, the dump must not fail.
            let _ = station(&i);
        }
    }
}
