//! Channel maps: which channel of a stream plays on each channel of an
//! output.

use core::fmt;

/// The most channels a map covers.
pub const MAX_CHANNELS: usize = 8;

/// An output channel that plays nothing.
const MUTED: u8 = 0xFF;

/// For each output channel, the input channel that plays on it, or `None`
/// to mute it. On a stereo stream, `[1, 0]` swaps left and right, `[0, 0]`
/// plays the left channel on both sides and `[None, 1]` mutes the left
/// side. A channel can't be mixed from several; a map only picks.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelMap {
    len: u8,
    from: [u8; MAX_CHANNELS],
}

impl ChannelMap {
    /// Every channel plays on itself.
    ///
    /// # Panics
    ///
    /// If `channels` is 0 or more than [`MAX_CHANNELS`].
    pub const fn identity(channels: usize) -> Self {
        assert!(channels > 0 && channels <= MAX_CHANNELS);
        let mut from = [MUTED; MAX_CHANNELS];
        let mut i = 0;
        while i < channels {
            from[i] = i as u8;
            i += 1;
        }
        Self {
            len: channels as u8,
            from,
        }
    }

    /// A map of as many output channels as the stream has, each naming one
    /// of them or `None`; `None` if it doesn't fit that.
    pub fn new(from: &[Option<u8>]) -> Option<Self> {
        if from.is_empty() || from.len() > MAX_CHANNELS {
            return None;
        }
        let mut map = Self {
            len: from.len() as u8,
            from: [MUTED; MAX_CHANNELS],
        };
        for (out, input) in from.iter().enumerate() {
            map.from[out] = match *input {
                Some(i) if usize::from(i) < from.len() => i,
                Some(_) => return None,
                None => MUTED,
            };
        }
        Some(map)
    }

    /// The number of channels.
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Never true; there for [`len`](Self::len).
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The input channel that plays on output channel `out`, if any.
    pub fn get(&self, out: usize) -> Option<u8> {
        self.from[..self.len()]
            .get(out)
            .copied()
            .filter(|&i| i != MUTED)
    }

    /// The input channels by output channel.
    pub fn iter(&self) -> impl Iterator<Item = Option<u8>> + '_ {
        (0..self.len()).map(|out| self.get(out))
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::identity(self.len())
    }

    /// Output channel `out` of a frame of input samples.
    pub fn sample(&self, frame: &[f32], out: usize) -> f32 {
        self.get(out)
            .and_then(|i| frame.get(usize::from(i)))
            .copied()
            .unwrap_or(0.0)
    }

    /// The map in one word, to share it without a lock: a byte per
    /// channel. [`from_bits`](Self::from_bits) needs the channel count.
    pub fn to_bits(self) -> u64 {
        u64::from_le_bytes(self.from)
    }

    /// The map of `channels` channels that [`to_bits`](Self::to_bits)
    /// gave.
    pub fn from_bits(channels: usize, bits: u64) -> Self {
        Self {
            len: channels.min(MAX_CHANNELS) as u8,
            from: bits.to_le_bytes(),
        }
    }
}

impl fmt::Debug for ChannelMap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A JSON array of input channels, `null` for muted: `[1, 0]`.
#[cfg(feature = "serde")]
impl serde::Serialize for ChannelMap {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for ChannelMap {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ChannelMap;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(
                    f,
                    "1 to {MAX_CHANNELS} input channels, each a channel number or null"
                )
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<ChannelMap, A::Error> {
                let mut from = [None; MAX_CHANNELS];
                let mut len = 0;
                while let Some(input) = seq.next_element::<Option<u8>>()? {
                    if len == MAX_CHANNELS {
                        return Err(serde::de::Error::invalid_length(len + 1, &self));
                    }
                    from[len] = input;
                    len += 1;
                }
                ChannelMap::new(&from[..len]).ok_or_else(|| {
                    serde::de::Error::custom(format_args!(
                        "each of the {len} channels must be null or 0 to {}",
                        len.saturating_sub(1)
                    ))
                })
            }
        }
        d.deserialize_seq(Visitor)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn maps_pick_channels() {
        let frame = [0.25, 0.5];
        let swap = ChannelMap::new(&[Some(1), Some(0)]).unwrap();
        assert_eq!(
            (swap.sample(&frame, 0), swap.sample(&frame, 1)),
            (0.5, 0.25)
        );
        let mono = ChannelMap::new(&[Some(0), Some(0)]).unwrap();
        assert_eq!(
            (mono.sample(&frame, 0), mono.sample(&frame, 1)),
            (0.25, 0.25)
        );
        let mute = ChannelMap::new(&[None, Some(1)]).unwrap();
        assert_eq!((mute.sample(&frame, 0), mute.sample(&frame, 1)), (0.0, 0.5));
        assert!(ChannelMap::new(&[Some(0), Some(1)]).unwrap().is_identity());
        assert!(!swap.is_identity());
    }

    #[test]
    fn only_channels_the_stream_has() {
        assert_eq!(ChannelMap::new(&[Some(2), Some(0)]), None);
        assert_eq!(ChannelMap::new(&[]), None);
        assert_eq!(ChannelMap::new(&[None; MAX_CHANNELS + 1]), None);
    }

    #[test]
    fn survives_the_trip_through_a_word() {
        for map in [
            ChannelMap::identity(2),
            ChannelMap::new(&[None, Some(0)]).unwrap(),
            ChannelMap::new(&[Some(7); 8]).unwrap(),
        ] {
            assert_eq!(ChannelMap::from_bits(map.len(), map.to_bits()), map);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn reads_and_writes_json_arrays() {
        let map: ChannelMap = serde_json::from_str("[null, 0]").unwrap();
        assert_eq!(map, ChannelMap::new(&[None, Some(0)]).unwrap());
        assert_eq!(serde_json::to_string(&map).unwrap(), "[null,0]");
        let err = serde_json::from_str::<ChannelMap>("[2, 0]").unwrap_err();
        assert!(std::format!("{err}").contains("null or 0 to 1"), "{err}");
        assert!(serde_json::from_str::<ChannelMap>("[]").is_err());
        assert!(serde_json::from_str::<ChannelMap>("[0,0,0,0,0,0,0,0,0]").is_err());
    }
}
