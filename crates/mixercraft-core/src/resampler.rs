//! Band-limited, variable-ratio sample-rate conversion.
//!
//! [`Resampler`] implements windowed-sinc interpolation with a precomputed
//! polyphase table (linearly interpolated between phases). The conversion
//! ratio can be changed continuously while running, which is what clock-drift
//! compensation needs: two devices that both claim 48 kHz never run at
//! exactly the same speed, so the effective ratio is `1 ± a few hundred ppm`
//! and is steered by [`crate::drift::DriftController`].
//!
//! The same component performs nominal-rate conversion (e.g. 44.1 kHz ↔
//! 48 kHz). The anti-aliasing cutoff follows the lower of both Nyquist
//! frequencies.

use crate::MAX_CHANNELS;

/// Quality presets trading CPU for stop-band attenuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// 32 taps. Suitable for the low-latency profile.
    Fast,
    /// 64 taps. Default.
    Balanced,
    /// 128 taps.
    High,
}

impl Quality {
    fn half_taps(self) -> usize {
        match self {
            Self::Fast => 16,
            Self::Balanced => 32,
            Self::High => 64,
        }
    }

    fn kaiser_beta(self) -> f64 {
        match self {
            Self::Fast => 7.0,
            Self::Balanced => 8.6,
            Self::High => 10.0,
        }
    }
}

const PHASES: usize = 256;
/// Passband edge relative to the effective Nyquist frequency.
const ROLLOFF: f64 = 0.94;

/// A streaming multichannel resampler working on interleaved `f32` frames.
///
/// Construction allocates; [`Resampler::process`] and all setters are
/// real-time safe.
#[derive(Debug, Clone)]
pub struct Resampler {
    channels: usize,
    half: usize,
    taps: usize,
    table: Vec<f32>,
    coeffs: Vec<f32>,
    hist: Vec<f32>,
    hist_frames: usize,
    cap_frames: usize,
    pos: f64,
    base_step: f64,
    adjust: f64,
}

impl Resampler {
    /// Creates a resampler from `in_rate` to `out_rate`.
    ///
    /// `max_input_block` bounds how many input frames can be buffered per
    /// [`Self::process`] call; larger inputs are consumed partially.
    ///
    /// # Panics
    /// If a rate is zero or `channels` is outside `1..=MAX_CHANNELS`.
    #[must_use]
    pub fn new(channels: usize, in_rate: u32, out_rate: u32, quality: Quality, max_input_block: usize) -> Self {
        assert!(in_rate > 0 && out_rate > 0, "sample rates must be non-zero");
        assert!((1..=MAX_CHANNELS).contains(&channels), "invalid channel count");
        let half = quality.half_taps();
        let taps = 2 * half;
        let base_step = f64::from(in_rate) / f64::from(out_rate);
        let cutoff = ROLLOFF * (1.0 / base_step).min(1.0);
        let table = build_table(half, cutoff, quality.kaiser_beta());
        let cap_frames = taps + max_input_block.max(1);
        let mut r = Self {
            channels,
            half,
            taps,
            table,
            coeffs: vec![0.0; taps],
            hist: vec![0.0; cap_frames * channels],
            hist_frames: 0,
            cap_frames,
            pos: 0.0,
            base_step,
            adjust: 1.0,
        };
        r.reset();
        r
    }

    /// Clears all buffered audio. The first output after a reset is delayed
    /// by [`Self::latency_frames`] input frames of silence.
    pub fn reset(&mut self) {
        self.hist.fill(0.0);
        // Pre-roll with `half` frames of silence so the first real input
        // frame can be interpolated without reading before the buffer start.
        self.hist_frames = self.half;
        self.pos = self.half as f64 - 1.0;
    }

    /// Number of channels.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Group delay of the filter in input frames.
    #[must_use]
    pub fn latency_frames(&self) -> usize {
        self.half
    }

    /// Sets the drift correction factor. `1.0` means the nominal ratio;
    /// values above 1 consume input faster. Real-time safe.
    pub fn set_adjust(&mut self, adjust: f64) {
        self.adjust = adjust;
    }

    /// The current drift correction factor.
    #[must_use]
    pub fn adjust(&self) -> f64 {
        self.adjust
    }

    /// Input frames buffered but not yet consumed by the read position.
    /// Used for latency accounting by the drift controller.
    #[must_use]
    pub fn buffered_frames(&self) -> f64 {
        (self.hist_frames as f64 - self.pos).max(0.0)
    }

    /// Converts as much as possible: buffers up to the free capacity from
    /// `input` and writes up to `output.len() / channels` frames.
    ///
    /// Both slices are interleaved with [`Self::channels`] channels. Returns
    /// `(input_frames_consumed, output_frames_produced)`. Real-time safe.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> (usize, usize) {
        let ch = self.channels;
        let half = self.half;

