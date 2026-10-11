//! `~/.orpheus/config.toml`. The file is optional; every field has a default.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Announced name; defaults to the hostname.
    pub name: Option<String>,
    pub api: ApiConfig,
    pub audio: AudioConfig,
    pub receiver: ReceiverConfig,
    pub loopback: LoopbackConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApiConfig {
    /// Address the HTTP API (NMOS + /x-orpheus) listens on.
    pub listen: SocketAddr,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 7480)),
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
    /// UDP port to receive RTP on.
    pub port: u16,
    /// Playout delay after the sender's timestamp. Covers network delay and
    /// the clock error between the nodes (NTP until PTP is in place).
    pub link_offset_ms: u32,
    /// `node.name` of the output to play on; default output if unset.
    pub target: Option<String>,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: RTP_PORT,
            link_offset_ms: 10,
            target: None,
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
    /// `node.name` of the output to play on; default output if unset.
    pub target: Option<String>,
}

impl Default for LoopbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            latency_ms: 20,
            target: None,
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
        self.name.clone().unwrap_or_else(|| {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|s| s.trim().to_owned())
                .unwrap_or_else(|_| "orpheus".to_owned())
        })
    }
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
    fn receiver_section() {
        let c = Config::default();
        assert!(c.receiver.enabled);
        assert_eq!(c.receiver.port, 5004);
        let c: Config = toml::from_str("[receiver]\nlink_offset_ms = 3").unwrap();
        assert_eq!(c.receiver.link_offset_ms, 3);
        assert_eq!(c.receiver.port, 5004);
    }
}
