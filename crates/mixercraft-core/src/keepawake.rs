//! "Keep awake" for a bus: speakers with auto-standby switch off after some
//! minutes without signal. Two ways to give them one:
//!
//! * [`KeepAwakeMode::Pulse`] (default): after `interval_s` of silence, a
//!   0.3 s burst of soft band-limited noise (about 150 Hz to 2.5 kHz) at a low
//!   level, repeated every `interval_s` while the bus stays silent. Real audio
//!   in the range speakers' signal detection listens to; short and quiet
//!   enough to go unnoticed at normal volume.
//! * [`KeepAwakeMode::Infrasound`]: after 10 s of silence a continuous quiet
//!   infrasonic sine (15 Hz by default). Inaudible, but many sound cards and
//!   speaker inputs filter it out before their signal detection (seen on the
//!   owner's USB speakers), so it does not work everywhere.
//!
//! Program audio stops either signal within a short fade. The signal is
//! added after the bus fader and meter, so it plays even while the bus is
//! muted and does not show on the meter. [`KeepAwake::trigger_test`] plays it
//! once immediately. Real-time safe.

use crate::buffer::AudioBlock;

/// Bus output above this (dBFS peak) counts as "playing".
pub const SILENCE_DB: f32 = -60.0;
/// Silence needed before the infrasonic tone starts.
pub const SILENCE_SECONDS: f32 = 10.0;
/// Length of one pulse.
pub const PULSE_SECONDS: f32 = 0.3;
const PULSE_RAMP_SECONDS: f32 = 0.03;
/// Length of a test of the infrasonic tone.
const TEST_TONE_SECONDS: f32 = 2.0;
const FADE_IN_SECONDS: f32 = 0.5;
const FADE_OUT_SECONDS: f32 = 0.1;

/// What a keep-awake bus plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeepAwakeMode {
    /// Short quiet noise bursts every `interval_s` of silence.
    #[default]
    Pulse,
    /// A continuous inaudible low sine while silent.
    Infrasound,
}

/// Settings of one bus.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeepAwakeParams {
    /// Whether keep-awake is used.
    pub enabled: bool,
    /// Pulse or infrasound.
    pub mode: KeepAwakeMode,
    /// Signal level (dBFS peak), -70 to -20.
    pub level_db: f32,
    /// Infrasound frequency (Hz), 5 to 40.
    pub frequency_hz: f32,
    /// Pulse interval (s), 30 to 900.
    pub interval_s: f32,
}

impl Default for KeepAwakeParams {
    fn default() -> Self {
        Self { enabled: false, mode: KeepAwakeMode::Pulse, level_db: -45.0, frequency_hz: 15.0, interval_s: 240.0 }
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
            mode: self.mode,
            level_db: clamp(self.level_db, -70.0, -20.0, d.level_db),
            frequency_hz: clamp(self.frequency_hz, 5.0, 40.0, d.frequency_hz),
            interval_s: clamp(self.interval_s, 30.0, 900.0, d.interval_s),
        }
    }
}

/// Band-limited noise for pulses: white noise through a one-pole high-pass
/// (~150 Hz) and low-pass (~2.5 kHz), scaled to a peak of about 1.
#[derive(Debug, Clone)]
struct Noise {
    seed: u32,
    hp_a: f32,
    hp_x: f32,
    hp_y: f32,
    lp_a: f32,
    lp_y: f32,
    norm: f32,
}

impl Noise {
    fn new(sample_rate: f32) -> Self {
        let one_pole = |hz: f32| (-std::f32::consts::TAU * hz / sample_rate).exp();
        let mut n = Self {
            seed: 0x9e37_79b9,
            hp_a: one_pole(150.0),
            hp_x: 0.0,
            hp_y: 0.0,
            lp_a: one_pole(2500.0),
            lp_y: 0.0,
            norm: 1.0,
        };
        // Measure the filtered peak once (control thread) to normalise.
        let mut peak = 0.0f32;
        for _ in 0..sample_rate as usize {
            peak = peak.max(n.raw().abs());
        }
        n.norm = if peak > 0.0 { 1.0 / peak } else { 1.0 };
        n
    }

