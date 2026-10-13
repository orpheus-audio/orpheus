//! SDP files (RFC 8866) describing a sender's RTP stream the way AES67 and
//! SMPTE ST 2110-30 receivers expect, and reading the parts of other
//! senders' files that a receiver needs.

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
             t=0 0\r\n\
             m=audio {} RTP/AVP {pt}\r\n\
             c=IN {} {dst}{}\r\n\
             a=rtpmap:{pt} {}/{}/{}\r\n",
            self.id,
            self.version,
            family(src),
            one_line(self.name),
            self.destination.port(),
            family(dst),
            // IPv4 multicast addresses carry a TTL.
            if dst.is_ipv4() && dst.is_multicast() {
                "/32"
            } else {
                ""
            },
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

/// What a receiver needs from an SDP file: the first audio stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// Where packets come from: the source filter, else the origin.
    pub source: Option<IpAddr>,
    /// The connection address: unicast to the receiver, or a group.
    pub destination: IpAddr,
    pub port: u16,
    pub payload_type: u8,
    /// `L24`, `L16`, …
    pub encoding: String,
    pub rate: u32,
    pub channels: u16,
}

/// Reads the first `m=audio` section with RTP/AVP.
pub fn parse(sdp: &str) -> Result<Stream, String> {
    let mut origin = None;
    let mut session_c = None;
    // Inside the first audio section: connection, port, payload type.
    let mut media: Option<(Option<IpAddr>, u16, u8)> = None;
    let mut rtpmap: Option<(String, u32, u16)> = None;
    let mut filter = None;
    let mut done = false;
    for line in sdp.lines() {
        let line = line.trim_end_matches('\r');
        let Some((kind, value)) = line.split_once('=') else {
            continue;
        };
        match kind {
            "m" if media.is_some() => done = true,
            "m" => {
                let f: Vec<&str> = value.split_whitespace().collect();
                if let ["audio", port, "RTP/AVP", pt, ..] = f[..] {
                    let port = port.split('/').next().unwrap_or(port);
                    let port = port.parse().map_err(|_| format!("bad port in {line:?}"))?;
                    let pt = pt
                        .parse()
                        .map_err(|_| format!("bad payload type in {line:?}"))?;
                    media = Some((None, port, pt));
                }
            }
            _ if done => {}
            "o" => origin = value.split_whitespace().nth(5).and_then(|a| a.parse().ok()),
            "c" => {
                let addr = connection(value).ok_or_else(|| format!("bad connection {line:?}"))?;
                match &mut media {
                    Some((c, ..)) => *c = Some(addr),
                    None => session_c = Some(addr),
                }
            }
            "a" if media.is_some() => {
                if let Some(map) = value.strip_prefix("rtpmap:") {
                    let (pt, enc) = map.split_once(' ').unwrap_or((map, ""));
                    if pt.parse() == Ok(media.unwrap().2) {
                        let mut p = enc.split('/');
                        let name = p.next().unwrap_or_default().to_owned();
                        let rate = p.next().and_then(|r| r.parse().ok());
                        let channels = p.next().map_or(Some(1), |c| c.parse().ok());
                        let (Some(rate), Some(channels)) = (rate, channels) else {
                            return Err(format!("bad rtpmap {line:?}"));
                        };
                        rtpmap = Some((name, rate, channels));
                    }
                } else if let Some(f) = value.strip_prefix("source-filter:") {
                    let f: Vec<&str> = f.split_whitespace().collect();
                    if let ["incl", "IN", _, _, src, ..] = f[..] {
                        filter = src.parse().ok();
                    }
                }
            }
            _ => {}
        }
    }
    let (c, port, payload_type) = media.ok_or("no RTP audio stream")?;
    let destination = c.or(session_c).ok_or("no connection address")?;
    let (encoding, rate, channels) = rtpmap.ok_or("no rtpmap for the payload type")?;
    Ok(Stream {
        source: filter.or(origin).filter(|ip: &IpAddr| !ip.is_unspecified()),
        destination,
        port,
        payload_type,
        encoding,
        rate,
        channels,
    })
}

/// `IN IP4 239.1.2.3/32` → the address.
fn connection(value: &str) -> Option<IpAddr> {
    let f: Vec<&str> = value.split_whitespace().collect();
    match f[..] {
        ["IN", "IP4" | "IP6", addr] => addr.split('/').next()?.parse().ok(),
        _ => None,
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
             t=0 0\r\n\
             m=audio 5004 RTP/AVP 97\r\n\
             c=IN IP4 192.168.1.20\r\n\
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
    fn reads_own_files() {
        let sdp = session(RefClock::LocalMac("00-1b-21-ab-cd-ef".into())).render();
        assert_eq!(
            parse(&sdp),
            Ok(Stream {
                source: Some("192.168.1.10".parse().unwrap()),
                destination: "192.168.1.20".parse().unwrap(),
                port: 5004,
                payload_type: 97,
                encoding: "L24".into(),
                rate: 48_000,
                channels: 2,
            })
        );
    }

    #[test]
    fn reads_other_files() {
        // AES67 style, multicast, origin only, session-level connection.
        let sdp = "v=0\no=- 1 1 IN IP4 10.0.0.9\ns=x\nc=IN IP4 239.1.2.3/32\nt=0 0\n\
                   m=video 5000 RTP/AVP 96\na=rtpmap:96 raw/90000\n\
                   m=audio 5006 RTP/AVP 96\na=rtpmap:96 L16/44100\n";
        let s = parse(sdp).unwrap();
        assert_eq!(s.source, Some("10.0.0.9".parse().unwrap()));
        assert_eq!(s.destination, "239.1.2.3".parse::<IpAddr>().unwrap());
        assert_eq!((s.port, s.payload_type), (5006, 96));
        assert_eq!(
            (s.encoding.as_str(), s.rate, s.channels),
            ("L16", 44_100, 1)
        );

        for bad in [
            "",
            "v=0\nm=audio 5004 RTP/AVP 97\n",
            "v=0\nc=IN IP4 10.0.0.1\nm=audio 5004 RTP/AVP 97\n",
            "v=0\nc=IN IP4 nope\nm=audio 5004 RTP/AVP 97\na=rtpmap:97 L24/48000/2\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn multicast() {
        let mut s = session(RefClock::LocalMac("00-1b-21-ab-cd-ef".into()));
        s.destination = "239.69.1.2:5004".parse().unwrap();
        let sdp = s.render();
        assert!(sdp.contains("c=IN IP4 239.69.1.2/32\r\n"), "{sdp}");
        assert!(sdp.contains("a=source-filter: incl IN IP4 239.69.1.2 192.168.1.10\r\n"));
        assert_eq!(parse(&sdp).unwrap().destination, s.destination.ip());
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
