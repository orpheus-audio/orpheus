//! Clocks that give a node its media-clock time.
//!
//! Every node derives RTP timestamps and playout times from a [`Clock`].
//! Nodes that share a clock agree on when each frame is played. The clock
//! counts TAI nanoseconds since the PTP epoch (1970-01-01 TAI), as AES67
//! expects. [`PtpClock`](ptp::PtpClock) synchronizes nodes over the network;
//! [`SystemClock`] is the fallback and relies on NTP keeping the hosts'
//! system clocks within the link offset of each other.

pub mod ptp;
mod timescale;

pub use timescale::Timescale;

/// A source of network-wide time.
pub trait Clock: Send + Sync {
    /// TAI nanoseconds since the PTP epoch.
    fn now_ns(&self) -> u64;
}

/// The host's realtime clock, disciplined by NTP, on the TAI timescale.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    scale: Timescale,
}

impl SystemClock {
    pub fn new() -> Self {
        Self::with_timescale(Timescale::tai())
    }

    /// A system clock on a custom timescale, e.g. a skewed one for tests.
    pub fn with_timescale(scale: Timescale) -> Self {
        Self { scale }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        self.scale.now()
    }
}
