//! Asynchronous sample-rate conversion between the media clock and the
//! audio system's clock.
//!
//! The network threads run on the media clock (PTP), the audio system on its
//! sound card's crystal; the two drift apart by up to a few hundred ppm.
//! Each network thread resamples between them: a control loop watches the
//! fill level of the port to the audio system and nudges the resampling
//! ratio so that the level, and with it the latency, stays where it was when
//! the port started playing. Without it the port would drop or repeat audio
//! every few minutes.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Adjustable, Async, FixedAsync, Resampler, SincInterpolationParameters, WindowFunction,
};

/// Largest correction the control loop applies.
const MAX_CORRECTION: f64 = 1000e-6;

/// Natural period of the control loop once it has locked on. Long, so that
/// the ratio barely moves with the level's jitter; drift changes slowly.
const LOOP_PERIOD_S: f64 = 60.0;

/// Natural period at the start. Short, so that the loop finds the drift
/// before the level strays far; it then lengthens to [`LOOP_PERIOD_S`].
const ACQUIRE_PERIOD_S: f64 = 10.0;

/// How fast the period lengthens, in seconds per second.
const PERIOD_GROWTH: f64 = 0.5;

/// Time constant of each of the two stages of the level filter. The level
/// is a sawtooth with the audio system's cycle; the filter smooths it out.
const LEVEL_TAU_S: f64 = 0.5;

/// Settling time after the port (re)started before the level is taken as
/// the setpoint.
const WARMUP_S: f64 = 3.0;

/// Sinc filter length; its delay is half of it.
const SINC_LEN: usize = 64;

/// Keeps a fill level steady by adjusting a rate. Called once per tick.
pub(crate) struct RateControl {
    tick_s: f64,
    rate: f64,
    /// Current and final natural period of the loop.
    period_s: f64,
    final_period_s: f64,
    alpha: f64,
    /// Both filter stages; the second is the filtered level.
    level: Option<(f64, f64)>,
    setpoint: Option<f64>,
    warmup_ticks: u32,
    integral: f64,
    correction: f64,
}

impl RateControl {
    /// `rate`: frames per second of the level; `tick_s`: seconds between
    /// updates.
    pub fn new(rate: u32, tick_s: f64) -> Self {
        Self::with_periods(rate, tick_s, ACQUIRE_PERIOD_S, LOOP_PERIOD_S)
    }

    fn with_periods(rate: u32, tick_s: f64, start_s: f64, final_s: f64) -> Self {
        let mut control = Self {
            tick_s,
            rate: rate as f64,
            period_s: start_s,
            final_period_s: final_s,
            alpha: tick_s / (LEVEL_TAU_S + tick_s),
            level: None,
            setpoint: None,
            warmup_ticks: 0,
            integral: 0.0,
            correction: 0.0,
        };
        control.restart();
        control
    }

    /// The port started (again): measure a new setpoint. The drift estimate
    /// is kept.
    pub fn restart(&mut self) {
        self.level = None;
        self.setpoint = None;
        self.warmup_ticks = (WARMUP_S / self.tick_s) as u32;
        self.correction = self.integral.clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }

    /// Feeds the current fill level (frames) and returns the correction: by
    /// how much (relative) to produce fewer frames into the port, or consume
    /// more from it. Positive when the level is above the setpoint.
    pub fn update(&mut self, level: f64) -> f64 {
        let (a, b) = match self.level {
            Some((a, b)) => {
                let a = a + self.alpha * (level - a);
                (a, b + self.alpha * (a - b))
            }
            None => (level, level),
        };
        self.level = Some((a, b));
        let filtered = b;
        if self.warmup_ticks > 0 {
            self.warmup_ticks -= 1;
            return self.correction;
        }
        let setpoint = *self.setpoint.get_or_insert(filtered);
        let error = filtered - setpoint;
        // PI loop around an integrator (level' = −rate · correction),
        // critically damped at ωn.
        self.period_s = (self.period_s + PERIOD_GROWTH * self.tick_s).min(self.final_period_s);
        let wn = std::f64::consts::TAU / self.period_s;
        let (kp, ki) = (2.0 * wn / self.rate, wn * wn / self.rate);
        let integral = self.integral + ki * error * self.tick_s;
        // Anti-windup: stop integrating while the correction is saturated.
        // A jump in the level (the audio system rescheduled) saturates it
        // for a while; integrating then would learn a drift that isn't
        // there, and the loop would dig the level far below the setpoint
        // once the jump has drained.
        if (kp * error + integral).abs() < MAX_CORRECTION {
            self.integral = integral;
        }
        self.correction = (kp * error + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.correction
    }

    pub fn correction(&self) -> f64 {
        self.correction
    }

    /// Filtered level minus setpoint, once there is a setpoint.
    #[cfg(test)]
    pub fn error(&self) -> Option<f64> {
        Some(self.level?.1 - self.setpoint?)
    }
}

/// Which side of the resampler has a fixed block size.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Fixed {
    /// Receiving: a fixed packet in, a varying amount into the port.
    Input,
    /// Sending: a varying amount from the port, a fixed packet out.
    Output,
}

