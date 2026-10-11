//! Lock-free audio ports between the realtime audio thread and the engine.
//!
//! A port pair is a single-producer single-consumer ring of interleaved `f32`
//! samples. The [`CapturePort`] side is fed by whoever produces audio (the
//! backend's capture callback, later the network receiver), the
//! [`PlaybackPort`] side is drained by whoever consumes it. Both methods are
//! realtime-safe: no allocation, no locks, no syscalls.
//!
//! The playback side owns the latency policy: after each read, `target`
//! frames stay in the ring, so audio is delayed by exactly `target` frames
//! when both sides run in the same cycle. Before it starts playing it waits
//! until the ring holds `target` frames plus one read and drops anything
//! beyond that ("priming"). It primes again after an underrun, and drops audio
//! when the ring has grown far beyond `target`, so the latency stays bounded
//! when the producer runs ahead.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rtrb::{Consumer, Producer, RingBuffer};

/// Counters shared by both ends of a port pair. Readable from any thread.
#[derive(Debug, Default)]
pub struct PortStats {
    underruns: AtomicU64,
    overruns: AtomicU64,
    resyncs: AtomicU64,
    level_frames: AtomicU64,
}

/// A point-in-time copy of [`PortStats`].
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    /// The playback side ran dry while playing.
    pub underruns: u64,
    /// The capture side found the ring full and dropped audio.
    pub overruns: u64,
    /// The playback side dropped audio to bring the latency back to target.
    pub resyncs: u64,
    /// Frames in the ring at the last playback callback.
    pub level_frames: u64,
}

impl PortStats {
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            underruns: self.underruns.load(Relaxed),
            overruns: self.overruns.load(Relaxed),
            resyncs: self.resyncs.load(Relaxed),
            level_frames: self.level_frames.load(Relaxed),
        }
    }
}

/// Creates a connected port pair for `channels` interleaved channels that
/// aims to keep `target_frames` frames buffered.
pub fn port_pair(
    channels: u16,
    target_frames: usize,
) -> (CapturePort, PlaybackPort, Arc<PortStats>) {
    assert!(channels > 0, "a port needs at least one channel");
    let ch = channels as usize;
    let capacity_frames = (4 * target_frames).max(target_frames + 16_384);
    let (producer, consumer) = RingBuffer::new(capacity_frames * ch);
    let stats = Arc::new(PortStats::default());
    let capture = CapturePort {
        producer,
        channels: ch,
        stats: stats.clone(),
    };
    let playback = PlaybackPort {
        consumer,
        channels: ch,
        target: target_frames * ch,
        priming: true,
        stats: stats.clone(),
    };
    (capture, playback, stats)
}

/// Rounds a sample count down to whole frames.
fn whole_frames(samples: usize, channels: usize) -> usize {
    samples - samples % channels
}

/// The writing end of a port pair.
pub struct CapturePort {
    producer: Producer<f32>,
    channels: usize,
    stats: Arc<PortStats>,
}

impl CapturePort {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Appends interleaved samples. A trailing partial frame is ignored; if
    /// the ring is full, the samples that do not fit are dropped and counted
    /// as an overrun.
    pub fn push(&mut self, samples: &[f32]) {
        let want = whole_frames(samples.len(), self.channels);
        let n = want.min(whole_frames(self.producer.slots(), self.channels));
        if n > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(n)
        {
            chunk.fill_from_iter(samples[..n].iter().copied());
        }
        if n < want {
            self.stats.overruns.fetch_add(1, Relaxed);
        }
    }
}

/// The reading end of a port pair.
pub struct PlaybackPort {
    consumer: Consumer<f32>,
    channels: usize,
    /// Target fill level in samples.
    target: usize,
    priming: bool,
    stats: Arc<PortStats>,
}

