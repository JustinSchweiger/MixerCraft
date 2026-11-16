//! Per-strip processing: noise gate, three-band EQ and compressor.
//!
//! Order: (noise suppression, see [`crate::denoise`]) → gate → EQ →
//! compressor, all before the fader. Every stage is
//! skipped entirely when it is off, so a strip without processing is
//! bit-exact. Stereo (or multichannel) strips are processed with linked
//! detection so the image does not shift.
//!
//! The compressor is controlled by one "amount" (0–10) that sets threshold,
//! ratio and make-up gain together, like the single compressor knob users
//! know from software mixers.
//!
//! All methods are real-time safe; coefficient updates are plain arithmetic.

use crate::MAX_CHANNELS;
use crate::buffer::AudioBlock;

/// Gate thresholds at or below this are "off".
pub const GATE_OFF_DB: f32 = -80.0;
/// Highest gate threshold.
pub const GATE_MAX_DB: f32 = -10.0;
/// Largest compressor amount.
pub const COMP_MAX: f32 = 10.0;
/// EQ gain range (±dB).
pub const EQ_RANGE_DB: f32 = 12.0;

const LOW_SHELF_HZ: f64 = 150.0;
const MID_PEAK_HZ: f64 = 1000.0;
const MID_Q: f64 = 0.8;
const HIGH_SHELF_HZ: f64 = 6000.0;
const GATE_HYSTERESIS_DB: f32 = 4.0;
const GATE_HOLD_S: f32 = 0.05;
const GATE_ATTACK_S: f32 = 0.0005;
const GATE_RELEASE_S: f32 = 0.12;
const GATE_ENV_RELEASE_S: f32 = 0.02;
const COMP_ATTACK_S: f32 = 0.005;
const COMP_RELEASE_S: f32 = 0.15;
const COMP_KNEE_DB: f32 = 6.0;
/// States below this are flushed to zero (denormal protection).
const DENORMAL: f32 = 1e-20;

/// User-facing settings of one strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FxParams {
    /// Gate threshold in dBFS; at or below [`GATE_OFF_DB`] the gate is off.
    pub gate_threshold_db: f32,
    /// Compressor amount 0 (off) to [`COMP_MAX`].
    pub comp_amount: f32,
    /// Low shelf gain (dB).
    pub eq_low_db: f32,
    /// Mid peak gain (dB).
    pub eq_mid_db: f32,
    /// High shelf gain (dB).
    pub eq_high_db: f32,
    /// RNNoise speech noise suppression (applied by [`crate::denoise`]).
    pub noise_suppression: bool,
}

impl Default for FxParams {
    fn default() -> Self {
        Self {
            gate_threshold_db: GATE_OFF_DB,
            comp_amount: 0.0,
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            noise_suppression: false,
        }
    }
}

fn clamp_or(v: f32, lo: f32, hi: f32, default: f32) -> f32 {
    if v.is_nan() { default } else { v.clamp(lo, hi) }
}

impl FxParams {
    /// Values clamped to their ranges; NaN becomes "off".
    #[must_use]
    pub fn sanitized(self) -> Self {
        Self {
            gate_threshold_db: clamp_or(self.gate_threshold_db, GATE_OFF_DB, GATE_MAX_DB, GATE_OFF_DB),
            comp_amount: clamp_or(self.comp_amount, 0.0, COMP_MAX, 0.0),
            eq_low_db: clamp_or(self.eq_low_db, -EQ_RANGE_DB, EQ_RANGE_DB, 0.0),
            eq_mid_db: clamp_or(self.eq_mid_db, -EQ_RANGE_DB, EQ_RANGE_DB, 0.0),
            eq_high_db: clamp_or(self.eq_high_db, -EQ_RANGE_DB, EQ_RANGE_DB, 0.0),
            noise_suppression: self.noise_suppression,
        }
    }

    /// Whether every stage is off.
    #[must_use]
    pub fn is_bypass(&self) -> bool {
        !self.noise_suppression && self.dynamics_eq_bypass()
    }

