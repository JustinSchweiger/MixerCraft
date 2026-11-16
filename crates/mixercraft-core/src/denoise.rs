//! Speech noise suppression (RNNoise) for a strip.
//!
//! RNNoise works on 10 ms frames of 480 samples at 48 kHz. The stage buffers
//! one frame, and RNNoise's overlap-add adds another, so while it is on the
//! signal is delayed by exactly 960 samples (20 ms). At other engine rates it
//! is not available and passes audio through.
//!
//! Switching on or off crossfades between the dry and the denoised signal
//! over one frame; switching on first primes the denoiser for two frames so
//! its start-up artefacts are never heard. When off and settled, audio is
//! untouched (bit-exact).
//!
//! All state is allocated by [`Denoiser::new`]; processing is real-time safe
//! (`mixercraft-denoise` is checked to be allocation-free).

use std::sync::LazyLock;

use mixercraft_denoise::{DenoiseState, FRAME_SIZE, RnnModel};

use crate::buffer::AudioBlock;

/// The only rate RNNoise supports.
pub const DENOISE_RATE: u32 = 48_000;
/// Added latency while on: one frame of buffering plus RNNoise's own frame
/// (20 ms at 48 kHz).
pub const DENOISE_LATENCY_FRAMES: usize = 2 * FRAME_SIZE;

/// RNNoise works on 16-bit-range samples.
const SCALE: f32 = 32_768.0;
/// Frames run through the denoiser before its output is faded in.
const PRIME_FRAMES: u32 = 2;

/// One shared copy of the network weights for every denoiser.
static MODEL: LazyLock<RnnModel> = LazyLock::new(RnnModel::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Untouched audio.
    Off,
    /// Running, output not used yet (start-up artefacts).
    Priming(u32),
    /// Crossfading dry → denoised.
    FadingIn,
    /// Denoised audio.
    On,
    /// Crossfading denoised → dry.
    FadingOut,
}

