//! `~/.orpheus/config.toml`. The file is optional; every field has a default.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use orpheus_clock::ptp::PtpTransport;
use orpheus_core::OutputType;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Announced name; defaults to the hostname.
    pub name: Option<String>,
    pub api: ApiConfig,
    pub audio: AudioConfig,
    pub receiver: ReceiverConfig,
    pub ptp: PtpConfig,
    pub loopback: LoopbackConfig,
    /// Icon names for other nodes' outputs, by output type, replacing the
    /// defaults (Linux: XDG icon names).
    pub icons: BTreeMap<OutputType, String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Address the HTTP API (NMOS + /x-orpheus) listens on. `[::]` means
    /// all addresses, IPv4 and IPv6.
    pub listen: SocketAddr,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0u16; 8], 7480)),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    /// Frames per processing cycle to ask the audio system for. Smaller is
    /// lower latency but more CPU wakeups; `None` keeps the system default.
    pub quantum: Option<u32>,
}

/// Default UDP port for RTP, as is common for AES67.
pub const RTP_PORT: u16 = 5004;

/// Plays the RTP stream sent to this node.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiverConfig {
    pub enabled: bool,
    /// UDP port to receive RTP on, over IPv4 and IPv6.
    pub port: u16,
    /// Playout delay after the sender's timestamp. Covers network delay and
    /// the clock error between the nodes.
    pub link_offset_ms: u32,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: RTP_PORT,
            link_offset_ms: 10,
        }
    }
}

/// Clock synchronization with the other nodes (and AES67 devices). Without
/// it, or if it can't start, the node falls back to the system clock and
/// relies on NTP.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PtpConfig {
    pub enabled: bool,
    /// Network interface; the one with the default route if unset.
    pub interface: Option<String>,
    /// `ipv4`, `ipv6`, or `auto`: IPv4 if the interface has an IPv4
    /// address. AES67 and Dante devices only speak PTP over IPv4.
    pub transport: PtpTransport,
    pub domain: u8,
    /// Grandmaster election: lower wins. Nodes default to 250 so that
    /// dedicated grandmasters (usually 128 or less) take precedence.
    pub priority1: u8,
    pub priority2: u8,
    /// Never become grandmaster.
    pub slave_only: bool,
    /// Clock offsets above this are stepped (cold start), smaller ones
    /// slewed.
    pub step_threshold_us: u32,
    /// Offset from the grandmaster under which the clock counts as settled.
    pub settle_threshold_us: u32,
}

impl Default for PtpConfig {
    fn default() -> Self {
        let d = orpheus_clock::ptp::PtpConfig::default();
        Self {
            enabled: true,
            interface: d.interface,
            transport: d.transport,
            domain: d.domain,
            priority1: d.priority1,
            priority2: d.priority2,
            slave_only: d.slave_only,
            step_threshold_us: d.step_threshold_us,
            settle_threshold_us: d.settle_threshold_us,
        }
    }
}

/// Test route: a virtual sink "Orpheus loopback" that plays back locally.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopbackConfig {
    pub enabled: bool,
    /// Buffer between the sink and the playback stream.
    pub latency_ms: u32,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            latency_ms: 20,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The configured name, or the hostname if none is set.
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(hostname)
    }
}

/// The machine's host name.
pub fn hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "orpheus".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let c = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(c.api.listen.port(), 7480);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: Config = toml::from_str("name = \"Kitchen\"").unwrap();
        assert_eq!(c.name(), "Kitchen");
        assert_eq!(c.api.listen.port(), 7480);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("nmae = \"typo\"").is_err());
    }

    #[test]
    fn loopback_section() {
        let c: Config = toml::from_str("[loopback]\nenabled = true").unwrap();
        assert!(c.loopback.enabled);
        assert_eq!(c.loopback.latency_ms, 20);
        assert!(!Config::default().loopback.enabled);
    }

    #[test]
    fn ptp_section() {
        let c = Config::default();
        assert!(c.ptp.enabled);
        assert_eq!(c.ptp.priority1, 250);
        let c: Config = toml::from_str("[ptp]\ninterface = \"enp3s0\"\ndomain = 1").unwrap();
        assert_eq!(c.ptp.interface.as_deref(), Some("enp3s0"));
        assert_eq!(c.ptp.domain, 1);
        assert_eq!(c.ptp.step_threshold_us, 1000);
        assert_eq!(c.ptp.transport, PtpTransport::Auto);
        let c: Config = toml::from_str("[ptp]\ntransport = \"ipv6\"").unwrap();
        assert_eq!(c.ptp.transport, PtpTransport::Ipv6);
    }

    #[test]
    fn icons_section() {
        let c: Config = toml::from_str("[icons]\nhifi = \"audio-card\"").unwrap();
        assert_eq!(c.icons[&OutputType::Hifi], "audio-card");
        assert_eq!(c.icons.len(), 1);
        assert!(toml::from_str::<Config>("[icons]\ncar = \"x\"").is_err());
    }

    #[test]
    fn receiver_section() {
        let c = Config::default();
        assert!(c.receiver.enabled);
        assert_eq!(c.receiver.port, 5004);
        let c: Config = toml::from_str("[receiver]\nlink_offset_ms = 3").unwrap();
        assert_eq!(c.receiver.link_offset_ms, 3);
        assert_eq!(c.receiver.port, 5004);
    }
}
