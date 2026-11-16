//! Speech denoising for MixerCraft: a vendored copy of `nnnoiseless` 0.5.2
//! (Joe Neeman's Rust port of Xiph's RNNoise, BSD-3-Clause; see `COPYING`
//! and `AUTHORS`).
//!
//! Changes from upstream, all for use on a real-time audio thread:
//! * the FFTs use `realfft` plans and scratch buffers created up front
//!   (upstream's `easyfft` caches plans per thread, so the first frame on
//!   every new thread allocated);
//! * the model is read without `unsafe` (copied once at load);
//! * training, C API and resampling helpers are removed.
//!
//! After [`DenoiseState`] is created (allocates), [`DenoiseState::process_frame`]
//! does not allocate; `tests/realtime_safety.rs` checks this.
#![deny(missing_docs)]
// Vendored code is kept close to upstream to ease updates.
#![allow(clippy::all, missing_debug_implementations, rust_2018_idioms)]

use std::sync::OnceLock as OnceCell;

mod util;

mod denoise;
mod features;
mod pitch;
mod rnn;

pub use denoise::DenoiseState;
pub use features::DenoiseFeatures;
pub use rnn::RnnModel;

#[doc(hidden)]
pub const FRAME_SIZE_SHIFT: usize = 2;
#[doc(hidden)]
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
#[doc(hidden)]
pub const FREQ_SIZE: usize = FRAME_SIZE + 1;

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

#[doc(hidden)]
pub const NB_BANDS: usize = 22;
pub(crate) const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
#[doc(hidden)]
pub const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
#[doc(hidden)]
pub const EBAND_5MS: [usize; 22] = [
    // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];
type Complex = realfft::num_complex::Complex32;

/// Computes the correlation between two frequency-domain signals, and aggregates the correlation
/// into bands.
///
/// `out` is the output (duh), and it has length `NB_BANDS`.
pub(crate) fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            let corr = x[idx].re * p[idx].re + x[idx].im * p[idx].im;
            out[i] += (1.0 - frac) * corr;
            out[i + 1] += frac * corr;
        }
    }
    out[0] *= 2.0;
    out[NB_BANDS - 1] *= 2.0;
}

fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
    for y in out.iter_mut() {
        *y = 0.0;
    }

    for i in 0..(NB_BANDS - 1) {
        let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
        for j in 0..band_size {
            let frac = j as f32 / band_size as f32;
            let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
            out[idx] = (1.0 - frac) * band_e[i] + frac * band_e[i + 1];
        }
    }
}

struct CommonState {
    window: [f32; WINDOW_SIZE],
    dct_table: [f32; NB_BANDS * NB_BANDS],
    wnorm: f32,
}

static COMMON: OnceCell<CommonState> = OnceCell::new();

fn common() -> &'static CommonState {
    if COMMON.get().is_none() {
        let pi = std::f64::consts::PI;
        let mut window = [0.0; WINDOW_SIZE];
        for i in 0..FRAME_SIZE {
            let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
            window[i] = (0.5 * pi * sin * sin).sin() as f32;
            window[WINDOW_SIZE - i - 1] = (0.5 * pi * sin * sin).sin() as f32;
        }
        let wnorm = 1_f32 / window.iter().map(|x| x * x).sum::<f32>();

        let mut dct_table = [0.0; NB_BANDS * NB_BANDS];
        for i in 0..NB_BANDS {
            for j in 0..NB_BANDS {
                dct_table[i * NB_BANDS + j] = ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
                if j == 0 {
                    dct_table[i * NB_BANDS + j] *= 0.5f32.sqrt();
                }
            }
        }

        let _ = COMMON.set(CommonState { window, dct_table, wnorm });
    }
    COMMON.get().unwrap()
}

/// A brute-force DCT (discrete cosine transform) of size NB_BANDS.
pub(crate) fn dct(out: &mut [f32], x: &[f32]) {
    let c = common();
    for i in 0..NB_BANDS {
        let mut sum = 0.0;
        for j in 0..NB_BANDS {
            sum += x[j] * c.dct_table[j * NB_BANDS + i];
        }
        out[i] = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
    }
}

fn apply_window(output: &mut [f32], input: &[f32]) {
    let c = common();
    for (x, &y, &w) in util::zip3(output, input, &c.window[..]) {
        *x = y * w;
    }
}

fn apply_window_in_place(xs: &mut [f32]) {
    let c = common();
    for (x, &w) in xs.iter_mut().zip(&c.window[..]) {
        *x *= w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic "noisy speech" signal: a gliding harmonic tone in
    /// bursts plus broadband noise, in 16-bit range.
    fn signal(frames: usize) -> Vec<f32> {
        let mut seed: u32 = 12345;
        (0..frames * FRAME_SIZE)
            .map(|i| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 3000.0;
                let t = i as f32 / 48_000.0;
                let on = ((t * 2.0) as u32) % 2 == 0;
                let f = 180.0 + 40.0 * (t * 0.7).sin();
                let voice = if on {
                    (1..6).map(|h| (2.0 * std::f32::consts::PI * f * h as f32 * t).sin() / h as f32).sum::<f32>()
                        * 6000.0
                } else {
                    0.0
                };
                voice + noise
            })
            .collect()
    }

    #[test]
    fn matches_upstream_nnnoiseless() {
        let input = signal(300);
        let mut ours = DenoiseState::new();
        let mut theirs = nnnoiseless::DenoiseState::new();
        let (mut a, mut b) = ([0.0f32; FRAME_SIZE], [0.0f32; FRAME_SIZE]);
        let mut max_diff = 0.0f32;
        for chunk in input.chunks_exact(FRAME_SIZE) {
            let va = ours.process_frame(&mut a, chunk);
            let vb = theirs.process_frame(&mut b, chunk);
            assert!((va - vb).abs() < 1e-4, "voice activity differs: {va} vs {vb}");
            for (x, y) in a.iter().zip(&b) {
                max_diff = max_diff.max((x - y).abs());
            }
        }
        // Same arithmetic, so the outputs agree to float rounding.
        assert!(max_diff < 0.5, "max sample difference {max_diff} (16-bit scale)");
    }

    #[test]
    fn removes_most_of_the_noise_in_pauses() {
        // 2 s of noise only, after 1 s to adapt.
        let mut seed: u32 = 7;
        let noise: Vec<f32> = (0..300 * FRAME_SIZE)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 3000.0
            })
            .collect();
        let mut state = DenoiseState::new();
        let mut out = [0.0f32; FRAME_SIZE];
        let (mut e_in, mut e_out) = (0.0f64, 0.0f64);
        for (k, chunk) in noise.chunks_exact(FRAME_SIZE).enumerate() {
            state.process_frame(&mut out, chunk);
            if k >= 100 {
                e_in += chunk.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
                e_out += out.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>();
            }
        }
        let reduction_db = 10.0 * (e_in / e_out.max(1e-9)).log10();
        assert!(reduction_db > 10.0, "only {reduction_db:.1} dB of noise removed");
    }
}
