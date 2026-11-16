//! "Keep awake" for a bus: speakers with auto-standby switch off after some
//! minutes without signal. When the bus has been silent for a while, a quiet
//! infrasonic sine (15 Hz by default, far below the threshold of hearing at
//! this level) is added so the speakers keep seeing a signal. It fades in
//! slowly and fades out quickly as soon as real audio returns.
//!
//! The tone is added after the bus fader and meter, so it plays even while
//! the bus is muted and does not show on the meter. Real-time safe.

use crate::buffer::AudioBlock;

/// Bus output above this (dBFS peak) counts as "playing".
pub const SILENCE_DB: f32 = -60.0;
/// Silence needed before the tone starts.
pub const SILENCE_SECONDS: f32 = 10.0;
const FADE_IN_SECONDS: f32 = 0.5;
const FADE_OUT_SECONDS: f32 = 0.1;

/// Settings of one bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeepAwakeParams {
    /// Whether the tone is used.
    pub enabled: bool,
    /// Tone level (dBFS peak), -70 to -20.
    pub level_db: f32,
    /// Tone frequency (Hz), 5 to 40.
    pub frequency_hz: f32,
}

impl Default for KeepAwakeParams {
    fn default() -> Self {
        Self { enabled: false, level_db: -45.0, frequency_hz: 15.0 }
    }
}

impl KeepAwakeParams {
    /// Values clamped to their ranges; NaN becomes the default.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let clamp = |v: f32, lo: f32, hi: f32, def: f32| if v.is_nan() { def } else { v.clamp(lo, hi) };
        Self {
            enabled: self.enabled,
            level_db: clamp(self.level_db, -70.0, -20.0, d.level_db),
            frequency_hz: clamp(self.frequency_hz, 5.0, 40.0, d.frequency_hz),
        }
    }
}

/// Keep-awake state of one bus.
#[derive(Debug, Clone)]
pub struct KeepAwake {
    params: KeepAwakeParams,
    sample_rate: f32,
    amplitude: f32,
    phase: f32,
    phase_inc: f32,
    silent_frames: u64,
    silence_needed: u64,
    gain: f32,
    fade_in: f32,
    fade_out: f32,
}

impl KeepAwake {
    /// Creates a (disabled) generator.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1) as f32;
        let mut k = Self {
            params: KeepAwakeParams::default(),
            sample_rate: sr,
            amplitude: 0.0,
            phase: 0.0,
            phase_inc: 0.0,
            silent_frames: 0,
            silence_needed: (SILENCE_SECONDS * sr) as u64,
            gain: 0.0,
            fade_in: 1.0 / (FADE_IN_SECONDS * sr),
            fade_out: 1.0 / (FADE_OUT_SECONDS * sr),
        };
        k.set_params(KeepAwakeParams::default());
        k
    }

    /// Current (sanitised) settings.
    #[must_use]
    pub fn params(&self) -> KeepAwakeParams {
        self.params
    }

    /// Whether the tone is currently playing (or fading).
    #[must_use]
    pub fn sounding(&self) -> bool {
        self.gain > 0.0
    }

    /// Applies new settings. Real-time safe.
    pub fn set_params(&mut self, params: KeepAwakeParams) {
        let p = params.sanitized();
        self.amplitude = 10f32.powf(p.level_db / 20.0);
        self.phase_inc = std::f32::consts::TAU * p.frequency_hz / self.sample_rate;
        self.params = p;
    }

    /// Looks at the (post-fader) bus output and adds the tone if the bus has
    /// been silent long enough. Real-time safe.
    pub fn process(&mut self, block: &mut AudioBlock) {
        if !self.params.enabled && self.gain == 0.0 {
            self.silent_frames = 0;
            return;
        }
        let frames = block.frames();
        let threshold = 10f32.powf(SILENCE_DB / 20.0);
        let playing = (0..block.channels()).any(|ch| block.channel(ch).iter().any(|s| s.abs() > threshold));
        if playing {
            self.silent_frames = 0;
        } else {
            self.silent_frames = self.silent_frames.saturating_add(frames as u64);
        }
        let want = self.params.enabled && !playing && self.silent_frames >= self.silence_needed;
        if !want && self.gain == 0.0 {
            return;
        }
        for i in 0..frames {
            self.gain = if want { (self.gain + self.fade_in).min(1.0) } else { (self.gain - self.fade_out).max(0.0) };
            let v = self.amplitude * self.gain * self.phase.sin();
            self.phase += self.phase_inc;
            if self.phase >= std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
            for ch in 0..block.channels() {
                block.channel_mut(ch)[i] += v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn run_silence(k: &mut KeepAwake, seconds: f32) -> AudioBlock {
        let mut last = AudioBlock::new(2, 480);
        for _ in 0..(seconds * 100.0) as usize {
            last.set_frames(480);
            last.clear();
            k.process(&mut last);
        }
        last
    }

    fn peak(b: &AudioBlock) -> f32 {
        b.channel(0).iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    #[test]
    fn disabled_does_nothing() {
        let mut k = KeepAwake::new(SR);
        let b = run_silence(&mut k, 15.0);
        assert_eq!(peak(&b), 0.0);
    }

    #[test]
    fn starts_after_ten_seconds_of_silence_at_the_set_level() {
        let mut k = KeepAwake::new(SR);
        k.set_params(KeepAwakeParams { enabled: true, ..KeepAwakeParams::default() });
        assert_eq!(peak(&run_silence(&mut k, 9.5)), 0.0, "not before 10 s");
        // 10 s + fade-in + a full 15 Hz period.
        let mut max = 0.0f32;
        run_silence(&mut k, 1.0);
        for _ in 0..10 {
            max = max.max(peak(&run_silence(&mut k, 0.01)));
        }
        let level_db = 20.0 * max.log10();
        assert!((level_db + 45.0).abs() < 0.5, "tone peak {level_db:.2} dBFS");
    }

    #[test]
    fn program_audio_stops_it_within_the_fade_out() {
        let mut k = KeepAwake::new(SR);
        k.set_params(KeepAwakeParams { enabled: true, ..KeepAwakeParams::default() });
        run_silence(&mut k, 11.0);
        assert!(k.sounding());
        // Music at -20 dBFS for 200 ms.
        for _ in 0..20 {
            let mut b = AudioBlock::new(2, 480);
            for ch in 0..2 {
                b.channel_mut(ch).fill(0.1);
            }
            k.process(&mut b);
        }
        assert!(!k.sounding(), "faded out");
        // And silence must last 10 s again before it restarts.
        assert_eq!(peak(&run_silence(&mut k, 5.0)), 0.0);
    }

    #[test]
    fn frequency_is_infrasonic_by_default() {
        let mut k = KeepAwake::new(SR);
        k.set_params(KeepAwakeParams { enabled: true, ..KeepAwakeParams::default() });
        run_silence(&mut k, 11.0);
        // Count zero crossings over one second: 2 per period.
        let mut prev = 0.0f32;
        let mut crossings = 0;
        for _ in 0..100 {
            let b = run_silence(&mut k, 0.01);
            for s in b.channel(0) {
                if (prev < 0.0) != (*s < 0.0) {
                    crossings += 1;
                }
                prev = *s;
            }
        }
        assert!((29..=31).contains(&crossings), "{crossings} zero crossings in 1 s");
    }

    #[test]
    fn params_are_sanitised() {
        let p = KeepAwakeParams { enabled: true, level_db: 0.0, frequency_hz: f32::NAN }.sanitized();
        assert_eq!((p.level_db, p.frequency_hz), (-20.0, 15.0));
    }
}
