//! Gain: turning a channel up or down, in decibels, without clicks.
//!
//! A [`SharedGain`] is set from outside the realtime thread; the realtime
//! thread reads it each cycle and a [`Ramp`] moves to it over
//! [`RAMP_FRAMES`], so that a change doesn't jump.

use std::sync::atomic::{AtomicU32, Ordering};

/// The highest gain, in dB.
pub const MAX_DB: f32 = 12.0;
/// The lowest gain other than silence, in dB.
pub const MIN_DB: f32 = -90.0;

/// How long a change of gain takes, in frames: 20 ms at 48 kHz.
pub const RAMP_FRAMES: u32 = 960;

/// The factor a gain of `db` multiplies samples by: 1 for 0 dB.
pub fn linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Whether `db` is a gain Orpheus takes.
pub fn valid(db: f32) -> bool {
    (MIN_DB..=MAX_DB).contains(&db)
}

/// A factor set by one thread and read by the realtime thread.
#[derive(Debug)]
pub struct SharedGain(AtomicU32);

impl SharedGain {
    pub fn new(factor: f32) -> Self {
        Self(AtomicU32::new(factor.to_bits()))
    }

    pub fn set(&self, factor: f32) {
        self.0.store(factor.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Plays a channel at a factor, moving from one factor to the next in a
/// straight line over [`RAMP_FRAMES`].
#[derive(Debug, Clone)]
pub struct Ramp {
    current: f32,
    target: f32,
    step: f32,
    /// Frames until `target`.
    left: u32,
}

impl Ramp {
    /// Starts at `factor`, without a ramp.
    pub fn new(factor: f32) -> Self {
        Self {
            current: factor,
            target: factor,
            step: 0.0,
            left: 0,
        }
    }

    /// The factor reached.
    pub fn current(&self) -> f32 {
        self.current
    }

    /// Writes `input` at `target` to `out`; silence where there is no
    /// input. Realtime safe.
    pub fn process(&mut self, target: f32, input: Option<&[f32]>, out: &mut [f32]) {
        if target != self.target {
            self.target = target;
            self.step = (target - self.current) / RAMP_FRAMES as f32;
            self.left = RAMP_FRAMES;
        }
        let Some(input) = input else {
            out.fill(0.0);
            self.skip(out.len());
            return;
        };
        let n = out.len().min(input.len());
        let mut i = 0;
        while i < n && self.left > 0 {
            self.advance();
            out[i] = input[i] * self.current;
            i += 1;
        }
        let g = self.current;
        if g == 1.0 {
            out[i..n].copy_from_slice(&input[i..n]);
        } else {
            for (o, s) in out[i..n].iter_mut().zip(&input[i..n]) {
                *o = s * g;
            }
        }
        out[n..].fill(0.0);
    }

    fn advance(&mut self) {
        self.left -= 1;
        // From the target back, so that rounding doesn't add up.
        self.current = self.target - self.step * self.left as f32;
    }

    fn skip(&mut self, frames: usize) {
        let n = (frames as u32).min(self.left);
        self.left -= n;
        self.current = self.target - self.step * self.left as f32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibels() {
        assert_eq!(linear(0.0), 1.0);
        assert!((linear(-6.0) - 0.501).abs() < 0.001);
        assert!((linear(-3.0) - 0.708).abs() < 0.001);
        assert!((linear(12.0) - 3.981).abs() < 0.001);
        assert!(valid(0.0) && valid(MAX_DB) && valid(MIN_DB));
        assert!(!valid(12.5) && !valid(-91.0) && !valid(f32::NAN));
    }

    #[test]
    fn unity_leaves_samples_alone() {
        let input: Vec<f32> = (0..256).map(|i| (i as f32 * 0.1).sin()).collect();
        let mut out = vec![9.0; 256];
        Ramp::new(1.0).process(1.0, Some(&input), &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn a_change_ramps_without_jumps() {
        let mut ramp = Ramp::new(1.0);
        let input = vec![1.0; 256];
        let mut played = Vec::new();
        let mut out = vec![0.0; 256];
        for _ in 0..5 {
            ramp.process(0.5, Some(&input), &mut out);
            played.extend_from_slice(&out);
        }
        let step = 0.5 / RAMP_FRAMES as f32;
        for w in played.windows(2) {
            assert!(w[1] <= w[0], "it falls only");
            assert!(w[0] - w[1] <= step * 1.01, "no jump: {} to {}", w[0], w[1]);
        }
        let reached = played.iter().position(|&s| s == 0.5).unwrap();
        assert_eq!(reached, RAMP_FRAMES as usize - 1);
        assert!(played[reached..].iter().all(|&s| s == 0.5));
        assert_eq!(ramp.current(), 0.5);
    }

    #[test]
    fn turning_back_midway_starts_from_where_it_is() {
        let mut ramp = Ramp::new(1.0);
        let input = vec![1.0; 480];
        let mut out = vec![0.0; 480];
        ramp.process(0.0, Some(&input), &mut out);
        let midway = ramp.current();
        assert!((midway - 0.5).abs() < 0.01, "{midway}");
        ramp.process(1.0, Some(&input), &mut out);
        assert!(out[0] > midway && out[0] - midway < 0.002, "{}", out[0]);
    }

    #[test]
    fn no_input_plays_silence_and_still_ramps() {
        let mut ramp = Ramp::new(0.0);
        let mut out = vec![1.0; RAMP_FRAMES as usize];
        ramp.process(1.0, None, &mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(ramp.current(), 1.0);
    }
}
