//! Persistent short identifiers.
//!
//! A [`DeviceId`] or [`RouteId`] is 40 random bits, written as 8 characters of
//! Crockford Base32 (`0-9`, `A-Z` without `I`, `L`, `O`, `U`), e.g. `K7QM3XRA`.
//! A device generates its ID once on first boot and keeps it for its lifetime;
//! a route gets one when it is first connected and keeps it until forgotten.
//!
//! Parsing is lenient as the Crockford spec suggests: lowercase is accepted,
//! `I`/`L` read as `1`, `O` reads as `0`, and hyphens are ignored (`K7QM-3XRA`).

use core::fmt;
use core::str::FromStr;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Number of characters in the canonical form.
pub const LEN: usize = 8;
const BITS: u32 = 5 * LEN as u32;
const MASK: u64 = (1 << BITS) - 1;

/// Error returned when parsing an ID fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The input had the wrong number of symbols (hyphens not counted).
    Length(usize),
    /// The input contained a character outside the Crockford alphabet.
    InvalidChar(char),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(n) => write!(f, "ID must have {LEN} symbols, got {n}"),
            Self::InvalidChar(c) => write!(f, "invalid character {c:?} in ID"),
        }
    }
}

impl core::error::Error for ParseError {}

fn decode_symbol(c: char) -> Option<u64> {
    let v = match c.to_ascii_uppercase() {
        c @ '0'..='9' => c as u8 - b'0',
        'O' => 0,
        'I' | 'L' => 1,
        c => ALPHABET.iter().position(|&a| a == c as u8)? as u8,
    };
    Some(v as u64)
}

fn encode(bits: u64) -> [u8; LEN] {
    let mut out = [0u8; LEN];
    for (i, c) in out.iter_mut().enumerate() {
        let shift = 5 * (LEN - 1 - i);
        *c = ALPHABET[((bits >> shift) & 0x1f) as usize];
    }
    out
}

fn decode(s: &str) -> Result<u64, ParseError> {
    let mut bits = 0u64;
    let mut n = 0;
    for c in s.chars().filter(|&c| c != '-') {
        let v = decode_symbol(c).ok_or(ParseError::InvalidChar(c))?;
        n += 1;
        if n <= LEN {
            bits = (bits << 5) | v;
        }
    }
    if n != LEN {
        return Err(ParseError::Length(n));
    }
    Ok(bits)
}

macro_rules! short_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Builds an ID from the low 40 bits of `bits`; higher bits are discarded.
            ///
            /// Use this on targets without `getrandom`, feeding it from the platform RNG
            /// (e.g. `esp_random()` on the ESP32).
            pub const fn from_bits(bits: u64) -> Self {
                Self(bits & MASK)
            }

            /// The 40-bit value.
            pub const fn bits(self) -> u64 {
                self.0
            }

            /// Generates a new random ID from the OS random source.
            #[cfg(feature = "getrandom")]
            pub fn generate() -> Result<Self, getrandom::Error> {
                let mut buf = [0u8; 8];
                getrandom::fill(&mut buf)?;
                Ok(Self::from_bits(u64::from_le_bytes(buf)))
            }

            /// The canonical 8-character uppercase form as ASCII bytes.
            pub fn to_ascii(self) -> [u8; LEN] {
                encode(self.0)
            }
        }

        impl FromStr for $name {
            type Err = ParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                decode(s).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let ascii = self.to_ascii();
                // The alphabet is pure ASCII, so this cannot fail.
                f.write_str(core::str::from_utf8(&ascii).unwrap())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        #[cfg(feature = "serde")]
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let ascii = self.to_ascii();
                s.serialize_str(core::str::from_utf8(&ascii).unwrap())
            }
        }

        #[cfg(feature = "serde")]
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "an ID of {LEN} Crockford Base32 symbols")
                    }
                    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<$name, E> {
                        v.parse().map_err(E::custom)
                    }
                }
                d.deserialize_str(Visitor)
            }
        }
    };
}

short_id! {
    /// A random, persistent 40-bit device identifier.
    DeviceId
}

short_id! {
    /// A random 40-bit route identifier, unique among the routes of one device.
    RouteId
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::string::ToString;

    #[test]
    fn roundtrip() {
        for bits in [0, 1, 0x12_3456_789a, MASK] {
            let id = DeviceId::from_bits(bits);
            let s = id.to_string();
            assert_eq!(s.len(), LEN);
            assert_eq!(s.parse::<DeviceId>(), Ok(id));
        }
    }

    #[test]
    fn known_values() {
        assert_eq!(DeviceId::from_bits(0).to_string(), "00000000");
        assert_eq!(DeviceId::from_bits(MASK).to_string(), "ZZZZZZZZ");
        assert_eq!(DeviceId::from_bits(31).to_string(), "0000000Z");
    }

    #[test]
    fn truncates_to_40_bits() {
        assert_eq!(DeviceId::from_bits(u64::MAX).bits(), MASK);
    }

    #[test]
    fn lenient_parsing() {
        let id: DeviceId = "K7QM3XRA".parse().unwrap();
        assert_eq!("k7qm-3xra".parse(), Ok(id));
        assert_eq!(
            "O0IL".repeat(2).parse::<DeviceId>().unwrap().to_string(),
            "00110011"
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!("K7QM3XR".parse::<DeviceId>(), Err(ParseError::Length(7)));
        assert_eq!("K7QM3XRAB".parse::<DeviceId>(), Err(ParseError::Length(9)));
        assert_eq!(
            "K7QM3XRU".parse::<DeviceId>(),
            Err(ParseError::InvalidChar('U'))
        );
        assert_eq!(
            "K7QM 3XRA".parse::<DeviceId>(),
            Err(ParseError::InvalidChar(' '))
        );
    }

    #[cfg(feature = "getrandom")]
    #[test]
    fn generate_differs() {
        let a = DeviceId::generate().unwrap();
        let b = DeviceId::generate().unwrap();
        assert_ne!(a, b);
        assert!(a.bits() <= MASK);
    }
}
