//! IS-05 v1.1 Connection API: the staged and active parameters of RTP
//! senders and receivers with one leg each.
//!
//! A controller patches a port's staged parameters and activates them, at
//! once or at a scheduled time; the active parameters are what the port
//! does, with every `auto` resolved.

use std::net::IpAddr;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use crate::is04::Version;

pub const API_VERSION: &str = "v1.1";
pub const TRANSPORT_RTP: &str = "urn:x-nmos:transport:rtp";
pub const TRANSPORT_FILE_SDP: &str = "application/sdp";

/// A parameter the port may choose itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Auto<T> {
    #[default]
    Auto,
    Value(T),
}

impl<T> Auto<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Self::Auto => None,
            Self::Value(v) => Some(v),
        }
    }
}

impl<T: Serialize> Serialize for Auto<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Value(v) => v.serialize(s),
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Auto<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "auto" => Ok(Self::Auto),
            v => T::deserialize(v)
                .map(Self::Value)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ActivateImmediate,
    ActivateScheduledAbsolute,
    ActivateScheduledRelative,
}

/// When staged parameters become active. `requested_time` is TAI for
/// absolute activations and a duration for relative ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub requested_time: Option<Version>,
    #[serde(default)]
    pub activation_time: Option<Version>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderTransport {
    pub source_ip: Auto<IpAddr>,
    pub destination_ip: Auto<IpAddr>,
    pub source_port: Auto<u16>,
    pub destination_port: Auto<u16>,
    pub rtp_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SenderParams {
    pub receiver_id: Option<Uuid>,
    pub master_enable: bool,
    pub activation: Activation,
    pub transport_params: Vec<SenderTransport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverTransport {
    /// Only packets from this address; `None` takes any.
    pub source_ip: Option<IpAddr>,
    /// The group to join; `None` for unicast.
    pub multicast_ip: Option<IpAddr>,
    pub interface_ip: Auto<IpAddr>,
    pub destination_port: Auto<u16>,
    pub rtp_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransportFile {
    pub data: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverParams {
    pub sender_id: Option<Uuid>,
    pub master_enable: bool,
    pub activation: Activation,
    pub transport_file: TransportFile,
    pub transport_params: Vec<ReceiverTransport>,
}

/// `staged` with a PATCH body applied: objects merge, each leg of
/// `transport_params` merges with the staged leg, and `activation` is
/// replaced as a whole.
pub fn patch<T: Serialize + DeserializeOwned>(staged: &T, body: Value) -> Result<T, String> {
    let Value::Object(body) = body else {
        return Err("the body must be a JSON object".into());
    };
    let mut merged = serde_json::to_value(staged).map_err(|e| e.to_string())?;
    let target = merged.as_object_mut().expect("parameters are objects");
    for (key, value) in body {
        match (key.as_str(), value) {
            ("transport_params", Value::Array(legs)) => {
                let staged = target
                    .get_mut("transport_params")
                    .and_then(Value::as_array_mut)
                    .ok_or("no transport_params here")?;
                if legs.len() != staged.len() {
                    return Err(format!(
                        "transport_params has {} legs, not {}",
                        staged.len(),
                        legs.len()
                    ));
                }
                for (leg, patch) in staged.iter_mut().zip(legs) {
                    let Value::Object(patch) = patch else {
                        return Err("each leg must be a JSON object".into());
                    };
                    let leg = leg.as_object_mut().expect("legs are objects");
                    leg.extend(patch);
                }
            }
            ("transport_params", _) => return Err("transport_params must be an array".into()),
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
    serde_json::from_value(merged).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sender() -> SenderParams {
        SenderParams {
            receiver_id: None,
            master_enable: true,
            activation: Activation::default(),
            transport_params: vec![SenderTransport {
                source_ip: Auto::Auto,
                destination_ip: Auto::Value("10.0.0.2".parse().unwrap()),
                source_port: Auto::Auto,
                destination_port: Auto::Value(5004),
                rtp_enabled: true,
            }],
        }
    }

    #[test]
    fn serializes_as_is05() {
        let json = serde_json::to_value(sender()).unwrap();
        assert_eq!(
            json,
            json!({
                "receiver_id": null,
                "master_enable": true,
                "activation": {"mode": null, "requested_time": null, "activation_time": null},
                "transport_params": [{
                    "source_ip": "auto",
                    "destination_ip": "10.0.0.2",
                    "source_port": "auto",
                    "destination_port": 5004,
                    "rtp_enabled": true,
                }],
            })
        );
    }

    #[test]
    fn patches_merge() {
        let p = patch(
            &sender(),
            json!({
                "master_enable": false,
                "activation": {"mode": "activate_scheduled_relative", "requested_time": "0:200000000"},
                "transport_params": [{"destination_port": 5006, "source_ip": "10.0.0.1"}],
            }),
        )
        .unwrap();
        assert!(!p.master_enable);
        assert_eq!(p.activation.mode, Some(Mode::ActivateScheduledRelative));
        assert_eq!(
            p.activation.requested_time,
            Some(Version::from_tai_nanos(200_000_000))
        );
        let leg = &p.transport_params[0];
        assert_eq!(leg.destination_port, Auto::Value(5006));
        assert_eq!(leg.source_ip, Auto::Value("10.0.0.1".parse().unwrap()));
        assert_eq!(leg.destination_ip, Auto::Value("10.0.0.2".parse().unwrap()));
        // Empty legs change nothing.
        assert_eq!(
            patch(&sender(), json!({"transport_params": [{}]})).unwrap(),
            sender()
        );
    }

    #[test]
    fn bad_patches_are_refused() {
        for body in [
            json!({"bad": "data"}),
            json!([]),
            json!({"transport_params": [{}, {}]}),
            json!({"transport_params": [{"destination_port": "x"}]}),
            json!({"transport_params": [{"bogus": 1}]}),
            json!({"activation": {"mode": "now"}}),
            json!({"activation": {"mode": "activate_immediate", "requested_time": "1:x"}}),
            json!({"master_enable": "yes"}),
        ] {
            assert!(patch(&sender(), body.clone()).is_err(), "{body}");
        }
    }

    #[test]
    fn tai_times() {
        assert_eq!("1:5".parse(), Ok(Version::from_tai_nanos(1_000_000_005)));
        for bad in ["", "1", "1:", ":1", "-1:0", "1:1000000000", "1:+5"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }
}
