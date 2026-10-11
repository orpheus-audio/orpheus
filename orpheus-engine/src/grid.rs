//! Pacing for network threads on the media-clock grid.

use std::time::Duration;

use orpheus_core::timebase::MediaClock;

/// How far a thread may fall behind (or the clock jump back) before it
/// gives up catching up and restarts at the current time.
const MAX_SLIP_MS: u64 = 100;

/// Ticks every `step` frames of media-clock time.
pub(crate) struct Grid {
    media: MediaClock,
    step: u64,
    max_slip: u64,
    next: u64,
}

pub(crate) enum Tick {
    /// The next tick is this far away.
    Wait(Duration),
    /// The next tick is due; handle it and call [`Grid::advance`].
    Due,
    /// The thread was stalled or the clock jumped; the grid moved to the
    /// current time.
    Realigned,
}

impl Grid {
    pub fn new(media: MediaClock, step: u64, now_ns: u64) -> Self {
        let mut grid = Self {
            media,
            step,
            max_slip: media.frames_at(MAX_SLIP_MS * 1_000_000),
            next: 0,
        };
        grid.align(now_ns);
        grid
    }

    /// Frame index of the next tick.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Checks whether frame `next + lead` has been reached.
    pub fn poll(&mut self, now_ns: u64, lead: u64) -> Tick {
        let now = self.media.frames_at(now_ns);
        let due = self.next + lead;
        if now > due + self.max_slip || due > now + lead + self.max_slip {
            self.align(now_ns);
            return Tick::Realigned;
        }
        let due_ns = self.media.ns_at(due);
        if due_ns > now_ns {
            Tick::Wait(Duration::from_nanos(due_ns - now_ns))
        } else {
            Tick::Due
        }
    }

    pub fn advance(&mut self) {
        self.next += self.step;
    }

    fn align(&mut self, now_ns: u64) {
        self.next = self.media.frames_at(now_ns).div_ceil(self.step) * self.step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    #[test]
    fn waits_ticks_and_realigns() {
        let media = MediaClock::new(48_000);
        let t0 = 1_000 * MS + MS / 2;
        let mut g = Grid::new(media, 48, t0);
        assert_eq!(g.next(), 48_048);
        match g.poll(t0, 48) {
            Tick::Wait(d) => assert_eq!(d, Duration::from_nanos(MS + MS / 2)),
            _ => panic!("should wait"),
        }
        assert!(matches!(g.poll(t0 + 2 * MS, 48), Tick::Due));
        g.advance();
        assert_eq!(g.next(), 48_096);
        // A second-long stall: start over at the current time.
        assert!(matches!(g.poll(t0 + 1_000 * MS, 0), Tick::Realigned));
        assert_eq!(g.next(), 96_048);
        // Clock stepped back.
        assert!(matches!(g.poll(t0, 0), Tick::Realigned));
        assert_eq!(g.next(), 48_048);
    }
}
