//! Mapping between knob positions (0..1) and processing settings.
//!
//! * Gate: 0 = off, then -80 … -10 dBFS threshold.
//! * Compressor: 0 … 10 amount (0 = off).
//! * EQ bands: -12 … +12 dB, centre = 0 dB. Values snap to 0.5 dB, and
//!   within ±0.25 dB of the centre to exactly 0 so a band can be switched
//!   fully off by hand.

use mixercraft_ipc::StripFx;

const GATE_OFF: f32 = -80.0;
const GATE_SPAN: f32 = 70.0;
const COMP_MAX: f32 = 10.0;
const EQ_RANGE: f32 = 12.0;

/// Knob positions of one strip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Knobs {
    pub gate: f32,
    pub comp: f32,
    pub low: f32,
    pub mid: f32,
    pub high: f32,
    /// Noise suppression switch.
    pub denoise: bool,
}

fn eq_pos(db: f32) -> f32 {
    ((db + EQ_RANGE) / (2.0 * EQ_RANGE)).clamp(0.0, 1.0)
}

fn eq_db(pos: f32) -> f32 {
    let db = ((pos.clamp(0.0, 1.0) * 2.0 * EQ_RANGE - EQ_RANGE) * 2.0).round() / 2.0;
    if db.abs() < 0.25 { 0.0 } else { db }
}

impl Knobs {
    /// Knob positions for engine settings.
    #[must_use]
    pub fn from_fx(fx: &StripFx) -> Self {
        Self {
            gate: if fx.gate_threshold_db <= GATE_OFF {
                0.0
            } else {
                ((fx.gate_threshold_db - GATE_OFF) / GATE_SPAN).clamp(0.0, 1.0)
            },
            comp: (fx.comp_amount / COMP_MAX).clamp(0.0, 1.0),
            low: eq_pos(fx.eq_low_db),
            mid: eq_pos(fx.eq_mid_db),
            high: eq_pos(fx.eq_high_db),
            denoise: fx.noise_suppression,
        }
    }

    /// Engine settings for these knob positions.
    #[must_use]
    pub fn to_fx(self) -> StripFx {
        StripFx {
            gate_threshold_db: if self.gate < 0.005 { GATE_OFF } else { (GATE_OFF + GATE_SPAN * self.gate).round() },
            comp_amount: if self.comp < 0.005 { 0.0 } else { (self.comp * COMP_MAX * 10.0).round() / 10.0 },
            eq_low_db: eq_db(self.low),
            eq_mid_db: eq_db(self.mid),
            eq_high_db: eq_db(self.high),
            noise_suppression: self.denoise,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_centered() {
        let k = Knobs::from_fx(&StripFx::default());
        assert_eq!(k, Knobs { gate: 0.0, comp: 0.0, low: 0.5, mid: 0.5, high: 0.5, denoise: false });
        assert_eq!(k.to_fx(), StripFx::default());
    }

    #[test]
    fn roundtrip_and_snapping() {
        let fx = StripFx {
            gate_threshold_db: -45.0,
            comp_amount: 4.5,
            eq_low_db: 3.0,
            eq_mid_db: -1.5,
            eq_high_db: 12.0,
            noise_suppression: true,
        };
        assert_eq!(Knobs::from_fx(&fx).to_fx(), fx);
        let k = Knobs { gate: 0.004, comp: 0.0, low: 0.505, mid: 0.0, high: 1.0, denoise: false };
        let f = k.to_fx();
        assert_eq!(f.gate_threshold_db, GATE_OFF, "tiny gate position means off");
        assert_eq!(f.eq_low_db, 0.0, "near centre snaps to 0 dB");
        assert_eq!((f.eq_mid_db, f.eq_high_db), (-12.0, 12.0));
    }
}