/// A resampler with its rate control, for interleaved `f32` audio.
pub(crate) struct Asrc {
    resampler: Async<f32>,
    control: RateControl,
    channels: usize,
    /// Port generation the control loop measured its setpoint in.
    generation: u64,
}

impl Asrc {
    /// `block`: frames on the fixed side per call; one call per `tick_s`.
    pub fn new(rate: u32, channels: usize, block: usize, fixed: Fixed, tick_s: f64) -> Self {
        let params = SincInterpolationParameters::new(SINC_LEN, WindowFunction::BlackmanHarris2);
        let resampler = Async::new_sinc(
            1.0,
            1.0 + 2.0 * MAX_CORRECTION,
            &params,
            block,
            channels,
            match fixed {
                Fixed::Input => FixedAsync::Input,
                Fixed::Output => FixedAsync::Output,
            },
        )
        .expect("valid resampler parameters");
        Self {
            resampler,
            control: RateControl::new(rate, tick_s),
            channels,
            generation: u64::MAX,
        }
    }

    /// Input frames the next [`process`](Self::process) call needs.
    pub fn input_frames_next(&self) -> usize {
        self.resampler.input_frames_next()
    }

    /// Largest number of input frames one call can need.
    pub fn input_frames_max(&self) -> usize {
        self.resampler.input_frames_max()
    }

    /// Largest number of frames one call can produce.
    pub fn output_frames_max(&self) -> usize {
        self.resampler.output_frames_max()
    }

