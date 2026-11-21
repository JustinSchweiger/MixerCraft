//! Short confirmation tones on a bus, played when a global hotkey fires so
//! the user hears what happened without looking (e.g. while a game has the
//! focus).
//!
//! Two sine notes a fifth apart, 70 ms each with 5 ms fades: rising
//! ([`ChimeKind::Up`]: unmuted, solo on, louder) or falling
//! ([`ChimeKind::Down`]: muted, solo off, quieter). Like the keep-awake
//! signal the tone is added after the bus fader and meter, so it is heard
//! even when the hotkey just muted that bus, and does not show on the meter.
//! Real-time safe.

use crate::buffer::AudioBlock;
use crate::dsp;

/// Length of one note.
pub const NOTE_SECONDS: f32 = 0.07;
/// Silence between the notes.
pub const GAP_SECONDS: f32 = 0.025;
const FADE_SECONDS: f32 = 0.005;
const LOW_HZ: f32 = 660.0;
const HIGH_HZ: f32 = 990.0;

/// Which tone to play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChimeKind {
    /// Rising: something was switched on or up.
    Up,
    /// Falling: something was switched off or down.
    Down,
}

/// Tone generator of one bus.
#[derive(Debug, Clone)]
pub struct Chime {
    sample_rate: f32,
    note_frames: u32,
    gap_frames: u32,
    fade_frames: f32,
    /// Frames into the current tone; `None` = silent.
    pos: Option<u32>,
    kind: ChimeKind,
    amplitude: f32,
    phase: f32,
}

impl Chime {
    /// Creates a silent generator.
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1) as f32;
        Self {
            sample_rate: sr,
            note_frames: (NOTE_SECONDS * sr) as u32,
            gap_frames: (GAP_SECONDS * sr) as u32,
            fade_frames: (FADE_SECONDS * sr).max(1.0),
            pos: None,
            kind: ChimeKind::Up,
            amplitude: 0.0,
            phase: 0.0,
        }
    }

    /// Starts `kind` at `level_db` (dBFS peak, clamped to -60..-6),
    /// replacing a tone still playing. Real-time safe.
    pub fn trigger(&mut self, kind: ChimeKind, level_db: f32) {
        let level = if level_db.is_nan() { -20.0 } else { level_db.clamp(-60.0, -6.0) };
        self.kind = kind;
        self.amplitude = dsp::db_to_gain(level);
        self.pos = Some(0);
        self.phase = 0.0;
    }

    /// Whether a tone is playing.
    #[must_use]
    pub fn sounding(&self) -> bool {
        self.pos.is_some()
    }

    /// Total length of one tone in frames.
    #[must_use]
    pub fn length_frames(&self) -> u32 {
        2 * self.note_frames + self.gap_frames
    }

    /// Adds the tone to every channel of `block`. Real-time safe.
    pub fn process(&mut self, block: &mut AudioBlock) {
        let Some(mut pos) = self.pos else { return };
        let total = self.length_frames();
        let (first, second) = match self.kind {
            ChimeKind::Up => (LOW_HZ, HIGH_HZ),
            ChimeKind::Down => (HIGH_HZ, LOW_HZ),
        };
        let second_start = self.note_frames + self.gap_frames;
        for frame in 0..block.frames() {
            if pos >= total {
                self.pos = None;
                return;
            }
            let (in_note, hz) = if pos < self.note_frames {
                (Some(pos), first)
            } else if pos >= second_start {
                (Some(pos - second_start), second)
            } else {
                (None, second)
            };
            if pos == second_start {
                self.phase = 0.0;
            }
            if let Some(t) = in_note {
                let t = t as f32;
                let fade = (t / self.fade_frames).min((self.note_frames as f32 - t) / self.fade_frames).clamp(0.0, 1.0);
                let s = self.amplitude * fade * (self.phase * std::f32::consts::TAU).sin();
                for ch in 0..block.channels() {
                    block.channel_mut(ch)[frame] += s;
                }
                self.phase += hz / self.sample_rate;
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                }
            }
            pos += 1;
        }
        self.pos = Some(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn render(c: &mut Chime, frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut block = AudioBlock::new(2, 480);
        let mut left = frames;
        while left > 0 {
            let n = left.min(480);
            block.set_frames(n);
            block.clear();
            c.process(&mut block);
            assert_eq!(block.channel(0), block.channel(1), "same on every channel");
            out.extend_from_slice(block.channel(0));
            left -= n;
        }
        out
    }

    fn zero_crossings(s: &[f32]) -> usize {
        s.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count()
    }

    #[test]
    fn silent_until_triggered_and_ends_by_itself() {
        let mut c = Chime::new(SR);
        assert!(render(&mut c, 4800).iter().all(|s| *s == 0.0));
        c.trigger(ChimeKind::Up, -20.0);
        let len = c.length_frames() as usize;
        let out = render(&mut c, len + 4800);
        assert!(!c.sounding());
        assert!(out[len..].iter().all(|s| *s == 0.0), "silent after the tone");
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - dsp::db_to_gain(-20.0)).abs() < 0.01, "peak {peak}");
        assert!(out[0].abs() < 1e-3, "starts with a fade, no click");
    }

    #[test]
    fn up_rises_and_down_falls() {
        let note = (NOTE_SECONDS * SR as f32) as usize;
        let second = note + (GAP_SECONDS * SR as f32) as usize;
        for (kind, rising) in [(ChimeKind::Up, true), (ChimeKind::Down, false)] {
            let mut c = Chime::new(SR);
            c.trigger(kind, -12.0);
            let len = c.length_frames() as usize;
            let out = render(&mut c, len);
            let a = zero_crossings(&out[..note]);
            let b = zero_crossings(&out[second..second + note]);
            assert_eq!(b > a, rising, "{kind:?}: {a} then {b} crossings");
            assert!(out[note + 10..second - 10].iter().all(|s| *s == 0.0), "gap between notes");
        }
    }

    #[test]
    fn levels_are_clamped() {
        let mut c = Chime::new(SR);
        c.trigger(ChimeKind::Down, 0.0);
        let peak = render(&mut c, 9600).iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= dsp::db_to_gain(-6.0) + 1e-3);
    }
}