    /// Whether gate, EQ and compressor are off (noise suppression is a
    /// separate stage).
    #[must_use]
    pub fn dynamics_eq_bypass(&self) -> bool {
        self.gate_threshold_db <= GATE_OFF_DB
            && self.comp_amount <= 0.0
            && self.eq_low_db == 0.0
            && self.eq_mid_db == 0.0
            && self.eq_high_db == 0.0
    }
}

/// Threshold (dB), ratio and make-up gain (dB) for a compressor amount.
#[must_use]
pub fn comp_settings(amount: f32) -> (f32, f32, f32) {
    let a = (amount / COMP_MAX).clamp(0.0, 1.0);
    let threshold = -30.0 * a;
    let ratio = 1.0 + 5.0 * a;
    // Half of the static gain reduction at 0 dBFS: keeps loudness roughly
    // constant without pushing peaks into clipping.
    let makeup = -threshold * (1.0 - 1.0 / ratio) * 0.5;
    (threshold, ratio, makeup)
}

fn coef(seconds: f32, sample_rate: f32) -> f32 {
    1.0 - (-1.0 / (seconds * sample_rate)).exp()
}

/// Transposed direct form II biquad with per-channel state.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [[f32; 2]; MAX_CHANNELS],
}

#[derive(Debug, Clone, Copy)]
enum Shape {
    LowShelf,
    Peak,
    HighShelf,
}

impl Biquad {
    const IDENTITY: Self = Self { b: [1.0, 0.0, 0.0], a: [0.0, 0.0], z: [[0.0; 2]; MAX_CHANNELS] };

