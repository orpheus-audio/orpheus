//! Receiver capabilities after AMWA BCP-004-01.
//!
//! A receiver lists in `caps.constraint_sets` the streams it can play.
//! Each constraint set constrains parameters such as the sample rate, by an
//! `enum` of allowed values and/or a `minimum` and `maximum`; a stream
//! fits a set if it meets all of its constraints, and fits the receiver if
//! it fits any set that is not disabled (`urn:x-nmos:cap:meta:enabled`).
//! Constraints on parameters [`check`] doesn't know are ignored, as the
//! recommendation allows.

use serde::Serialize;
use serde_json::Value;

use crate::is04::Rational;

pub const MEDIA_TYPE: &str = "urn:x-nmos:cap:format:media_type";
pub const SAMPLE_RATE: &str = "urn:x-nmos:cap:format:sample_rate";
pub const CHANNEL_COUNT: &str = "urn:x-nmos:cap:format:channel_count";
pub const SAMPLE_DEPTH: &str = "urn:x-nmos:cap:format:sample_depth";
pub const PACKET_TIME: &str = "urn:x-nmos:cap:transport:packet_time";
const ENABLED: &str = "urn:x-nmos:cap:meta:enabled";

/// The one constraint set of a receiver that plays exactly one format.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AudioConstraints {
    #[serde(rename = "urn:x-nmos:cap:format:media_type")]
    pub media_type: Enum<&'static str>,
    #[serde(rename = "urn:x-nmos:cap:format:sample_rate")]
    pub sample_rate: Enum<Rational>,
    #[serde(rename = "urn:x-nmos:cap:format:channel_count")]
    pub channel_count: Enum<u16>,
    #[serde(rename = "urn:x-nmos:cap:format:sample_depth")]
    pub sample_depth: Enum<u8>,
}

impl AudioConstraints {
    /// Exactly `stream`'s format, at any packet time.
    pub fn exactly(stream: &Stream<'static>) -> Self {
        Self {
            media_type: Enum::one(stream.media_type),
            sample_rate: Enum::one(Rational {
                numerator: stream.sample_rate,
                denominator: 1,
            }),
            channel_count: Enum::one(stream.channel_count),
            sample_depth: Enum::one(stream.sample_depth),
        }
    }
}

/// A constraint that allows only the values listed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Enum<T> {
    #[serde(rename = "enum")]
    pub values: Vec<T>,
}

impl<T> Enum<T> {
    pub fn one(value: T) -> Self {
        Self {
            values: vec![value],
        }
    }
}

/// A stream as a sender would send it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stream<'a> {
    /// `audio/L24`, …
    pub media_type: &'a str,
    pub sample_rate: u32,
    pub channel_count: u16,
    /// Bits per sample.
    pub sample_depth: u8,
    pub packet_time_ms: f64,
}

impl Stream<'_> {
    /// The parameters it has, with their names and units for messages.
    fn params(&self) -> [(&'static str, &'static str, Param<'_>, &'static str); 5] {
        [
            (MEDIA_TYPE, "media type", Param::Text(self.media_type), ""),
            (
                SAMPLE_RATE,
                "sample rate",
                Param::Rational(self.sample_rate.into(), 1),
                " Hz",
            ),
            (
                CHANNEL_COUNT,
                "channel count",
                Param::Number(self.channel_count.into()),
                "",
            ),
            (
                SAMPLE_DEPTH,
                "sample depth",
                Param::Number(self.sample_depth.into()),
                " bits",
            ),
            (
                PACKET_TIME,
                "packet time",
                Param::Number(self.packet_time_ms),
                " ms",
            ),
        ]
    }
}

/// A parameter's value.
#[derive(Debug, Clone, Copy)]
enum Param<'a> {
    Text(&'a str),
    Number(f64),
    Rational(u64, u64),
}

impl Param<'_> {
    /// Compares with a constraint value of the same kind; `None` if it is
    /// of another kind.
    fn cmp(&self, v: &Value) -> Option<std::cmp::Ordering> {
        match *self {
            Param::Text(s) => {
                let v = v.as_str()?;
                // Media types ignore case.
                Some(s.to_ascii_lowercase().cmp(&v.to_ascii_lowercase()))
            }
            Param::Number(n) => n.partial_cmp(&v.as_f64()?),
            Param::Rational(n, d) => {
                let (vn, vd) = rational(v)?;
                (u128::from(n) * u128::from(vd)).partial_cmp(&(u128::from(vn) * u128::from(d)))
            }
        }
    }

    /// Whether it meets `constraint`.
    fn meets(&self, constraint: &Value) -> bool {
        use std::cmp::Ordering::*;
        let listed = constraint["enum"]
            .as_array()
            .is_none_or(|e| e.iter().any(|v| self.cmp(v) == Some(Equal)));
        let above = constraint
            .get("minimum")
            .is_none_or(|m| matches!(self.cmp(m), Some(Greater | Equal)));
        let below = constraint
            .get("maximum")
            .is_none_or(|m| matches!(self.cmp(m), Some(Less | Equal)));
        listed && above && below
    }
}

impl std::fmt::Display for Param<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            Param::Text(s) => f.write_str(s),
            Param::Number(n) => write!(f, "{n}"),
            Param::Rational(n, 1) => write!(f, "{n}"),
            Param::Rational(n, d) => write!(f, "{n}/{d}"),
        }
    }
}

/// An IS-04 rational: `numerator` and `denominator`, which defaults to 1.
fn rational(v: &Value) -> Option<(u64, u64)> {
    let n = v["numerator"].as_u64()?;
    let d = match v.get("denominator") {
        Some(d) => d.as_u64().filter(|&d| d > 0)?,
        None => 1,
    };
    Some((n, d))
}

