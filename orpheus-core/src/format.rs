//! PCM audio formats.

/// Sample rate and channel count of a PCM stream.
///
/// Inside a node, audio travels as interleaved `f32`; the wire encoding
/// (L16/L24) is a property of the RTP stream, not of this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    /// The AES67 baseline: 48 kHz stereo.
    pub const AES67: Self = Self {
        rate: 48_000,
        channels: 2,
    };

    /// Number of frames in `ms` milliseconds, rounded down.
    pub const fn frames_in_ms(self, ms: u32) -> usize {
        (self.rate as u64 * ms as u64 / 1000) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_in_ms() {
        assert_eq!(AudioFormat::AES67.frames_in_ms(1), 48);
        assert_eq!(AudioFormat::AES67.frames_in_ms(20), 960);
        let cd = AudioFormat {
            rate: 44_100,
            channels: 2,
        };
        assert_eq!(cd.frames_in_ms(1), 44);
    }
}
