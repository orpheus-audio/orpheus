//! `~/.orpheus/config.toml`. The file is optional; every field has a default.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Result};
use orpheus_clock::ptp::PtpTransport;
use orpheus_core::OutputType;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Announced name; defaults to the hostname.
    pub name: Option<String>,
    pub api: ApiConfig,
    pub audio: AudioConfig,
    pub receiver: ReceiverConfig,
    pub sender: SenderConfig,
    pub routes: RoutesConfig,
    pub sources: SourcesConfig,
    pub ptp: PtpConfig,
    pub loopback: LoopbackConfig,
    pub aes70: Aes70Config,
    pub ui: UiConfig,
    /// Icon names for other nodes' outputs, by output type, replacing the
    /// defaults (Linux: XDG icon names).
    pub icons: BTreeMap<OutputType, String>,
    /// The keys the file sets, as `section.key`.
    #[serde(skip)]
    pub keys: Vec<String>,
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

/// How much audio the audio system moves per cycle, asked for either in
/// frames or in milliseconds; `None` keeps the system default.
#[derive(Debug, Default, Deserialize)]
#[serde(try_from = "RawAudio")]
pub struct AudioConfig {
    pub block: Option<Block>,
}

/// A block size as configured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Block {
    /// `block_frames`: exactly this many frames at every sample rate.
    Frames(u32),
    /// `cycle_ms`: this long, in frames at the stream's rate.
    Millis(f64),
}

/// Block sizes PipeWire accepts (`clock.quantum-limit` defaults to 8192).
const BLOCK_FRAMES: std::ops::RangeInclusive<u32> = 16..=8192;

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawAudio {
    block_frames: Option<u32>,
    cycle_ms: Option<f64>,
}

impl TryFrom<RawAudio> for AudioConfig {
    type Error = String;

    fn try_from(raw: RawAudio) -> Result<Self, String> {
        let block = match (raw.block_frames, raw.cycle_ms) {
            (Some(_), Some(_)) => {
                return Err("set either block_frames or cycle_ms, not both".into());
            }
            (Some(frames), None) => Some(Block::Frames(frames)),
            (None, Some(ms)) => Some(Block::Millis(ms)),
            (None, None) => None,
        };
        let audio = Self { block };
        if let Some(frames) = audio.block_frames(48_000)
            && !BLOCK_FRAMES.contains(&frames)
        {
            return Err(format!(
                "the block must be {} to {} frames (at 48 kHz {:.2} to {:.0} ms), not {frames}",
                BLOCK_FRAMES.start(),
                BLOCK_FRAMES.end(),
                *BLOCK_FRAMES.start() as f64 / 48.0,
                *BLOCK_FRAMES.end() as f64 / 48.0,
            ));
        }
        Ok(audio)
    }
}

impl AudioConfig {
    /// The block size in frames at `rate`; `cycle_ms` rounds to the
    /// nearest frame.
    pub fn block_frames(&self, rate: u32) -> Option<u32> {
        self.block.map(|b| match b {
            Block::Frames(frames) => frames,
            Block::Millis(ms) => {
                let frames = (ms * f64::from(rate) / 1000.0).round();
                if frames.is_finite() {
                    frames.max(0.0) as u32
                } else {
                    0
                }
            }
        })
    }
}

/// Default UDP port for RTP, as is common for AES67.
pub const RTP_PORT: u16 = 5004;

/// The most receivers a node can have.
const MAX_RECEIVERS: u16 = 32;

/// Plays the RTP streams sent to this node, one receiver per connection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReceiverConfig {
    pub enabled: bool,
    /// UDP port of the first receiver, over IPv4 and IPv6; receiver `k`
    /// listens on `port + 2k`.
    pub port: u16,
    /// How many receivers there can be, and so connections.
    pub max: u16,
    /// Playout delay after the sender's timestamp. Covers network delay and
    /// the clock error between the nodes.
    pub link_offset_ms: u32,
    /// What the receiver plays while no sender is connected to it.
    pub accept: Accept,
    /// Which of several streams playing at once are heard.
    pub policy: Policy,
    /// How far normal streams are turned down while a high-priority one
    /// plays.
    pub duck_db: f32,
    /// Where streams play that have no output remembered, by machine name;
    /// `None` for the audio system's default output.
    pub default_output: Option<String>,
    /// What happens to a stream whose output goes away.
    pub when_lost: WhenLost,
    /// How long an output may be gone before it counts as lost: a stream
    /// keeps its place over shorter gaps, such as a Bluetooth speaker
    /// reconnecting.
    pub lost_after_ms: u32,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: RTP_PORT,
            max: 8,
            link_offset_ms: 10,
            accept: Accept::default(),
            policy: Policy::default(),
            duck_db: 20.0,
            default_output: None,
            when_lost: WhenLost::default(),
            lost_after_ms: 2000,
        }
    }
}

