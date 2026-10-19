//! Levels: how loud a channel plays, for meters.
//!
//! The realtime thread adds each cycle's samples to a [`SharedLevel`];
//! another thread takes what came since, every [`WINDOW`], into a
//! [`Meter`], which holds the peak over the last second and the RMS over
//! the last 300 ms, as level meters do.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// How often a [`Meter`] takes a [`SharedLevel`]'s samples.
pub const WINDOW: Duration = Duration::from_millis(100);
/// How many windows the peak holds: a second.
const PEAK_WINDOWS: usize = 10;
/// How many windows the RMS is over: 300 ms.
const RMS_WINDOWS: usize = 3;
/// Squares are summed in fixed point, which an atomic can add up.
const ENERGY_SCALE: f64 = (1u64 << 32) as f64;

/// Where samples reach full scale: they no longer fit an integer format.
pub const FULL_SCALE: f32 = 1.0;

/// The samples of a channel since they were last taken, added by the
/// realtime thread and taken by another.
#[derive(Debug, Default)]
pub struct SharedLevel {
    /// The largest magnitude, as bits: those of floats that aren't
    /// negative order like the floats.
    peak: AtomicU32,
    /// The sum of the squares, times [`ENERGY_SCALE`].
    energy: AtomicU64,
    frames: AtomicU64,
    /// How many samples reached full scale.
    over: AtomicU64,
}

impl SharedLevel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `samples`. Realtime safe.
    pub fn record(&self, samples: &[f32]) {
        let (mut peak, mut energy, mut over) = (0f32, 0f64, 0u64);
        for &s in samples {
            let a = s.abs();
            peak = peak.max(a);
            energy += f64::from(s) * f64::from(s);
            over += u64::from(a >= FULL_SCALE);
        }
        self.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        self.energy
            .fetch_add((energy * ENERGY_SCALE) as u64, Ordering::Relaxed);
        self.frames
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
        if over > 0 {
            self.over.fetch_add(over, Ordering::Relaxed);
        }
    }

    /// What came since the last time.
    pub fn take(&self) -> Window {
        Window {
            peak: f32::from_bits(self.peak.swap(0, Ordering::Relaxed)),
            energy: self.energy.swap(0, Ordering::Relaxed) as f64 / ENERGY_SCALE,
            frames: self.frames.swap(0, Ordering::Relaxed),
            over: self.over.swap(0, Ordering::Relaxed),
        }
    }
}

/// The samples of a channel over a while.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Window {
    pub peak: f32,
    /// The sum of the squares.
    pub energy: f64,
    pub frames: u64,
    /// How many samples reached full scale.
    pub over: u64,
}

/// How loud a channel plays, as factors of full scale.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Level {
    /// The largest magnitude over the last second.
    pub peak: f32,
    /// Over the last 300 ms.
    pub rms: f32,
}

/// The last windows of a channel.
#[derive(Debug, Clone, Default)]
pub struct Meter {
    windows: VecDeque<Window>,
}

impl Meter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the next window.
    pub fn push(&mut self, window: Window) {
        if self.windows.len() == PEAK_WINDOWS {
            self.windows.pop_front();
        }
        self.windows.push_back(window);
    }

    pub fn level(&self) -> Level {
        let peak = self.windows.iter().map(|w| w.peak).fold(0.0, f32::max);
        let (energy, frames) = self
            .windows
            .iter()
            .rev()
            .take(RMS_WINDOWS)
            .fold((0.0, 0), |(e, f), w| (e + w.energy, f + w.frames));
        let rms = if frames == 0 {
            0.0
        } else {
            (energy / frames as f64).sqrt() as f32
        };
        Level { peak, rms }
    }
}

/// A factor of full scale in dBFS; `None` for silence.
pub fn dbfs(factor: f32) -> Option<f32> {
    (factor > 0.0).then(|| 20.0 * factor.log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(amplitude: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| amplitude * (i as f32 * 2.0 * std::f32::consts::PI / 48.0).sin())
            .collect()
    }

    #[test]
    fn a_sine_has_its_peak_and_rms() {
        let shared = SharedLevel::new();
        for chunk in sine(0.5, 4800).chunks(256) {
            shared.record(chunk);
        }
        let mut meter = Meter::new();
        meter.push(shared.take());
        let l = meter.level();
        assert!((l.peak - 0.5).abs() < 0.001, "{l:?}");
        assert!((l.rms - 0.5 / 2f32.sqrt()).abs() < 0.001, "{l:?}");
        assert!((dbfs(l.peak).unwrap() + 6.02).abs() < 0.01);
        assert_eq!(dbfs(0.0), None);
    }

    #[test]
    fn taking_starts_over() {
        let shared = SharedLevel::new();
        shared.record(&[0.5, -1.5, 1.0]);
        let w = shared.take();
        assert_eq!((w.peak, w.frames, w.over), (1.5, 3, 2));
        assert_eq!(shared.take(), Window::default());
    }

    #[test]
    fn the_peak_holds_a_second_and_the_rms_follows_faster() {
        let mut meter = Meter::new();
        let loud = Window {
            peak: 1.0,
            energy: 4800.0,
            frames: 4800,
            over: 0,
        };
        let quiet = Window {
            frames: 4800,
            ..Window::default()
        };
        meter.push(loud);
        for _ in 0..RMS_WINDOWS {
            meter.push(quiet);
        }
        assert_eq!(
            meter.level(),
            Level {
                peak: 1.0,
                rms: 0.0
            }
        );
        for _ in RMS_WINDOWS..PEAK_WINDOWS {
            meter.push(quiet);
        }
        assert_eq!(meter.level(), Level::default());
    }

    #[test]
    fn silence_is_no_level() {
        let shared = SharedLevel::new();
        shared.record(&[0.0; 256]);
        let mut meter = Meter::new();
        meter.push(shared.take());
        assert_eq!(meter.level(), Level::default());
        assert_eq!(Meter::new().level(), Level::default());
    }
}
