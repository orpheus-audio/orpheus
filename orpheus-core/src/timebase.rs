//! Conversion between clock time and media-clock frames.
//!
//! AES67 streams use a media clock that counts frames since the clock's
//! epoch (`a=mediaclk:direct=0`), so an RTP timestamp is the frame count at
//! the sampling instant, truncated to 32 bits.

const NS_PER_S: u128 = 1_000_000_000;

/// A media clock running at `rate` frames per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaClock {
    rate: u32,
}

impl MediaClock {
    pub const fn new(rate: u32) -> Self {
        assert!(rate > 0);
        Self { rate }
    }

    /// Frames elapsed at `ns` nanoseconds since the epoch.
    pub const fn frames_at(self, ns: u64) -> u64 {
        (ns as u128 * self.rate as u128 / NS_PER_S) as u64
    }

    /// The first nanosecond at which `frames` have elapsed.
    pub const fn ns_at(self, frames: u64) -> u64 {
        (frames as u128 * NS_PER_S).div_ceil(self.rate as u128) as u64
    }
}

/// Extends a 32-bit RTP timestamp to the 64-bit frame count closest to
/// `near`.
pub const fn extend(ts: u32, near: u64) -> u64 {
    near.wrapping_add_signed(ts.wrapping_sub(near as u32) as i32 as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_ns_round_trip() {
        let c = MediaClock::new(48_000);
        assert_eq!(c.frames_at(1_000_000_000), 48_000);
        assert_eq!(c.frames_at(20_833), 0);
        assert_eq!(c.frames_at(20_834), 1);
        assert_eq!(c.ns_at(1), 20_834);
        // About 55 years of nanoseconds since 1970.
        let now = 1_760_000_000_123_456_789;
        let f = c.frames_at(now);
        assert_eq!(c.frames_at(c.ns_at(f)), f);
        assert!(c.ns_at(f) <= now && now < c.ns_at(f + 1));
    }

    #[test]
    fn extend_across_wrap() {
        let near = 3 << 32 | 10;
        assert_eq!(extend(15, near), near + 5);
        assert_eq!(extend(u32::MAX, near), near - 11);
        assert_eq!(extend(0x7fff_0000, near), (3 << 32) + 0x7fff_0000);
    }
}
