//! SDP files (RFC 8866) describing a sender's RTP stream the way AES67 and
//! SMPTE ST 2110-30 receivers expect.

use std::fmt::Write;
use std::net::{IpAddr, SocketAddr};

use orpheus_core::AudioFormat;
use orpheus_core::rtp::Encoding;

/// What timestamps the stream (RFC 7273).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefClock {
    /// A PTP grandmaster, `00-11-22-ff-fe-33-44-55`, in a domain.
    Ptp { gmid: String, domain: u8 },
    /// The sender's own clock, named by the MAC address of its interface.
    LocalMac(String),
}

/// One RTP stream from `source` to `destination`.
#[derive(Debug, Clone)]
pub struct Session<'a> {
    /// Stays the same for the sender's lifetime.
    pub id: u64,
    /// Grows whenever the description changes.
    pub version: u64,
    pub name: &'a str,
    pub source: IpAddr,
    pub destination: SocketAddr,
    pub payload_type: u8,
    pub encoding: Encoding,
    pub format: AudioFormat,
    pub ptime_ms: u32,
    pub clock: RefClock,
    /// The RTP timestamp at the clock's epoch; Orpheus counts from 0.
    pub media_clock_offset: u32,
}

impl Session<'_> {
    pub fn render(&self) -> String {
        let mut s = String::new();
        let src = self.source;
        let dst = self.destination.ip();
        let pt = self.payload_type;
        let f = self.format;
        // `write!` into a `String` cannot fail.
        let _ = write!(
            s,
            "v=0\r\n\
             o=- {} {} IN {} {src}\r\n\
             s={}\r\n\
             c=IN {} {dst}\r\n\
             t=0 0\r\n\
             m=audio {} RTP/AVP {pt}\r\n\
             a=rtpmap:{pt} {}/{}/{}\r\n",
            self.id,
            self.version,
            family(src),
            one_line(self.name),
            family(dst),
            self.destination.port(),
            self.encoding.name(),
            f.rate,
            f.channels,
        );
        if let Some(order) = channel_order(f.channels) {
            let _ = write!(s, "a=fmtp:{pt} channel-order=SMPTE2110.({order})\r\n");
        }
        let _ = write!(
            s,
            "a=source-filter: incl IN {} {dst} {src}\r\n\
             a=sendonly\r\n\
             a=ptime:{}\r\n",
            family(dst),
            self.ptime_ms,
        );
        let _ = match &self.clock {
            RefClock::Ptp { gmid, domain } => write!(
                s,
                "a=ts-refclk:ptp=IEEE1588-2008:{}:{domain}\r\n",
                gmid.to_uppercase()
            ),
            RefClock::LocalMac(mac) => {
                write!(s, "a=ts-refclk:localmac={}\r\n", mac.to_uppercase())
            }
        };
        let _ = write!(s, "a=mediaclk:direct={}\r\n", self.media_clock_offset);
        s
    }
}

fn family(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(_) => "IP4",
        IpAddr::V6(_) => "IP6",
    }
}

/// The SMPTE ST 2110-30 channel grouping, for layouts that have one.
fn channel_order(channels: u16) -> Option<&'static str> {
    match channels {
        1 => Some("M"),
        2 => Some("ST"),
        _ => None,
    }
}

/// SDP values end at a line break.
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(clock: RefClock) -> Session<'static> {
        Session {
            id: 42,
            version: 7,
            name: "Living room",
            source: "192.168.1.10".parse().unwrap(),
            destination: "192.168.1.20:5004".parse().unwrap(),
            payload_type: 97,
            encoding: Encoding::L24,
            format: AudioFormat::AES67,
            ptime_ms: 1,
            clock,
            media_clock_offset: 0,
        }
    }

    #[test]
    fn aes67_with_ptp() {
        let sdp = session(RefClock::Ptp {
            gmid: "72-8e-2c-ff-fe-86-7f-85".into(),
            domain: 0,
        })
        .render();
        assert_eq!(
            sdp,
            "v=0\r\n\
             o=- 42 7 IN IP4 192.168.1.10\r\n\
             s=Living room\r\n\
             c=IN IP4 192.168.1.20\r\n\
             t=0 0\r\n\
             m=audio 5004 RTP/AVP 97\r\n\
             a=rtpmap:97 L24/48000/2\r\n\
             a=fmtp:97 channel-order=SMPTE2110.(ST)\r\n\
             a=source-filter: incl IN IP4 192.168.1.20 192.168.1.10\r\n\
             a=sendonly\r\n\
             a=ptime:1\r\n\
             a=ts-refclk:ptp=IEEE1588-2008:72-8E-2C-FF-FE-86-7F-85:0\r\n\
             a=mediaclk:direct=0\r\n"
        );
    }

    #[test]
    fn ipv6_and_local_clock() {
        let mut s = session(RefClock::LocalMac("00-1b-21-ab-cd-ef".into()));
        s.source = "fd00::10".parse().unwrap();
        s.destination = "[fd00::20]:5004".parse().unwrap();
        s.name = "Bad\r\nname";
        let sdp = s.render();
        assert!(sdp.contains("o=- 42 7 IN IP6 fd00::10\r\n"));
        assert!(sdp.contains("s=Bad  name\r\n"));
        assert!(sdp.contains("c=IN IP6 fd00::20\r\n"));
        assert!(sdp.contains("a=source-filter: incl IN IP6 fd00::20 fd00::10\r\n"));
        assert!(sdp.contains("a=ts-refclk:localmac=00-1B-21-AB-CD-EF\r\n"));
    }
}