/// Noise suppression for up to the channels of one strip.
pub struct Denoiser {
    states: Vec<Box<DenoiseState<'static>>>,
    input: Vec<[f32; FRAME_SIZE]>,
    output: Vec<[f32; FRAME_SIZE]>,
    pos: usize,
    available: bool,
    phase: Phase,
    /// 0 = dry, 1 = denoised.
    mix: f32,
    step: f32,
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser")
            .field("channels", &self.states.len())
            .field("available", &self.available)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl Denoiser {
    /// Creates a denoiser for `channels` channels at `sample_rate`. Allocates;
    /// call on a control thread.
    #[must_use]
    pub fn new(channels: usize, sample_rate: u32) -> Self {
        let available = sample_rate == DENOISE_RATE;
        let n = if available { channels } else { 0 };
        Self {
            states: (0..n).map(|_| DenoiseState::with_model(&MODEL)).collect(),
            input: vec![[0.0; FRAME_SIZE]; n],
            output: vec![[0.0; FRAME_SIZE]; n],
            pos: 0,
            available,
            phase: Phase::Off,
            mix: 0.0,
            step: 1.0 / FRAME_SIZE as f32,
        }
    }

    /// Whether noise suppression works at this sample rate.
    #[must_use]
    pub fn available(&self) -> bool {
        self.available
    }

    /// Whether audio is currently being denoised (or crossfaded).
    #[must_use]
    pub fn active(&self) -> bool {
        self.phase != Phase::Off
    }

    /// Switches noise suppression on or off. Real-time safe.
    pub fn set_enabled(&mut self, on: bool) {
        if !self.available {
            return;
        }
        self.phase = match (on, self.phase) {
            (true, Phase::Off) => {
                // Start from a clean history.
                for buf in self.input.iter_mut().chain(self.output.iter_mut()) {
                    buf.fill(0.0);
                }
                self.pos = 0;
                self.mix = 0.0;
                Phase::Priming(PRIME_FRAMES)
            }
            (true, Phase::FadingOut) => Phase::FadingIn,
            (false, Phase::Priming(_)) => Phase::Off,
            (false, Phase::FadingIn | Phase::On) => Phase::FadingOut,
            (_, p) => p,
        };
    }

    /// Processes the valid frames of `block` in place. Real-time safe.
    pub fn process(&mut self, block: &mut AudioBlock) {
        if self.phase == Phase::Off {
            return;
        }
        let channels = block.channels().min(self.states.len());
        for i in 0..block.frames() {
            for ch in 0..channels {
                let sample = &mut block.channel_mut(ch)[i];
                let dry = *sample;
                let wet = self.output[ch][self.pos] / SCALE;
                self.input[ch][self.pos] = dry * SCALE;
                *sample = dry + (wet - dry) * self.mix;
            }
            self.mix = match self.phase {
                Phase::FadingIn => (self.mix + self.step).min(1.0),
                Phase::FadingOut => (self.mix - self.step).max(0.0),
                _ => self.mix,
            };
            if self.phase == Phase::FadingIn && self.mix >= 1.0 {
                self.phase = Phase::On;
            }
            if self.phase == Phase::FadingOut && self.mix <= 0.0 {
                self.phase = Phase::Off;
            }
            self.pos += 1;
            if self.pos == FRAME_SIZE {
                self.pos = 0;
                for ch in 0..channels {
                    self.states[ch].process_frame(&mut self.output[ch], &self.input[ch]);
                }
                if let Phase::Priming(n) = self.phase {
                    self.phase = if n <= 1 { Phase::FadingIn } else { Phase::Priming(n - 1) };
                }
            }
        }
        // Remaining channels (a strip wider than the denoiser) are left as is.
        if self.phase == Phase::Off {
            self.mix = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise_block(seed: &mut u32, frames: usize, amp: f32) -> AudioBlock {
        let mut b = AudioBlock::new(2, frames);
        for i in 0..frames {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let v = ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * amp;
            b.channel_mut(0)[i] = v;
            b.channel_mut(1)[i] = v;
        }
        b
    }

    fn energy(b: &AudioBlock) -> f64 {
        b.channel(0).iter().map(|s| f64::from(*s).powi(2)).sum()
    }

    #[test]
    fn off_is_bit_exact_and_unavailable_rates_pass_through() {
        let mut seed = 1;
        let original = noise_block(&mut seed, 480, 0.1);
        let mut d = Denoiser::new(2, DENOISE_RATE);
        let mut b = original.clone();
        d.process(&mut b);
        assert_eq!(b.channel(0), original.channel(0));

        let mut d = Denoiser::new(2, 44_100);
        assert!(!d.available());
        d.set_enabled(true);
        let mut b = original.clone();
        d.process(&mut b);
        assert_eq!(b.channel(1), original.channel(1));
    }

    /// Speech-like bursts (0.5 s on, 0.5 s off) over white noise. Returns the
    /// level change of speech and of the pauses (dB), skipping 50 ms after
    /// each boundary (the output is delayed and RNNoise releases gently).
    fn speech_and_pause_change(d: &mut Denoiser, noise_amp: f32, seed: u32) -> (f64, f64) {
        let mut seed = seed;
        let (mut sp_in, mut sp_out, mut pa_in, mut pa_out) = (0.0, 0.0, 0.0, 0.0);
        let mut history = [false; 6];
        for k in 0..600 {
            let mut b = AudioBlock::new(1, 480);
            let mut voiced = false;
            for (i, s) in b.channel_mut(0).iter_mut().enumerate() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 2.0 * noise_amp;
                let t = (k * 480 + i) as f32 / 48_000.0;
                voiced = ((t * 2.0) as u32).is_multiple_of(2);
                let f = 140.0 + 30.0 * (t * 3.0).sin();
                let v: f32 = if voiced {
                    (1..8).map(|h| (std::f32::consts::TAU * f * h as f32 * t).sin() / h as f32).sum::<f32>() * 0.15
                } else {
                    0.0
                };
                *s = v + noise;
            }
            let input = energy(&b);
            d.process(&mut b);
            history.rotate_left(1);
            history[5] = voiced;
            if k >= 200 && history.iter().all(|v| *v == voiced) {
                if voiced {
                    sp_in += input;
                    sp_out += energy(&b);
                } else {
                    pa_in += input;
                    pa_out += energy(&b);
                }
            }
        }
        (10.0 * (sp_out / sp_in).log10(), 10.0 * (pa_out / pa_in).log10())
    }

    #[test]
    fn keeps_speech_and_removes_noise_in_pauses() {
        for (noise_amp, seed) in [(0.02, 7), (0.005, 1), (0.05, 99)] {
            let mut d = Denoiser::new(1, DENOISE_RATE);
            d.set_enabled(true);
            let (speech, pauses) = speech_and_pause_change(&mut d, noise_amp, seed);
            assert!(speech > -1.0, "speech changed by {speech:.1} dB (noise {noise_amp})");
            assert!(pauses < -15.0, "noise in pauses changed by only {pauses:.1} dB (noise {noise_amp})");
        }
    }

    #[test]
    fn returns_to_bit_exact_after_switching_off() {
        let mut seed = 42;
        let mut d = Denoiser::new(2, DENOISE_RATE);
        d.set_enabled(true);
        for _ in 0..20 {
            let mut b = noise_block(&mut seed, 480, 0.05);
            d.process(&mut b);
        }
        assert!(d.active());
        d.set_enabled(false);
        for _ in 0..3 {
            let mut b = noise_block(&mut seed, 480, 0.05);
            d.process(&mut b);
        }
        assert!(!d.active());
        let original = noise_block(&mut seed, 480, 0.05);
        let mut b = original.clone();
        d.process(&mut b);
        assert_eq!(b.channel(0), original.channel(0), "bit-exact again after fading out");
    }

    #[test]
    fn switching_on_has_no_jump() {
        // A steady 200 Hz tone (speech-like level) must not jump when the
        // denoiser fades in: the largest sample-to-sample step stays small.
        let mut d = Denoiser::new(1, DENOISE_RATE);
        d.set_enabled(true);
        let mut max_step = 0.0f32;
        let mut prev = 0.0f32;
        for k in 0..20 {
            let mut b = AudioBlock::new(1, 480);
            for (i, s) in b.channel_mut(0).iter_mut().enumerate() {
                let t = (k * 480 + i) as f32 / 48_000.0;
                *s = 0.3 * (2.0 * std::f32::consts::PI * 200.0 * t).sin();
            }
            d.process(&mut b);
            for s in b.channel(0) {
                max_step = max_step.max((s - prev).abs());
                prev = *s;
            }
        }
        // A 200 Hz sine at 0.3 changes by at most ~0.008 per sample.
        assert!(max_step < 0.05, "largest step {max_step}");
    }
}