    /// RBJ audio EQ cookbook coefficients.
    fn design(&mut self, shape: Shape, freq: f64, q: f64, gain_db: f32, sample_rate: f64) {
        let a = 10f64.powf(f64::from(gain_db) / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * (freq / sample_rate).min(0.49);
        let (sin, cos) = w0.sin_cos();
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::Peak => {
                let alpha = sin / (2.0 * q);
                (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
            }
            Shape::LowShelf | Shape::HighShelf => {
                // Shelf slope S = 1.
                let alpha = sin / 2.0 * 2f64.sqrt();
                let k = 2.0 * a.sqrt() * alpha;
                if matches!(shape, Shape::LowShelf) {
                    (
                        a * ((a + 1.0) - (a - 1.0) * cos + k),
                        2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                        a * ((a + 1.0) - (a - 1.0) * cos - k),
                        (a + 1.0) + (a - 1.0) * cos + k,
                        -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                        (a + 1.0) + (a - 1.0) * cos - k,
                    )
                } else {
                    (
                        a * ((a + 1.0) + (a - 1.0) * cos + k),
                        -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                        a * ((a + 1.0) + (a - 1.0) * cos - k),
                        (a + 1.0) - (a - 1.0) * cos + k,
                        2.0 * ((a - 1.0) - (a + 1.0) * cos),
                        (a + 1.0) - (a - 1.0) * cos - k,
                    )
                }
            }
        };
        self.b = [(b0 / a0) as f32, (b1 / a0) as f32, (b2 / a0) as f32];
        self.a = [(a1 / a0) as f32, (a2 / a0) as f32];
    }

    #[inline]
    fn process(&mut self, ch: usize, x: f32) -> f32 {
        let z = &mut self.z[ch];
        let y = self.b[0] * x + z[0];
        z[0] = self.b[1] * x - self.a[0] * y + z[1];
        z[1] = self.b[2] * x - self.a[1] * y;
        y
    }

    fn flush_denormals(&mut self) {
        for z in self.z.iter_mut().flatten() {
            if z.abs() < DENORMAL {
                *z = 0.0;
            }
        }
    }
}

/// Processing state of one strip. No heap allocation.
#[derive(Debug, Clone)]
pub struct StripFx {
    params: FxParams,
    sample_rate: f32,
    eq: [Biquad; 3],
    eq_on: [bool; 3],
    gate_open_lin: f32,
    gate_close_lin: f32,
    gate_env: f32,
    gate_gain: f32,
    gate_open: bool,
    gate_hold: u32,
    comp: (f32, f32, f32),
    comp_gr_db: f32,
}

impl StripFx {
    /// Creates a bypassed processor.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let mut fx = Self {
            params: FxParams::default(),
            sample_rate: sample_rate.max(1) as f32,
            eq: [Biquad::IDENTITY; 3],
            eq_on: [false; 3],
            gate_open_lin: 0.0,
            gate_close_lin: 0.0,
            gate_env: 0.0,
            gate_gain: 1.0,
            gate_open: true,
            gate_hold: 0,
            comp: comp_settings(0.0),
            comp_gr_db: 0.0,
        };
        fx.set_params(FxParams::default());
        fx
    }

    /// Current (sanitised) settings.
    #[must_use]
    pub fn params(&self) -> FxParams {
        self.params
    }

    /// Applies new settings. Real-time safe.
    pub fn set_params(&mut self, params: FxParams) {
        let p = params.sanitized();
        let fs = f64::from(self.sample_rate);
        let bands = [
            (Shape::LowShelf, LOW_SHELF_HZ, 0.707, p.eq_low_db),
            (Shape::Peak, MID_PEAK_HZ, MID_Q, p.eq_mid_db),
            (Shape::HighShelf, HIGH_SHELF_HZ, 0.707, p.eq_high_db),
        ];
        for (i, (shape, f, q, g)) in bands.into_iter().enumerate() {
            let was_on = self.eq_on[i];
            self.eq_on[i] = g != 0.0;
            if self.eq_on[i] {
                if !was_on {
                    self.eq[i].z = [[0.0; 2]; MAX_CHANNELS];
                }
                self.eq[i].design(shape, f, q, g, fs);
            }
        }
        let thr = 10f32.powf(p.gate_threshold_db / 20.0);
        self.gate_open_lin = thr;
        self.gate_close_lin = thr * 10f32.powf(-GATE_HYSTERESIS_DB / 20.0);
        if p.gate_threshold_db <= GATE_OFF_DB {
            self.gate_gain = 1.0;
            self.gate_open = true;
        }
        self.comp = comp_settings(p.comp_amount);
        if p.comp_amount <= 0.0 {
            self.comp_gr_db = 0.0;
        }
        self.params = p;
    }

    /// Current compressor gain reduction in dB (≤ 0), for metering.
    #[must_use]
    pub fn gain_reduction_db(&self) -> f32 {
        self.comp_gr_db
    }

    /// Processes the valid frames of `block` in place. Real-time safe.
    pub fn process(&mut self, block: &mut AudioBlock) {
        if self.params.dynamics_eq_bypass() {
            return;
        }
        let channels = block.channels().min(MAX_CHANNELS);
        let frames = block.frames();
        if self.params.gate_threshold_db > GATE_OFF_DB {
            self.gate(block, channels, frames);
        }
        for band in 0..3 {
            if self.eq_on[band] {
                for ch in 0..channels {
                    let q = &mut self.eq[band];
                    for s in block.channel_mut(ch) {
                        *s = q.process(ch, *s);
                    }
                }
                self.eq[band].flush_denormals();
            }
        }
        if self.params.comp_amount > 0.0 {
            self.compress(block, channels, frames);
        }
    }

    fn peak_at(block: &AudioBlock, channels: usize, i: usize) -> f32 {
        (0..channels).map(|ch| block.channel(ch)[i].abs()).fold(0.0, f32::max)
    }

    fn gate(&mut self, block: &mut AudioBlock, channels: usize, frames: usize) {
        let env_rel = 1.0 - coef(GATE_ENV_RELEASE_S, self.sample_rate);
        let att = coef(GATE_ATTACK_S, self.sample_rate);
        let rel = coef(GATE_RELEASE_S, self.sample_rate);
        let hold = (GATE_HOLD_S * self.sample_rate) as u32;
        for i in 0..frames {
            let peak = Self::peak_at(block, channels, i);
            self.gate_env = if peak > self.gate_env { peak } else { self.gate_env * env_rel };
            if self.gate_env >= self.gate_open_lin {
                self.gate_open = true;
                self.gate_hold = hold;
            } else if self.gate_env < self.gate_close_lin {
                if self.gate_hold > 0 {
                    self.gate_hold -= 1;
                } else {
                    self.gate_open = false;
                }
            }
            let target = if self.gate_open { 1.0 } else { 0.0 };
            self.gate_gain += (target - self.gate_gain) * if self.gate_open { att } else { rel };
            for ch in 0..channels {
                block.channel_mut(ch)[i] *= self.gate_gain;
            }
        }
        if self.gate_env < DENORMAL {
            self.gate_env = 0.0;
        }
        if self.gate_gain < DENORMAL {
            self.gate_gain = 0.0;
        }
    }

    fn compress(&mut self, block: &mut AudioBlock, channels: usize, frames: usize) {
        let (threshold, ratio, makeup) = self.comp;
        let att = coef(COMP_ATTACK_S, self.sample_rate);
        let rel = coef(COMP_RELEASE_S, self.sample_rate);
        let slope = 1.0 / ratio - 1.0;
        for i in 0..frames {
            let peak = Self::peak_at(block, channels, i).max(1e-6);
            let over = 20.0 * peak.log10() - threshold;
            let target = if 2.0 * over < -COMP_KNEE_DB {
                0.0
            } else if 2.0 * over.abs() <= COMP_KNEE_DB {
                slope * (over + COMP_KNEE_DB / 2.0).powi(2) / (2.0 * COMP_KNEE_DB)
            } else {
                slope * over
            };
            let k = if target < self.comp_gr_db { att } else { rel };
            self.comp_gr_db += (target - self.comp_gr_db) * k;
            let gain = 10f32.powf((self.comp_gr_db + makeup) / 20.0);
            for ch in 0..channels {
                block.channel_mut(ch)[i] *= gain;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    const SR: u32 = 48_000;

    fn sine_block(freq: f32, amp: f32, frames: usize, offset: usize) -> AudioBlock {
        let mut b = AudioBlock::new(2, frames);
        for ch in 0..2 {
            for (i, s) in b.channel_mut(ch).iter_mut().enumerate() {
                *s = amp * (2.0 * PI * freq * (i + offset) as f32 / SR as f32).sin();
            }
        }
        b
    }

    /// Runs `seconds` of a sine through `fx` and returns the output RMS (dB)
    /// of the last 100 ms.
    fn steady_db(fx: &mut StripFx, freq: f32, amp: f32, seconds: f32) -> f32 {
        let block = 480;
        let n = (seconds * SR as f32) as usize / block;
        let mut sum = 0.0f64;
        let mut count = 0;
        for k in 0..n {
            let mut b = sine_block(freq, amp, block, k * block);
            fx.process(&mut b);
            if k >= n - 10 {
                for s in b.channel(0) {
                    sum += f64::from(*s) * f64::from(*s);
                    count += 1;
                }
            }
        }
        (10.0 * (sum / count as f64).log10()) as f32
    }

    fn rms_db(amp: f32) -> f32 {
        20.0 * (amp / 2f32.sqrt()).log10()
    }

    #[test]
    fn bypass_is_bit_exact() {
        let mut fx = StripFx::new(SR);
        let original = sine_block(440.0, 0.5, 256, 0);
        let mut b = original.clone();
        fx.process(&mut b);
        assert_eq!(b.channel(0), original.channel(0));
        fx.set_params(FxParams { eq_mid_db: 3.0, ..FxParams::default() });
        fx.set_params(FxParams::default());
        fx.process(&mut b);
        assert_eq!(b.channel(1), original.channel(1), "bypass again after turning a band off");
    }

    #[test]
    fn eq_bands_hit_their_gain_where_they_should() {
        let cases = [
            (FxParams { eq_low_db: 6.0, ..FxParams::default() }, 40.0, 6.0),
            (FxParams { eq_low_db: 6.0, ..FxParams::default() }, 5000.0, 0.0),
            (FxParams { eq_mid_db: -9.0, ..FxParams::default() }, 1000.0, -9.0),
            (FxParams { eq_mid_db: -9.0, ..FxParams::default() }, 60.0, 0.0),
            (FxParams { eq_high_db: 12.0, ..FxParams::default() }, 16000.0, 12.0),
            (FxParams { eq_high_db: 12.0, ..FxParams::default() }, 100.0, 0.0),
        ];
        for (p, f, expected) in cases {
            let mut fx = StripFx::new(SR);
            fx.set_params(p);
            let got = steady_db(&mut fx, f, 0.1, 0.5) - rms_db(0.1);
            assert!((got - expected).abs() < 0.8, "{p:?} at {f} Hz: {got:.2} dB, expected {expected}");
        }
    }

    #[test]
    fn gate_closes_below_threshold_and_opens_above() {
        let mut fx = StripFx::new(SR);
        fx.set_params(FxParams { gate_threshold_db: -40.0, ..FxParams::default() });
        // -60 dBFS hum: gated to (near) silence.
        let quiet = steady_db(&mut fx, 100.0, 0.001, 1.0);
        assert!(quiet < -100.0, "gated level {quiet:.1} dB");
        // -10 dBFS voice-level signal: passes unchanged.
        let loud = steady_db(&mut fx, 300.0, 0.3, 0.5) - rms_db(0.3);
        assert!(loud.abs() < 0.1, "open gate changed the level by {loud:.2} dB");
    }

    #[test]
    fn compressor_reduces_dynamics_as_specified() {
        let (thr, ratio, makeup) = comp_settings(COMP_MAX);
        assert_eq!((thr, ratio), (-30.0, 6.0));
        let mut fx = StripFx::new(SR);
        fx.set_params(FxParams { comp_amount: COMP_MAX, ..FxParams::default() });
        // A -6 dBFS peak sine is 24 dB over the threshold: 20 dB reduction.
        let loud = steady_db(&mut fx, 500.0, 0.5, 1.0) - rms_db(0.5);
        let expected = -(24.0 * (1.0 - 1.0 / ratio)) + makeup;
        assert!((loud - expected).abs() < 1.0, "loud: {loud:.2} dB, expected {expected:.2}");
        // A -50 dBFS signal is below the knee: only make-up gain.
        let quiet = steady_db(&mut fx, 500.0, 0.00316, 1.5) - rms_db(0.00316);
        assert!((quiet - makeup).abs() < 1.0, "quiet: {quiet:.2} dB, expected {makeup:.2}");
        assert!(fx.gain_reduction_db() > -1.0);
    }

    #[test]
    fn parameters_are_sanitised() {
        let p = FxParams {
            gate_threshold_db: f32::NAN,
            comp_amount: 99.0,
            eq_low_db: -40.0,
            eq_mid_db: 0.0,
            eq_high_db: f32::INFINITY,
            noise_suppression: true,
        }
        .sanitized();
        assert_eq!(p.gate_threshold_db, GATE_OFF_DB);
        assert_eq!(p.comp_amount, COMP_MAX);
        assert_eq!(p.eq_low_db, -EQ_RANGE_DB);
        assert_eq!(p.eq_high_db, EQ_RANGE_DB);
        assert!(p.noise_suppression);
    }

    #[test]
    fn silence_after_signal_leaves_no_denormals() {
        let mut fx = StripFx::new(SR);
        fx.set_params(FxParams {
            gate_threshold_db: -50.0,
            comp_amount: 5.0,
            eq_low_db: 6.0,
            eq_mid_db: -3.0,
            eq_high_db: 4.0,
            noise_suppression: false,
        });
        steady_db(&mut fx, 200.0, 0.5, 0.2);
        let mut silent = AudioBlock::new(2, 480);
        for _ in 0..2000 {
            silent.clear();
            fx.process(&mut silent);
        }
        for band in &fx.eq {
            assert!(band.z.iter().flatten().all(|z| *z == 0.0 || z.abs() >= DENORMAL));
        }
        assert!(silent.channel(0).iter().all(|s| *s == 0.0 || s.is_normal()));
    }
}