    fn raw(&mut self) -> f32 {
        // xorshift32
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        let white = (self.seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let hp = self.hp_a * (self.hp_y + white - self.hp_x);
        self.hp_x = white;
        self.hp_y = hp;
        self.lp_y = (1.0 - self.lp_a) * hp + self.lp_a * self.lp_y;
        self.lp_y
    }

    fn next(&mut self) -> f32 {
        (self.raw() * self.norm).clamp(-1.0, 1.0)
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
    /// Frames of silence (any mode).
    silent_frames: u64,
    silence_needed: u64,
    /// Frames since the last pulse or program audio.
    since_pulse: u64,
    interval_frames: u64,
    /// Infrasound fade gain.
    gain: f32,
    fade_in: f32,
    fade_out: f32,
    noise: Noise,
    /// Remaining frames of the current pulse (0 = none).
    pulse_left: u64,
    pulse_frames: u64,
    ramp_frames: u64,
    /// Remaining frames of a test of the infrasonic tone.
    test_tone_left: u64,
}

impl KeepAwake {
    /// Creates a (disabled) generator. Not real-time safe (measures the
    /// noise filter once).
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
            since_pulse: 0,
            interval_frames: 0,
            gain: 0.0,
            fade_in: 1.0 / (FADE_IN_SECONDS * sr),
            fade_out: 1.0 / (FADE_OUT_SECONDS * sr),
            noise: Noise::new(sr),
            pulse_left: 0,
            pulse_frames: (PULSE_SECONDS * sr) as u64,
            ramp_frames: (PULSE_RAMP_SECONDS * sr).max(1.0) as u64,
            test_tone_left: 0,
        };
        k.set_params(KeepAwakeParams::default());
        k
    }

    /// Current (sanitised) settings.
    #[must_use]
    pub fn params(&self) -> KeepAwakeParams {
        self.params
    }

    /// Whether a signal is currently playing (or fading).
    #[must_use]
    pub fn sounding(&self) -> bool {
        self.gain > 0.0 || self.pulse_left > 0
    }

    /// Applies new settings. Real-time safe.
    pub fn set_params(&mut self, params: KeepAwakeParams) {
        let p = params.sanitized();
        self.amplitude = 10f32.powf(p.level_db / 20.0);
        self.phase_inc = std::f32::consts::TAU * p.frequency_hz / self.sample_rate;
        self.interval_frames = (p.interval_s * self.sample_rate) as u64;
        if p.mode != self.params.mode {
            self.pulse_left = 0;
            self.test_tone_left = 0;
        }
        self.params = p;
    }

    /// Plays the selected signal once now (a pulse, or 2 s of the tone),
    /// even if keep-awake is off or audio is playing. Real-time safe.
    pub fn trigger_test(&mut self) {
        match self.params.mode {
            KeepAwakeMode::Pulse => self.pulse_left = self.pulse_frames,
            KeepAwakeMode::Infrasound => self.test_tone_left = (TEST_TONE_SECONDS * self.sample_rate) as u64,
        }
    }

    /// Looks at the (post-fader) bus output and adds the keep-awake signal
    /// when due. Real-time safe.
    pub fn process(&mut self, block: &mut AudioBlock) {
        let frames = block.frames();
        let threshold = 10f32.powf(SILENCE_DB / 20.0);
        let playing = (0..block.channels()).any(|ch| block.channel(ch).iter().any(|s| s.abs() > threshold));
        if playing {
            self.silent_frames = 0;
            self.since_pulse = 0;
        } else {
            self.silent_frames = self.silent_frames.saturating_add(frames as u64);
            self.since_pulse = self.since_pulse.saturating_add(frames as u64);
        }
        match self.params.mode {
            KeepAwakeMode::Pulse => self.process_pulse(block, playing),
            KeepAwakeMode::Infrasound => self.process_tone(block, playing),
        }
    }

    fn process_pulse(&mut self, block: &mut AudioBlock, playing: bool) {
        self.gain = 0.0;
        if self.params.enabled && !playing && self.pulse_left == 0 && self.since_pulse >= self.interval_frames {
            self.pulse_left = self.pulse_frames;
            self.since_pulse = 0;
        }
        if self.pulse_left == 0 {
            return;
        }
        for i in 0..block.frames() {
            if self.pulse_left == 0 {
                break;
            }
            // Raised edges: ramp in over the first, out over the last ramp.
            let done = self.pulse_frames - self.pulse_left;
            let edge = done.min(self.pulse_left).min(self.ramp_frames) as f32 / self.ramp_frames as f32;
            let v = self.amplitude * edge * self.noise.next();
            for ch in 0..block.channels() {
                block.channel_mut(ch)[i] += v;
            }
            self.pulse_left -= 1;
        }
    }

