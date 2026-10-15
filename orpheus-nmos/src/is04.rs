//! IS-04 v1.3 resources, as the Node API serves them.

use std::collections::BTreeMap;

use orpheus_core::{Layout, Symbol};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::caps::AudioConstraints;

pub const API_VERSION: &str = "v1.3";
pub const FORMAT_AUDIO: &str = "urn:x-nmos:format:audio";
/// RTP, unicast or multicast.
pub const TRANSPORT_RTP: &str = "urn:x-nmos:transport:rtp";
pub const DEVICE_GENERIC: &str = "urn:x-nmos:device:generic";
/// A device control for the IS-05 Connection API.
pub const CONTROL_CONNECTION: &str = "urn:x-nmos:control:sr-ctrl/v1.1";
/// The output type of a receiver's output (`speaker`, `hifi`, …).
pub const TAG_FORM_FACTOR: &str = "urn:x-orpheus:form-factor";
/// What a receiver warns its senders about: `jitter` if their packets
/// arrive with a wide spread.
pub const TAG_WARNINGS: &str = "urn:x-orpheus:warnings";
/// What the receiving node's policy does to a receiver's stream: `muted`
/// while another plays, or `ducked` under a high-priority one.
pub const TAG_STATE: &str = "urn:x-orpheus:state";

pub type Tags = BTreeMap<String, Vec<String>>;

/// When a resource last changed: TAI seconds and nanoseconds, written
/// `secs:nanos`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(u64);

impl Version {
    pub const fn from_tai_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn tai_nanos(self) -> u64 {
        self.0
    }

    /// The next possible version, for changes within the same nanosecond.
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0 / 1_000_000_000, self.0 % 1_000_000_000)
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl std::str::FromStr for Version {
    type Err = String;

    /// `secs:nanos`, nanoseconds below 10⁹.
    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("not a TAI time (secs:nanos): {s:?}");
        let (secs, nanos) = s.split_once(':').ok_or_else(bad)?;
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        if !digits(secs) || !digits(nanos) {
            return Err(bad());
        }
        let secs: u64 = secs.parse().map_err(|_| bad())?;
        let nanos: u64 = nanos.parse().map_err(|_| bad())?;
        if nanos >= 1_000_000_000 {
            return Err(bad());
        }
        secs.checked_mul(1_000_000_000)
            .and_then(|n| n.checked_add(nanos))
            .map(Self)
            .ok_or_else(bad)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The fields every resource has.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Base {
    pub id: Uuid,
    pub version: Version,
    pub label: String,
    pub description: String,
    pub tags: Tags,
}

impl Base {
    pub fn new(id: Uuid, label: impl Into<String>) -> Self {
        Self {
            id,
            version: Version::default(),
            label: label.into(),
            description: String::new(),
            tags: Tags::new(),
        }
    }
}

/// A resource with a [`Base`].
pub trait Resource: Serialize {
    fn base(&self) -> &Base;
    fn base_mut(&mut self) -> &mut Base;
}

macro_rules! resource {
    ($($t:ty),*) => {$(
        impl Resource for $t {
            fn base(&self) -> &Base {
                &self.base
            }
            fn base_mut(&mut self) -> &mut Base {
                &mut self.base
            }
        }
    )*};
}
resource!(Node, Device, Source, Flow, Sender, Receiver);

/// An empty JSON object, for `caps` without capabilities.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Node {
    #[serde(flatten)]
    pub base: Base,
    /// Base URL of the node's HTTP APIs, ending in `/`.
    pub href: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    pub api: Api,
    pub caps: Empty,
    pub services: Vec<Empty>,
    pub clocks: Vec<Clock>,
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Api {
    pub versions: Vec<String>,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub protocol: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "ref_type", rename_all = "lowercase")]
pub enum Clock {
    /// A free-running clock: the system clock.
    Internal { name: String },
    Ptp {
        name: String,
        /// Traceable to TAI, e.g. through GPS.
        traceable: bool,
        version: &'static str,
        /// Grandmaster identity, `00-11-22-ff-fe-33-44-55`.
        gmid: String,
        /// Synchronized to the grandmaster.
        locked: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Interface {
    /// Identifies the switch at the other end (LLDP); unknown here.
    pub chassis_id: Option<String>,
    /// The interface's MAC address, `00-11-22-33-44-55`.
    pub port_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Device {
    #[serde(flatten)]
    pub base: Base,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub node_id: Uuid,
    /// Deprecated in v1.3 but still required.
    pub senders: Vec<Uuid>,
    /// Deprecated in v1.3 but still required.
    pub receivers: Vec<Uuid>,
    pub controls: Vec<Control>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Control {
    pub href: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Source {
    #[serde(flatten)]
    pub base: Base,
    pub format: &'static str,
    pub caps: Empty,
    pub device_id: Uuid,
    pub parents: Vec<Uuid>,
    /// The node clock the source is timed by.
    pub clock_name: Option<String>,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Channel {
    pub label: String,
    /// `L`, `R`, … from the NMOS channel symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Symbol>,
}

impl Channel {
    /// The channels of `layout`, each with its label or the name of its
    /// symbol.
    pub fn of(layout: &Layout) -> Vec<Self> {
        layout
            .channels()
            .iter()
            .enumerate()
            .map(|(i, c)| Self {
                label: layout.label(i),
                symbol: Some(c.symbol),
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Flow {
    #[serde(flatten)]
    pub base: Base,
    pub format: &'static str,
    pub source_id: Uuid,
    pub device_id: Uuid,
    pub parents: Vec<Uuid>,
    pub media_type: &'static str,
    pub sample_rate: Rational,
    pub bit_depth: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rational {
    pub numerator: u32,
    pub denominator: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sender {
    #[serde(flatten)]
    pub base: Base,
    pub flow_id: Option<Uuid>,
    pub transport: &'static str,
    pub device_id: Uuid,
    /// Where the SDP file is; `None` until there is one.
    pub manifest_href: Option<String>,
    pub interface_bindings: Vec<String>,
    pub subscription: SenderSubscription,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SenderSubscription {
    pub receiver_id: Option<Uuid>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Receiver {
    #[serde(flatten)]
    pub base: Base,
    pub format: &'static str,
    pub caps: ReceiverCaps,
    pub device_id: Uuid,
    pub transport: &'static str,
    pub interface_bindings: Vec<String>,
    pub subscription: ReceiverSubscription,
}

/// What a receiver plays: IS-04's media types, and the finer constraints
/// of BCP-004-01 with the time they last changed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReceiverCaps {
    pub media_types: Vec<&'static str>,
    pub constraint_sets: Vec<AudioConstraints>,
    pub version: Version,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReceiverSubscription {
    pub sender_id: Option<Uuid>,
    pub active: bool,
}

/// A MAC address in NMOS form: lowercase, `-` separated.
pub fn mac(bytes: [u8; 6]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    parts.join("-")
}