        // 1. Discard history that can no longer be reached.
        let first_needed = self.pos.floor() as isize - half as isize + 1;
        if first_needed > 0 {
            let drop = (first_needed as usize).min(self.hist_frames);
            self.hist.copy_within(drop * ch..self.hist_frames * ch, 0);
            self.hist_frames -= drop;
            self.pos -= drop as f64;
        }

        // 2. Append input.
        let in_frames = input.len() / ch;
        let take = in_frames.min(self.cap_frames - self.hist_frames);
        let start = self.hist_frames * ch;
        self.hist[start..start + take * ch].copy_from_slice(&input[..take * ch]);
        self.hist_frames += take;

        // 3. Produce.
        let out_frames = output.len() / ch;
        let step = self.base_step * self.adjust;
        let mut produced = 0;
        while produced < out_frames {
            let i = self.pos.floor();
            let idx = i as usize;
            if idx + half >= self.hist_frames {
                break;
            }
            self.compute_coeffs(self.pos - i);
            let base = (idx + 1 - half) * ch;
            let out = &mut output[produced * ch..(produced + 1) * ch];
            for (c, o) in out.iter_mut().enumerate() {
                let mut acc = 0.0f32;
                for (t, k) in self.coeffs.iter().enumerate() {
                    acc += self.hist[base + t * ch + c] * k;
                }
                *o = acc;
            }
            self.pos += step;
            produced += 1;
        }
        (take, produced)
    }

    fn compute_coeffs(&mut self, frac: f64) {
        let pf = frac * PHASES as f64;
        let p = (pf as usize).min(PHASES - 1);
        let a = (pf - p as f64) as f32;
        let row0 = &self.table[p * self.taps..(p + 1) * self.taps];
        let row1 = &self.table[(p + 1) * self.taps..(p + 2) * self.taps];
        for ((c, x0), x1) in self.coeffs.iter_mut().zip(row0).zip(row1) {
            *c = x0 + (x1 - x0) * a;
        }
    }
}

/// Builds `PHASES + 1` rows of `2 * half` taps. Row `p` holds the kernel for
/// fractional offset `p / PHASES`; tap `j` multiplies input sample
/// `i + j - half + 1`. Each row is normalised to unity DC gain.
fn build_table(half: usize, cutoff: f64, beta: f64) -> Vec<f32> {
    let taps = 2 * half;
    let mut table = vec![0.0f32; (PHASES + 1) * taps];
    let i0_beta = bessel_i0(beta);
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut row64 = [0.0f64; 128];
        let mut sum = 0.0;
        for (j, v) in row64.iter_mut().enumerate().take(taps) {
            let x = (j as f64 - half as f64 + 1.0) - frac;
            let w_arg = x / half as f64;
            let window =
                if w_arg.abs() >= 1.0 { 0.0 } else { bessel_i0(beta * (1.0 - w_arg * w_arg).sqrt()) / i0_beta };
            *v = cutoff * sinc(cutoff * x) * window;
            sum += *v;
        }
        for (dst, v) in row.iter_mut().zip(&row64[..taps]) {
            *dst = (*v / sum) as f32;
        }
    }
    table
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let px = std::f64::consts::PI * x;
        px.sin() / px
    }
}