    fn process_tone(&mut self, block: &mut AudioBlock, playing: bool) {
        self.pulse_left = 0;
        let testing = self.test_tone_left > 0;
        let want = testing || (self.params.enabled && !playing && self.silent_frames >= self.silence_needed);
        if !want && self.gain == 0.0 {
            return;
        }
        for i in 0..block.frames() {
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
        self.test_tone_left = self.test_tone_left.saturating_sub(block.frames() as u64);
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

    /// Peak over `seconds` of silence, block by block.
    fn peak_over(k: &mut KeepAwake, seconds: f32) -> f32 {
        let mut max = 0.0f32;
        for _ in 0..(seconds * 100.0) as usize {
            max = max.max(peak(&run_silence(k, 0.01)));
        }
        max
    }

    fn peak(b: &AudioBlock) -> f32 {
        b.channel(0).iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    fn tone() -> KeepAwakeParams {
        KeepAwakeParams { enabled: true, mode: KeepAwakeMode::Infrasound, ..KeepAwakeParams::default() }
    }

    fn pulse(interval_s: f32) -> KeepAwakeParams {
        KeepAwakeParams { enabled: true, mode: KeepAwakeMode::Pulse, interval_s, ..KeepAwakeParams::default() }
    }

    fn music(k: &mut KeepAwake, blocks: usize) {
        for _ in 0..blocks {
            let mut b = AudioBlock::new(2, 480);
            for ch in 0..2 {
                b.channel_mut(ch).fill(0.1);
            }
            k.process(&mut b);
        }
    }

    #[test]
    fn disabled_does_nothing() {
        let mut k = KeepAwake::new(SR);
        assert_eq!(peak_over(&mut k, 300.0), 0.0);
    }

    #[test]
    fn pulses_after_each_interval_of_silence_at_the_set_level() {
        let mut k = KeepAwake::new(SR);
        k.set_params(pulse(30.0));
        assert_eq!(peak_over(&mut k, 29.9), 0.0, "nothing before the interval");
        let p = peak_over(&mut k, 0.5);
        let level_db = 20.0 * p.log10();
        assert!((-47.0..=-44.9).contains(&level_db), "pulse peak {level_db:.2} dBFS");
        assert!(!k.sounding(), "a pulse lasts 0.3 s");
        assert_eq!(peak_over(&mut k, 29.0), 0.0, "quiet until the next interval");
        assert!(peak_over(&mut k, 1.0) > 0.0, "and pulses again");
    }

    #[test]
    fn program_audio_resets_the_pulse_interval() {
        let mut k = KeepAwake::new(SR);
        k.set_params(pulse(30.0));
        run_silence(&mut k, 25.0);
        music(&mut k, 10);
        assert_eq!(peak_over(&mut k, 29.0), 0.0, "interval restarted by the music");
        assert!(peak_over(&mut k, 1.5) > 0.0);
    }

    #[test]
    fn pulse_noise_is_in_the_audible_band() {
        // The pulse must not be infrasound: count zero crossings in one
        // (test) pulse; band-limited noise around 150 Hz to 2.5 kHz crosses
        // zero hundreds of times in 0.3 s.
        let mut k = KeepAwake::new(SR);
        k.set_params(pulse(240.0));
        k.trigger_test();
        let (mut prev, mut crossings) = (0.0f32, 0);
        for _ in 0..30 {
            let b = run_silence(&mut k, 0.01);
            for s in b.channel(0) {
                if (prev < 0.0) != (*s < 0.0) {
                    crossings += 1;
                }
                prev = *s;
            }
        }
        assert!(crossings > 200, "{crossings} zero crossings in a pulse");
    }

    #[test]
    fn test_trigger_plays_now_even_when_disabled() {
        let mut k = KeepAwake::new(SR);
        k.trigger_test();
        assert!(peak_over(&mut k, 0.3) > 0.0, "pulse test");
        k.set_params(KeepAwakeParams { mode: KeepAwakeMode::Infrasound, ..KeepAwakeParams::default() });
        k.trigger_test();
        assert!(peak_over(&mut k, 1.0) > 0.0, "tone test");
        // 2 s of tone plus the fade-out, then silence.
        run_silence(&mut k, 1.5);
        assert_eq!(peak_over(&mut k, 1.0), 0.0, "test tone ends");
    }

    #[test]
    fn tone_starts_after_ten_seconds_of_silence_at_the_set_level() {
        let mut k = KeepAwake::new(SR);
        k.set_params(tone());
        assert_eq!(peak(&run_silence(&mut k, 9.5)), 0.0, "not before 10 s");
        run_silence(&mut k, 1.0);
        let level_db = 20.0 * peak_over(&mut k, 0.1).log10();
        assert!((level_db + 45.0).abs() < 0.5, "tone peak {level_db:.2} dBFS");
    }

    #[test]
    fn program_audio_stops_the_tone_within_the_fade_out() {
        let mut k = KeepAwake::new(SR);
        k.set_params(tone());
        run_silence(&mut k, 11.0);
        assert!(k.sounding());
        music(&mut k, 20);
        assert!(!k.sounding(), "faded out");
        assert_eq!(peak(&run_silence(&mut k, 5.0)), 0.0);
    }

    #[test]
    fn tone_frequency_is_infrasonic_by_default() {
        let mut k = KeepAwake::new(SR);
        k.set_params(tone());
        run_silence(&mut k, 11.0);
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
        let p = KeepAwakeParams { level_db: 0.0, frequency_hz: f32::NAN, interval_s: 5.0, ..tone() }.sanitized();
        assert_eq!((p.level_db, p.frequency_hz, p.interval_s), (-20.0, 15.0, 30.0));
    }
}
