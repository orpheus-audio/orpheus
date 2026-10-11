//! Jitter buffer: holds received audio by RTP timestamp until it is played.
//!
//! Packets may arrive late, early, out of order, twice or not at all. The
//! buffer is a ring of frames indexed by timestamp; each slot remembers the
//! timestamp it holds, so a read knows exactly which frames arrived and
//! fills the rest with silence. It makes no timing decisions itself: the
//! caller reads each timestamp range at its playout time.

use alloc::vec;
use alloc::vec::Vec;

pub struct JitterBuffer {
    channels: usize,
    /// Capacity in frames minus one; the capacity is a power of two, so slot
    /// indices stay consistent when timestamps wrap.
    mask: u32,
    samples: Vec<f32>,
    stamps: Vec<u32>,
    /// Timestamp the next read starts at; `None` before the first read.
    head: Option<u32>,
}

/// What [`JitterBuffer::insert`] did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    /// Stored, except for frames already played.
    Stored,
    /// Every frame was already played.
    Late,
    /// Too far ahead of the playout position to fit; dropped.
    Early,
}

impl JitterBuffer {
    /// Creates a buffer for `channels` interleaved channels that holds at
    /// least `frames` frames ahead of the playout position.
    pub fn new(channels: usize, frames: usize) -> Self {
        assert!(channels > 0);
        let cap = frames.max(1).next_power_of_two();
        assert!(cap <= 1 << 31);
        let mut buf = Self {
            channels,
            mask: cap as u32 - 1,
            samples: vec![0.0; cap * channels],
            stamps: vec![0; cap],
            head: None,
        };
        buf.reset();
        buf
    }

    pub fn capacity(&self) -> usize {
        self.mask as usize + 1
    }

    /// Forgets all audio and the playout position.
    pub fn reset(&mut self) {
        // A stamp half the timestamp range away from any position in use.
        for (i, s) in self.stamps.iter_mut().enumerate() {
            *s = i as u32 ^ 1 << 31;
        }
        self.samples.fill(0.0);
        self.head = None;
    }

    /// Stores interleaved `samples` starting at timestamp `ts`. A trailing
    /// partial frame is ignored.
    pub fn insert(&mut self, ts: u32, samples: &[f32]) -> Insert {
        let frames = samples.len() / self.channels;
        let mut skip = 0;
        if let Some(head) = self.head {
            let start = ts.wrapping_sub(head) as i32 as i64;
            if start + frames as i64 <= 0 {
                return Insert::Late;
            }
            if start + frames as i64 > self.capacity() as i64 {
                return Insert::Early;
            }
            skip = (-start).max(0) as usize;
        } else if frames > self.capacity() {
            return Insert::Early;
        }
        let ch = self.channels;
        for (i, frame) in samples.chunks_exact(ch).enumerate().skip(skip) {
            let t = ts.wrapping_add(i as u32);
            let slot = (t & self.mask) as usize;
            self.stamps[slot] = t;
            self.samples[slot * ch..][..ch].copy_from_slice(frame);
        }
        Insert::Stored
    }

    /// Plays frames from timestamp `ts` into `out`, with silence for frames
    /// that never arrived, and returns how many were missing.
    pub fn read(&mut self, ts: u32, out: &mut [f32]) -> usize {
        let ch = self.channels;
        let mut missing = 0;
        let mut frames = 0;
        for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
            let t = ts.wrapping_add(i as u32);
            let slot = (t & self.mask) as usize;
            if self.stamps[slot] == t {
                frame.copy_from_slice(&self.samples[slot * ch..][..ch]);
            } else {
                frame.fill(0.0);
                missing += 1;
            }
            frames += 1;
        }
        self.head = Some(ts.wrapping_add(frames as u32));
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(ts: u32, n: usize) -> Vec<f32> {
        // Stereo, both channels carry the timestamp.
        (0..n).flat_map(|i| [(ts as usize + i) as f32; 2]).collect()
    }

    #[test]
    fn reordered_packets_play_in_order() {
        let mut jb = JitterBuffer::new(2, 64);
        assert_eq!(jb.insert(104, &frames(104, 4)), Insert::Stored);
        assert_eq!(jb.insert(100, &frames(100, 4)), Insert::Stored);
        let mut out = [0.0; 16];
        assert_eq!(jb.read(100, &mut out), 0);
        assert_eq!(out.to_vec(), frames(100, 8));
    }

    #[test]
    fn missing_frames_are_silent() {
        let mut jb = JitterBuffer::new(2, 64);
        jb.insert(0, &frames(0, 2));
        jb.insert(4, &frames(4, 2));
        let mut out = [9.0; 12];
        assert_eq!(jb.read(0, &mut out), 2);
        assert_eq!(
            out,
            [0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 4.0, 4.0, 5.0, 5.0]
        );
    }

    #[test]
    fn late_and_early_packets() {
        let mut jb = JitterBuffer::new(2, 8);
        let mut out = [0.0; 8];
        jb.read(100, &mut out); // head is now 104
        assert_eq!(jb.insert(96, &frames(96, 8)), Insert::Late);
        assert_eq!(jb.insert(108, &frames(108, 4)), Insert::Stored); // ends at head+8
        assert_eq!(jb.insert(109, &frames(109, 4)), Insert::Early);
        // Partly late: the unplayed part is kept.
        assert_eq!(jb.insert(102, &frames(102, 4)), Insert::Stored);
        assert_eq!(jb.read(104, &mut out), 2, "106 and 107 never came");
        assert_eq!(out[..4].to_vec(), frames(104, 2));
    }

    #[test]
    fn played_slots_do_not_replay() {
        let mut jb = JitterBuffer::new(1, 4);
        jb.insert(0, &[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0; 4];
        jb.read(0, &mut out);
        // Same slots, one lap later: nothing arrived.
        assert_eq!(jb.read(4, &mut out), 4);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn timestamps_wrap() {
        let mut jb = JitterBuffer::new(2, 16);
        let ts = u32::MAX - 1;
        jb.insert(ts, &frames(0, 4));
        let mut out = [0.0; 8];
        assert_eq!(jb.read(ts, &mut out), 0);
        assert_eq!(out.to_vec(), frames(0, 4));
        assert_eq!(jb.insert(2, &frames(9, 1)), Insert::Stored);
        assert_eq!(jb.insert(ts, &frames(0, 1)), Insert::Late);
    }

    #[test]
    fn reset_forgets_everything() {
        let mut jb = JitterBuffer::new(1, 4);
        jb.insert(0, &[1.0]);
        let mut out = [0.0; 1];
        jb.read(0, &mut out);
        jb.reset();
        assert_eq!(jb.insert(0, &[1.0, 2.0, 3.0, 4.0, 5.0]), Insert::Early);
        assert_eq!(jb.read(0, &mut out), 1);
    }
}