/// Zeroth-order modified Bessel function of the first kind (series).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    for k in 1..64 {
        term *= (half_x / k as f64) * (half_x / k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Runs a whole signal through in fixed-size chunks.
    fn run(r: &mut Resampler, input: &[f32], chunk: usize, out_frames: usize) -> Vec<f32> {
        let ch = r.channels();
        let mut out = vec![0.0; out_frames * ch];
        let mut consumed = 0;
        let mut produced = 0;
        while produced < out_frames {
            let end = (consumed + chunk).min(input.len() / ch);
            let (c, p) = r.process(&input[consumed * ch..end * ch], &mut out[produced * ch..]);
            consumed += c;
            produced += p;
            if c == 0 && p == 0 {
                break;
            }
        }
        out.truncate(produced * ch);
        out
    }

    fn sine(freq: f64, rate: f64, frames: usize) -> Vec<f32> {
        (0..frames).map(|n| (2.0 * PI * freq * n as f64 / rate).sin() as f32).collect()
    }

    /// Fits amplitude and phase of `freq` with least squares and returns the
    /// residual (noise + distortion) relative to the fitted signal in dB.
    fn snr_db(signal: &[f32], freq: f64, rate: f64) -> f64 {
        let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (n, &y) in signal.iter().enumerate() {
            let w = 2.0 * PI * freq * n as f64 / rate;
            let (s, c) = w.sin_cos();
            ss += s * s;
            sc += s * c;
            cc += c * c;
            ys += f64::from(y) * s;
            yc += f64::from(y) * c;
        }
        let det = ss * cc - sc * sc;
        let a = (ys * cc - yc * sc) / det;
        let b = (yc * ss - ys * sc) / det;
        let (mut sig, mut err) = (0.0, 0.0);
        for (n, &y) in signal.iter().enumerate() {
            let w = 2.0 * PI * freq * n as f64 / rate;
            let fit = a * w.sin() + b * w.cos();
            sig += fit * fit;
            err += (f64::from(y) - fit).powi(2);
        }
        10.0 * (sig / err).log10()
    }

    #[test]
    fn unity_ratio_preserves_signal_after_latency() {
        let mut r = Resampler::new(1, 48_000, 48_000, Quality::Balanced, 512);
        let input = sine(1000.0, 48_000.0, 4800);
        let out = run(&mut r, &input, 480, 4000);
        assert_eq!(out.len(), 4000);
        // The read position starts at `half - 1` into a buffer pre-rolled with
        // `half` silent frames: output n corresponds to input n - 1.
        for n in 100..4000 {
            assert!((out[n] - input[n - 1]).abs() < 1e-3, "n={n}");
        }
    }

    #[test]
    fn dc_gain_is_unity_at_any_ratio() {
        for (i, o) in [(44_100, 48_000), (48_000, 44_100), (48_000, 96_000), (96_000, 48_000)] {
            let mut r = Resampler::new(2, i, o, Quality::Balanced, 256);
            let input = vec![0.5f32; 2 * 20_000];
            let out = run(&mut r, &input, 256, 8000);
            let tail = &out[out.len() - 200..];
            assert!(tail.iter().all(|s| (s - 0.5).abs() < 1e-4), "{i}->{o}");
        }
    }

    #[test]
    fn sine_quality_44k1_to_48k() {
        let mut r = Resampler::new(1, 44_100, 48_000, Quality::Balanced, 512);
        let input = sine(1000.0, 44_100.0, 44_100);
        let out = run(&mut r, &input, 441, 40_000);
        let snr = snr_db(&out[1000..], 1000.0, 48_000.0);
        assert!(snr > 90.0, "SNR {snr:.1} dB");
    }

    #[test]
    fn high_frequency_sine_quality_48k_to_44k1() {
        let mut r = Resampler::new(1, 48_000, 44_100, Quality::Balanced, 512);
        let input = sine(15_000.0, 48_000.0, 48_000);
        let out = run(&mut r, &input, 480, 40_000);
        let snr = snr_db(&out[1000..], 15_000.0, 44_100.0);
        assert!(snr > 70.0, "SNR {snr:.1} dB");
    }

    #[test]
    fn downsampling_rejects_content_above_new_nyquist() {
        // 30 kHz cannot be represented at 48 kHz and must be filtered out.
        let mut r = Resampler::new(1, 96_000, 48_000, Quality::Balanced, 512);
        let input = sine(30_000.0, 96_000.0, 48_000);
        let out = run(&mut r, &input, 960, 20_000);
        let rms = (out[1000..].iter().map(|s| s * s).sum::<f32>() / (out.len() - 1000) as f32).sqrt();
        let db = 20.0 * f64::from(rms).log10();
        assert!(db < -70.0, "alias level {db:.1} dB");
    }

    #[test]
    fn drift_adjust_changes_consumption_rate() {
        let mut r = Resampler::new(1, 48_000, 48_000, Quality::Fast, 1024);
        r.set_adjust(1.001);
        let input = vec![0.0f32; 200_000];
        let mut consumed = 0;
        let mut produced = 0;
        let mut out = vec![0.0f32; 480];
        while produced < 100_000 {
            let (c, p) = r.process(&input[consumed..consumed + 1024], &mut out);
            consumed += c;
            produced += p;
        }
        let ratio = (consumed as f64 - r.buffered_frames()) / produced as f64;
        assert!((ratio - 1.001).abs() < 2e-5, "ratio {ratio}");
    }

    #[test]
    fn interleaved_channels_stay_independent() {
        let mut r = Resampler::new(2, 48_000, 44_100, Quality::Fast, 256);
        let input: Vec<f32> = (0..4000).flat_map(|_| [0.25f32, -0.75]).collect();
        let out = run(&mut r, &input, 256, 2000);
        let (l, rr) = (out[out.len() - 2], out[out.len() - 1]);
        assert!((l - 0.25).abs() < 1e-4 && (rr + 0.75).abs() < 1e-4);
    }
}
