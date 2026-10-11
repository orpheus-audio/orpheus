//! Lock-free audio ports between the realtime audio thread and the engine.
//!
//! A port pair is a single-producer single-consumer ring of interleaved `f32`
//! samples. The [`CapturePort`] side is fed by whoever produces audio (the
//! backend's capture callback, later the network receiver), the
//! [`PlaybackPort`] side is drained by whoever consumes it. Both methods are
//! realtime-safe: no allocation, no locks, no syscalls.
//!
//! The playback side owns the latency policy: it waits until the ring holds
//! `target` frames before it starts playing ("priming"), primes again after an
//! underrun, and drops audio when the ring has grown far beyond `target`, so
//! the latency stays bounded when the producer runs ahead.

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

        if self.priming {
            if avail < self.target.max(self.channels) {
                out.fill(0.0);
                return;
            }
            self.priming = false;
        }

        // The producer ran ahead (e.g. we were not scheduled for a while):
        // drop the oldest audio to get back to the target latency.
        let slack = self.target.max(want);
        if avail > self.target + slack {
            let excess = avail - self.target;
            if let Ok(chunk) = self.consumer.read_chunk(excess) {
                chunk.commit_all();
                avail = self.target;
                self.stats.resyncs.fetch_add(1, Relaxed);
            }
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
        let mut out = [1.0; 4];
        cap.push(&ramp(0.0, 6)); // 3 frames < target 4
        play.fill(&mut out);
        assert_eq!(out, [0.0; 4]);
        cap.push(&ramp(6.0, 2)); // 4 frames
        play.fill(&mut out);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(stats.snapshot().underruns, 0);
    }

    #[test]
    fn underrun_pads_with_silence_and_reprimes() {
        let (mut cap, mut play, stats) = port_pair(1, 2);
        cap.push(&[1.0, 2.0, 3.0]);
        let mut out = [9.0; 4];
        play.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 0.0]);
        assert_eq!(stats.snapshot().underruns, 1);

        // One sample is below the target of 2, so we stay silent.
        cap.push(&[4.0]);
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
        cap.push(&ramp(0.0, 10));
        let mut out = [0.0; 2];
        play.fill(&mut out);
        // 10 buffered > target 2 + slack 2: keep the newest 2 and play them.
        assert_eq!(out, [8.0, 9.0]);
        assert_eq!(stats.snapshot().resyncs, 1);
    }

    #[test]
    fn partial_frames_are_ignored() {
        let (mut cap, mut play, _) = port_pair(2, 1);
        cap.push(&[1.0, 2.0, 3.0]); // trailing sample dropped
        let mut out = [9.0; 3];
        play.fill(&mut out);
        assert_eq!(out, [1.0, 2.0, 0.0]);
    }
}