    /// Resamples `input` (exactly [`input_frames_next`](Self::input_frames_next)
    /// frames) into `output` and returns the number of frames written.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        let ch = self.channels;
        let (Ok(i), Ok(mut o)) = (
            InterleavedSlice::new(input, ch, input.len() / ch),
            InterleavedSlice::new_mut(output, ch, output.len() / ch),
        ) else {
            return 0;
        };
        match self.resampler.process_into_buffer(&i, &mut o, None) {
            Ok((_, written)) => written,
            Err(_) => 0,
        }
    }

    /// Updates the ratio from the port's fill level. `generation` changes
    /// whenever the port's latency is set anew; while the port is not
    /// `playing`, the ratio holds.
    pub fn control(&mut self, level_frames: usize, generation: u64, playing: bool) {
        if generation != self.generation {
            self.generation = generation;
            self.control.restart();
        }
        if !playing {
            return;
        }
        let correction = self.control.update(level_frames as f64);
        // Output frames per input frame. Receiving, a high level means
        // producing fewer frames; sending, consuming more. Both lower it.
        let _ = self
            .resampler
            .set_resample_ratio_relative(1.0 - correction, true);
    }

    /// The level jumped for another reason, e.g. the clock was stepped:
    /// measure a new setpoint.
    pub fn restart(&mut self) {
        self.control.restart();
    }

    /// The rate correction currently applied, in ppm.
    pub fn correction_ppm(&self) -> f64 {
        self.control.correction() * 1e6
    }

    /// Delay through the resampler, in frames.
    pub fn delay(&self) -> usize {
        self.resampler.output_delay()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port drained by a sound card `drift` faster than the media clock,
    /// in 256-frame bursts, and filled through the control loop.
    fn simulate(drift: f64, seconds: f64) -> (Vec<f64>, RateControl) {
        let rate = 48_000.0;
        let tick = 0.001;
        let mut c = RateControl::new(48_000, tick);
        let mut level = 1000.0;
        let mut card_phase = 0.0;
        let mut levels = Vec::new();
        for _ in 0..(seconds / tick) as usize {
            // Producer: 48 frames per tick, corrected.
            level += rate * tick * (1.0 - c.correction());
            // Consumer: whole quanta at the card's rate.
            card_phase += rate * tick * (1.0 + drift);
            while card_phase >= 256.0 {
                card_phase -= 256.0;
                level -= 256.0;
            }
            c.update(level);
            levels.push(level);
        }
        (levels, c)
    }

    #[test]
    fn holds_the_level_against_drift() {
        let drift = 200e-6;
        let (levels, c) = simulate(drift, 300.0);
        // The drift estimate converges to the drift (with opposite sign: the
        // producer has to speed up).
        assert!((c.integral + drift).abs() < 2e-6, "{}", c.integral);
        assert!((c.correction() + drift).abs() < 20e-6, "{}", c.correction());
        // The level ends up back at the setpoint, give or take the ripple.
        let error = c.error().unwrap();
        assert!(error.abs() < 2.0, "error {error}");
        // And never strays far on the way: one quantum of sawtooth plus
        // about 1 ms at 200 ppm.
        let min = levels[3000..].iter().cloned().fold(f64::MAX, f64::min);
        let max = levels[3000..].iter().cloned().fold(f64::MIN, f64::max);
        assert!(max - min < 256.0 + 50.0, "range {min}..{max}");
    }

    #[test]
    fn a_jump_in_the_level_doesnt_wind_up_the_drift_estimate() {
        // A sink driven in 1024-frame blocks, drained 48 frames per tick;
        // after the loop has locked on, the level jumps up by one block for
        // good, as when the audio system reschedules.
        let (rate, tick, block, drift) = (48_000.0, 0.001, 1024.0, 50e-6);
        let mut c = RateControl::new(48_000, tick);
        let (mut level, mut card_phase) = (1500.0, 0.0);
        let mut lowest = f64::MAX;
        for i in 0..(400.0 / tick) as usize {
            if i == (200.0 / tick) as usize {
                level += block;
            }
            card_phase += rate * tick * (1.0 + drift);
            while card_phase >= block {
                card_phase -= block;
                level += block;
            }
            level -= rate * tick * (1.0 + c.correction());
            c.update(level);
            if i > (100.0 / tick) as usize {
                lowest = lowest.min(level);
            }
        }
        // It drains the extra block without learning a drift that isn't
        // there, and never digs below the sawtooth it had before the jump.
        assert!((c.integral - drift).abs() < 20e-6, "{}", c.integral);
        assert!(lowest > 1500.0 - block - 100.0, "lowest {lowest}");
    }

    #[test]
    fn restart_keeps_the_drift_estimate() {
        let (_, mut c) = simulate(-100e-6, 200.0);
        let estimate = c.integral;
        assert!((estimate - 100e-6).abs() < 2e-6, "{estimate}");
        c.restart();
        assert_eq!(c.correction(), estimate);
        assert!(c.error().is_none());
    }

    #[test]
    fn resampler_passes_a_sine_through() {
        let mut a = Asrc::new(48_000, 2, 48, Fixed::Input, 0.001);
        let mut out = vec![0.0; a.output_frames_max() * 2];
        let mut played: Vec<f32> = Vec::new();
        for block in 0..200 {
            let input: Vec<f32> = (0..48)
                .flat_map(|i| {
                    let t = (block * 48 + i) as f32 / 48_000.0;
                    [(t * 1000.0 * std::f32::consts::TAU).sin() * 0.5; 2]
                })
                .collect();
            let n = a.process(&input, &mut out);
            played.extend(out[..n * 2].iter().step_by(2));
        }
        // Ratio 1: as many frames out as in, less the filter delay at the
        // start; the tail is the same sine, delayed.
        assert!((played.len() as i64 - 200 * 48).abs() <= 48);
        let peak = played[4000..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        // The delay is where the output matches the input best.
        let sine = |t: usize| (t as f32 / 48.0 * std::f32::consts::TAU).sin() * 0.5;
        let error = |d: usize| {
            (8000..8100)
                .map(|t| (played[t] - sine(t - d)).abs())
                .sum::<f32>()
        };
        let best = (0..64)
            .min_by(|&x, &y| error(x).total_cmp(&error(y)))
            .unwrap();
        assert!(error(best) / 100.0 < 0.001);
        assert!(best.abs_diff(a.delay()) <= 1, "{best} vs {}", a.delay());
    }
}