/// Which streams a receiver plays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Accept {
    /// Only a sender connected to it (through IS-05, which `orpheusctl
    /// connect` does on both ends).
    #[default]
    Connected,
    /// Also any stream sent to its port while no sender is connected.
    Any,
}

/// Which streams are heard when several play at once. A stream plays while
/// it carries sound (above −60 dBFS) or did within the last 5 seconds;
/// high-priority streams are always heard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// The one that started playing last; the others are muted.
    #[default]
    Last,
    /// The one that started playing first, until it stops.
    First,
    /// All of them.
    Mix,
}

/// What happens to a stream whose output goes away (switched off,
/// unplugged, gone with its node). It never plays somewhere else by itself,
/// unless told to with `default`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WhenLost {
    /// Pauses the player on the sending node where it can tell for sure
    /// which one plays the stream and that player plays nowhere else;
    /// else parks it.
    #[default]
    Pause,
    /// Parks it: it plays nowhere until its output is back or it is played
    /// elsewhere.
    Park,
    /// Plays it where streams without an output play.
    Default,
}

/// What Orpheus shows in the audio system.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    /// The language of the names Orpheus gives, such as `de`; `None` for
    /// the environment's.
    pub language: Option<String>,
}

/// The routes this node sends: when they hold a receiver on the peer.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SenderConfig {
    /// When a route holds its receiver on the peer.
    pub claim: Claim,
    /// With `claim = "on_play"`: how long a route's sink stays silent
    /// before the peer's receiver is let go.
    pub release_after_s: u32,
    /// How long sending fades in when it starts, so that audio cut in
    /// mid-sound doesn't click.
    pub fade_in_ms: u32,
}

impl Default for SenderConfig {
    fn default() -> Self {
        Self {
            claim: Claim::default(),
            release_after_s: 600,
            fade_in_ms: 20,
        }
    }
}

/// When a route holds its receiver on the peer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Claim {
    /// From connecting to disconnecting, also while nothing plays, so that
    /// short sounds such as alerts are heard in full.
    #[default]
    Always,
    /// Only while something plays into the sink: the receiver is claimed
    /// when audio starts (or the sink becomes the default output) and let
    /// go after `release_after_s` of silence, so that other senders can
    /// use it. The first moments of a sound after a pause are lost.
    OnPlay,
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

/// How long disconnected routes are remembered, so that connecting the
/// same peer again restores them. Routes to pinned nodes are kept anyway.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutesConfig {
    /// Forgets a route this many days after it was last connected.
    pub max_age_days: u32,
    /// Forgets the oldest routes beyond this many.
    pub max_entries: u32,
}

impl Default for RoutesConfig {
    fn default() -> Self {
        Self {
            max_age_days: 365,
            max_entries: 100,
        }
    }
}

/// How long the receivers remember a source's output and volume after it
/// last played. Sources from pinned nodes are kept anyway.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SourcesConfig {
    /// Forgets a source this many days after it last played.
    pub max_age_days: u32,
    /// Forgets the oldest sources beyond this many.
    pub max_entries: u32,
}

impl Default for SourcesConfig {
    fn default() -> Self {
        Self {
            max_age_days: 365,
            max_entries: 100,
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

/// AES70 (OCA) control: controllers see the wires and set their gains.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Aes70Config {
    pub enabled: bool,
    /// TCP port for OCP.1, on the API's address. Over a WebSocket,
    /// controllers use the API's port.
    pub port: u16,
}

impl Default for Aes70Config {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 7481,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let mut config: Self =
                    toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))?;
                config.keys = keys(&s);
                config
                    .check()
                    .with_context(|| format!("checking {}", path.display()))?;
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// What the types can't express.
    fn check(&self) -> Result<()> {
        let r = &self.receiver;
        if !(1..=MAX_RECEIVERS).contains(&r.max) {
            anyhow::bail!("[receiver] max must be 1 to {MAX_RECEIVERS}");
        }
        if u32::from(r.port) + 2 * (u32::from(r.max) - 1) > u32::from(u16::MAX) {
            anyhow::bail!("[receiver] port + 2 × (max − 1) must stay below 65536");
        }
        crate::settings::Settings::from_config(self)
            .check()
            .map_err(anyhow::Error::msg)
    }