/// A constraint value for messages.
fn show(v: &Value) -> String {
    match (v.as_str(), rational(v)) {
        (Some(s), _) => s.to_owned(),
        (_, Some((n, 1))) => n.to_string(),
        (_, Some((n, d))) => format!("{n}/{d}"),
        _ => v.to_string(),
    }
}

/// What a constraint allows, for messages: "8", "1 or 2", "at most 8".
fn allowed(constraint: &Value) -> String {
    let mut parts = vec![];
    if let Some(e) = constraint["enum"].as_array() {
        let values: Vec<String> = e.iter().map(show).collect();
        parts.push(match values.as_slice() {
            [] => "nothing".to_owned(),
            [one] => one.clone(),
            [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
        });
    }
    match (constraint.get("minimum"), constraint.get("maximum")) {
        (Some(min), Some(max)) => parts.push(format!("{} to {}", show(min), show(max))),
        (Some(min), None) => parts.push(format!("at least {}", show(min))),
        (None, Some(max)) => parts.push(format!("at most {}", show(max))),
        (None, None) => {}
    }
    parts.join(", ")
}

/// Whether a receiver with `caps` can play `stream`; if not, why, as in
/// "channel count 1; it takes 2".
pub fn check(caps: &Value, stream: &Stream) -> Result<(), String> {
    if let Some(types) = caps["media_types"].as_array()
        && !types.iter().any(|t| {
            t.as_str()
                .is_some_and(|t| t.eq_ignore_ascii_case(stream.media_type))
        })
    {
        let types: Vec<String> = types.iter().map(show).collect();
        return Err(format!(
            "media type {}; it takes {}",
            stream.media_type,
            types.join(", ")
        ));
    }
    let Some(sets) = caps["constraint_sets"].as_array() else {
        return Ok(());
    };
    let mut why = None;
    for set in sets.iter().filter(|s| s[ENABLED] != false) {
        let failed = stream
            .params()
            .into_iter()
            .find(|(urn, _, param, _)| set.get(*urn).is_some_and(|c| !param.meets(c)));
        match failed {
            None => return Ok(()),
            Some((urn, name, param, unit)) => {
                why.get_or_insert_with(|| {
                    format!(
                        "{name} {param}{unit}; it takes {}{unit}",
                        allowed(&set[urn])
                    )
                });
            }
        }
    }
    Err(why.unwrap_or_else(|| "it takes no stream at the moment".to_owned()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const L24: Stream<'static> = Stream {
        media_type: "audio/L24",
        sample_rate: 48_000,
        channel_count: 2,
        sample_depth: 24,
        packet_time_ms: 1.0,
    };

    #[test]
    fn our_constraints_take_our_stream() {
        let set = serde_json::to_value(AudioConstraints::exactly(&L24)).unwrap();
        assert_eq!(
            set[SAMPLE_RATE],
            json!({"enum": [{"numerator": 48000, "denominator": 1}]})
        );
        assert_eq!(set[CHANNEL_COUNT], json!({"enum": [2]}));
        let caps = json!({"media_types": ["audio/L24"], "constraint_sets": [set]});
        assert_eq!(check(&caps, &L24), Ok(()));
        let mono = Stream {
            channel_count: 1,
            ..L24
        };
        assert_eq!(
            check(&caps, &mono),
            Err("channel count 1; it takes 2".into())
        );
    }

    #[test]
    fn without_constraint_sets_media_types_decide() {
        assert_eq!(check(&json!({}), &L24), Ok(()));
        assert_eq!(check(&json!({"media_types": ["audio/l24"]}), &L24), Ok(()));
        assert_eq!(
            check(&json!({"media_types": ["audio/L16"]}), &L24),
            Err("media type audio/L24; it takes audio/L16".into())
        );
    }

    #[test]
    fn any_enabled_set_will_do() {
        let caps = json!({"constraint_sets": [
            {CHANNEL_COUNT: {"enum": [8]}},
            {CHANNEL_COUNT: {"enum": [2]}, ENABLED: false},
            {CHANNEL_COUNT: {"minimum": 1, "maximum": 2}, SAMPLE_RATE: {"enum": [{"numerator": 48000}]}},
        ]});
        assert_eq!(check(&caps, &L24), Ok(()));
        let hd = Stream {
            sample_rate: 96_000,
            ..L24
        };
        // The first set that fails says why.
        assert_eq!(check(&caps, &hd), Err("channel count 2; it takes 8".into()));
    }

    #[test]
    fn ranges_rationals_and_unknown_constraints() {
        let caps = |c: Value| json!({"constraint_sets": [c]});
        let packet = caps(json!({PACKET_TIME: {"enum": [0.125, 0.25]}}));
        assert_eq!(
            check(&packet, &L24),
            Err("packet time 1 ms; it takes 0.125 or 0.25 ms".into())
        );
        let max = caps(json!({CHANNEL_COUNT: {"maximum": 1}}));
        assert_eq!(
            check(&max, &L24),
            Err("channel count 2; it takes at most 1".into())
        );
        let rate = caps(json!({SAMPLE_RATE: {"enum": [{"numerator": 96000, "denominator": 2}]}}));
        assert_eq!(check(&rate, &L24), Ok(()));
        let unknown =
            caps(json!({"urn:x-nmos:cap:format:color_sampling": {"enum": ["YCbCr-4:2:2"]}}));
        assert_eq!(check(&unknown, &L24), Ok(()));
        assert_eq!(
            check(&json!({"constraint_sets": []}), &L24),
            Err("it takes no stream at the moment".into())
        );
    }
}
