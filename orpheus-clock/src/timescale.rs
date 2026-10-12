//! Mapping from the kernel's realtime clock to TAI.

use std::time::{SystemTime, UNIX_EPOCH};

/// TAI − UTC since 2017; used when the kernel does not know the offset
/// (it only does when something like ptp4l or chrony's `leapsectz` sets it).
const TAI_UTC_S: i64 = 37;

/// Converts `CLOCK_REALTIME` readings (system time, packet timestamps) to
/// TAI nanoseconds.
///
/// For tests it can also run fast or slow and be offset, so that one host
/// can play two nodes whose clocks disagree.
#[derive(Debug, Clone, Copy)]
pub struct Timescale {
    offset_ns: i64,
    skew_ppm: f64,
    /// Realtime at which the skew starts to accumulate.
    origin_ns: u64,
}

impl Timescale {
    /// TAI, using the kernel's TAI offset if it has one.
    pub fn tai() -> Self {
        let kernel = kernel_tai_offset();
        Self {
            offset_ns: if kernel > 0 { kernel } else { TAI_UTC_S } * 1_000_000_000,
            skew_ppm: 0.0,
            origin_ns: 0,
        }
    }

    /// The same timescale, offset by `offset_ns` and running `skew_ppm`
    /// parts per million fast from now on. For testing only.
    pub fn skewed(self, skew_ppm: f64, offset_ns: i64) -> Self {
        Self {
            offset_ns: self.offset_ns + offset_ns,
            skew_ppm,
            origin_ns: realtime_ns(),
        }
    }

    pub fn is_skewed(&self) -> bool {
        self.skew_ppm != 0.0 || self.offset_ns % 1_000_000_000 != 0
    }

    /// Converts a realtime reading in nanoseconds.
    pub fn from_realtime(&self, realtime_ns: u64) -> u64 {
        let elapsed = realtime_ns as i64 - self.origin_ns as i64;
        let skew = (elapsed as f64 * self.skew_ppm * 1e-6) as i64;
        (realtime_ns as i64 + self.offset_ns + skew) as u64
    }

    pub fn now(&self) -> u64 {
        self.from_realtime(realtime_ns())
    }
}

fn realtime_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// The kernel's TAI − UTC offset in seconds, 0 if unset.
fn kernel_tai_offset() -> i64 {
    let mut tai = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut utc = tai;
    // SAFETY: plain syscalls writing into the timespecs we pass.
    let ok = unsafe {
        libc::clock_gettime(libc::CLOCK_TAI, &mut tai) == 0
            && libc::clock_gettime(libc::CLOCK_REALTIME, &mut utc) == 0
    };
    if !ok {
        return 0;
    }
    // Rounded: the two readings are a few hundred nanoseconds apart.
    // `time_t` is not i64 everywhere.
    #[allow(clippy::unnecessary_cast)]
    let diff_ns = (tai.tv_sec - utc.tv_sec) as i64 * 1_000_000_000 + (tai.tv_nsec - utc.tv_nsec);
    (diff_ns + 500_000_000).div_euclid(1_000_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn tai_is_ahead_of_utc() {
        let t = Timescale::tai();
        let offset = t.from_realtime(1000 * S) as i64 - (1000 * S) as i64;
        assert!(offset >= 37 * S as i64, "offset {offset}");
        assert_eq!(offset % S as i64, 0);
        assert!(!t.is_skewed());
    }

    #[test]
    fn skew_accumulates_from_origin() {
        let base = Timescale::tai();
        let t = base.skewed(100.0, 5_000);
        let r = t.origin_ns + 10 * S;
        let diff = t.from_realtime(r) as i64 - base.from_realtime(r) as i64;
        assert_eq!(diff, 5_000 + 1_000_000); // 10 s × 100 ppm = 1 ms
        assert!(t.is_skewed());
    }
}