    /// The configured name, or the hostname if none is set.
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(hostname)
    }
}

/// The keys a config file sets, as `section.key` (or `key` at the top);
/// empty if it doesn't parse.
pub fn keys(toml: &str) -> Vec<String> {
    let Ok(table) = toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    table
        .iter()
        .flat_map(|(name, value)| match value {
            toml::Value::Table(section) => section
                .keys()
                .map(|key| format!("{name}.{key}"))
                .collect::<Vec<_>>(),
            _ => vec![name.clone()],
        })
        .collect()
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
    fn aes70_section() {
        let c = Config::default();
        assert!(c.aes70.enabled);
        assert_eq!(c.aes70.port, 7481);
        let c: Config = toml::from_str("[aes70]\nenabled = false").unwrap();
        assert!(!c.aes70.enabled);
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
    fn audio_section() {
        let block =
            |toml: &str| toml::from_str::<Config>(toml).map(|c| c.audio.block_frames(48_000));
        assert_eq!(block("").unwrap(), None);
        assert_eq!(block("[audio]\nblock_frames = 256").unwrap(), Some(256));
        assert_eq!(block("[audio]\ncycle_ms = 5").unwrap(), Some(240));
        assert_eq!(block("[audio]\ncycle_ms = 5.33").unwrap(), Some(256));
        let c: Config = toml::from_str("[audio]\ncycle_ms = 5").unwrap();
        assert_eq!(c.audio.block_frames(96_000), Some(480));
        let c: Config = toml::from_str("[audio]\nblock_frames = 256").unwrap();
        assert_eq!(c.audio.block_frames(96_000), Some(256));
        let both = block("[audio]\nblock_frames = 256\ncycle_ms = 5").unwrap_err();
        assert!(both.to_string().contains("not both"), "{both}");
        assert!(block("[audio]\nblock_frames = 0").is_err());
        assert!(block("[audio]\ncycle_ms = -1").is_err());
        assert!(block("[audio]\ncycle_ms = 1000").is_err());
        assert!(block("[audio]\nquantum = 256").is_err());
    }

    #[test]
    fn receiver_section() {
        let c = Config::default();
        assert!(c.receiver.enabled);
        assert_eq!(c.receiver.port, 5004);
        assert_eq!(c.receiver.accept, Accept::Connected);
        let c: Config = toml::from_str("[receiver]\nlink_offset_ms = 3").unwrap();
        assert_eq!(c.receiver.link_offset_ms, 3);
        assert_eq!(c.receiver.port, 5004);
        let c: Config = toml::from_str("[receiver]\naccept = \"any\"").unwrap();
        assert_eq!(c.receiver.accept, Accept::Any);
        assert!(toml::from_str::<Config>("[receiver]\naccept = \"all\"").is_err());
        assert_eq!(c.receiver.max, 8);
        let max = |toml: &str| toml::from_str::<Config>(toml).unwrap().check();
        assert!(max("[receiver]\nmax = 1").is_ok());
        assert!(max("[receiver]\nmax = 0").is_err());
        assert!(max("[receiver]\nmax = 33").is_err());
        assert!(max("[receiver]\nport = 65530\nmax = 3").is_ok());
        assert!(max("[receiver]\nport = 65530\nmax = 4").is_err());
        assert_eq!(c.receiver.policy, Policy::Last);
        assert_eq!(c.receiver.duck_db, 20.0);
        let c: Config = toml::from_str("[receiver]\npolicy = \"mix\"\nduck_db = 12").unwrap();
        assert_eq!(c.receiver.policy, Policy::Mix);
        assert_eq!(c.receiver.duck_db, 12.0);
        assert!(max("[receiver]\nduck_db = -3").is_err());
        assert!(max("[receiver]\nduck_db = nan").is_err());
    }
}