impl PlaybackPort {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Fills `out` with interleaved samples, writing silence where no audio
    /// is available.
    pub fn fill(&mut self, out: &mut [f32]) {
        let want = whole_frames(out.len(), self.channels);
        let mut avail = whole_frames(self.consumer.slots(), self.channels);
        self.stats
            .level_frames
            .store((avail / self.channels) as u64, Relaxed);

        // Starting (again): play once one read and the target fit, from
        // exactly `target` behind the newest audio.
        if self.priming {
            let need = self.target + want.max(self.channels);
            if avail < need {
                out.fill(0.0);
                return;
            }
            self.priming = false;
            avail -= self.skip(avail - need);
        }

        // The producer ran ahead (e.g. we were not scheduled for a while):
        // drop the oldest audio to get back to the target latency.
        let slack = self.target.max(want);
        if avail > self.target + want + slack {
            avail -= self.skip(avail - self.target - want);
            self.stats.resyncs.fetch_add(1, Relaxed);
        }

        let n = want.min(avail);
        if n > 0
            && let Ok(chunk) = self.consumer.read_chunk(n)
        {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
        }
        out[n..].fill(0.0);
        if n < want {
            self.stats.underruns.fetch_add(1, Relaxed);
            self.priming = true;
        }
    }

    /// Drops the oldest `samples` (whole frames) and returns how many it
    /// dropped.
    fn skip(&mut self, samples: usize) -> usize {
        match self.consumer.read_chunk(samples) {
            Ok(chunk) => {
                chunk.commit_all();
                samples
            }
            Err(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(start: f32, len: usize) -> Vec<f32> {
        (0..len).map(|i| start + i as f32).collect()
    }

    #[test]
    fn primes_before_playing() {
        let (mut cap, mut play, stats) = port_pair(2, 4);
        let mut out = [1.0; 4]; // 2 frames
        cap.push(&ramp(0.0, 10)); // 5 frames < target 4 + read 2
        play.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        cap.push(&ramp(10.0, 2)); // 6 frames
        play.fill(&mut out);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(stats.snapshot().underruns, 0);
        assert_eq!(play.consumer.slots(), 8, "target stays buffered");
    }

    #[test]
    fn priming_drops_to_exact_delay() {
        // Same-cycle producer and consumer: each cycle pushes then reads 3.
        let (mut cap, mut play, _) = port_pair(1, 5);
        let mut out = [0.0; 3];
        let mut played = Vec::new();
        for cycle in 0..6 {
            cap.push(&ramp(cycle as f32 * 3.0, 3));
            play.fill(&mut out);
            played.extend_from_slice(&out);
        }
        // Sample t is heard at t + 5 once playing.
        let first = played.iter().position(|&s| s != 0.0).unwrap();
        assert_eq!(played[first] as usize + 5, first);
        assert!(played[first..].windows(2).all(|w| w[1] == w[0] + 1.0));
    }

    #[test]
    fn underrun_pads_with_silence_and_reprimes() {
        let (mut cap, mut play, stats) = port_pair(1, 2);
        let mut out = [9.0; 4];
        cap.push(&ramp(1.0, 6));
        play.fill(&mut out); // primed: 4 read, 2 kept
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        cap.push(&[7.0]);
        play.fill(&mut out);
        assert_eq!(out, [5.0, 6.0, 7.0, 0.0]);
        assert_eq!(stats.snapshot().underruns, 1);

        // Below target + one read, so we stay silent.
        cap.push(&[8.0]);
        play.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        assert_eq!(stats.snapshot().underruns, 1, "priming is not an underrun");
    }

    #[test]
    fn full_ring_counts_overrun() {
        let (mut cap, _play, stats) = port_pair(1, 1);
        let capacity = 1 + 16_384;
        cap.push(&vec![0.0; capacity]);
        assert_eq!(stats.snapshot().overruns, 0);
        cap.push(&[1.0]);
        assert_eq!(stats.snapshot().overruns, 1);
    }

    #[test]
    fn excess_is_dropped_to_target() {
        let (mut cap, mut play, stats) = port_pair(1, 2);
        let mut out = [0.0; 2];
        cap.push(&ramp(0.0, 4));
        play.fill(&mut out);
        assert_eq!(out, [0.0, 1.0]);
        cap.push(&ramp(4.0, 10));
        play.fill(&mut out);
        // 12 buffered > target 2 + read 2 + slack 2: keep the newest 4,
        // play 2 of them.
        assert_eq!(out, [10.0, 11.0]);
        assert_eq!(stats.snapshot().resyncs, 1);
    }

    #[test]
    fn partial_frames_are_ignored() {
        let (mut cap, mut play, _) = port_pair(2, 0);
        cap.push(&[1.0, 2.0, 3.0]); // trailing sample dropped
        let mut out = [9.0; 3];
        play.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 0.0]);
    }
}
