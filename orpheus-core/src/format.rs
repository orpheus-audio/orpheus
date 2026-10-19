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

    /// The most channels a stream may have: what fits a packet at the
    /// shortest packet time (ST 2110-30).
    pub const MAX_CHANNELS: u16 = 64;

    /// Number of frames in `ms` milliseconds, rounded down.
    pub const fn frames_in_ms(self, ms: u32) -> usize {
        (self.rate as u64 * ms as u64 / 1000) as usize
    }

    /// Time each RTP packet carries, in microseconds, as ST 2110-30's
    /// levels have it: 1 ms for up to 8 channels, 125 µs for more, so
    /// that a packet stays within an Ethernet frame.
    pub const fn packet_us(self) -> u32 {
        if self.channels <= 8 { 1000 } else { 125 }
    }

    /// Frames each RTP packet carries.
    pub const fn packet_frames(self) -> usize {
        (self.rate as u64 * self.packet_us() as u64 / 1_000_000) as usize
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

    #[test]
    fn packets_fit_an_ethernet_frame() {
        let with = |channels| AudioFormat {
            rate: 48_000,
            channels,
        };
        assert_eq!(with(2).packet_frames(), 48);
        assert_eq!(with(8).packet_frames(), 48);
        assert_eq!(with(9).packet_frames(), 6);
        // L24: payloads of at most 1152 bytes.
        for channels in 1..=AudioFormat::MAX_CHANNELS {
            assert!(with(channels).packet_frames() * usize::from(channels) * 3 <= 1152);
        }
    }
}
