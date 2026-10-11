//! Clocks that give a node its media-clock time.
//!
//! Every node derives RTP timestamps and playout times from a [`Clock`].
//! Nodes that share a clock (later: PTP) agree on when each frame is played.
//! Until PTP is in place, [`SystemClock`] relies on NTP keeping the hosts'
//! system clocks within the link offset of each other.

use std::time::{SystemTime, UNIX_EPOCH};

/// A source of network-wide time.
pub trait Clock: Send + Sync {
    /// Nanoseconds since the clock's epoch.
    fn now_ns(&self) -> u64;
}

/// The host's realtime clock (`CLOCK_REALTIME`), disciplined by NTP.
///
/// Uses the Unix epoch, not PTP's TAI epoch, so it only interoperates with
/// other nodes on the same kind of clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    }
}
