//! Meter ballistics for display.
//!
//! The engine reports peak-hold-since-last-read amplitudes. For display they
//! are converted to a 0..1 scale over [`FLOOR_DB`]..0 dBFS, rise instantly,
//! fall at [`FALL_DB_PER_S`], and keep a peak marker for [`HOLD`] before it
//! falls too.

use std::time::Duration;

/// Bottom of the meter scale.
pub const FLOOR_DB: f32 = -60.0;
/// Release speed of the bar.
pub const FALL_DB_PER_S: f32 = 24.0;
/// How long the peak marker stays put.
pub const HOLD: Duration = Duration::from_millis(1500);

/// Converts a linear amplitude to the 0..1 meter scale.
#[must_use]
pub fn to_scale(amplitude: f32) -> f32 {
    if amplitude <= 0.0 || !amplitude.is_finite() {
        return 0.0;
    }
    let db = 20.0 * amplitude.log10();
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Display state of one meter channel.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Channel {
    /// Bar height, 0..1.
    pub level: f32,
    /// Peak marker, 0..1.
    pub peak: f32,
    held: Duration,
}

impl Channel {
    /// Advances by `dt` with a new peak amplitude reading.
    pub fn update(&mut self, amplitude: f32, dt: Duration) {
        let target = to_scale(amplitude);
        let fall = FALL_DB_PER_S / -FLOOR_DB * dt.as_secs_f32();
        self.level = if target >= self.level { target } else { (self.level - fall).max(target) };
        if self.level >= self.peak {
            self.peak = self.level;
            self.held = Duration::ZERO;
        } else {
            self.held += dt;
            if self.held > HOLD {
                self.peak = (self.peak - fall).max(self.level);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(33);

    #[test]
    fn scale_anchors() {
        assert_eq!(to_scale(1.0), 1.0);
        assert_eq!(to_scale(0.0), 0.0);
        assert!((to_scale(0.5) - (1.0 - 6.0206 / 60.0)).abs() < 1e-4);
        assert_eq!(to_scale(0.0001), 0.0, "below -60 dBFS");
        assert_eq!(to_scale(f32::NAN), 0.0);
    }

    #[test]
    fn instant_attack_slow_release_and_peak_hold() {
        let mut c = Channel::default();
        c.update(1.0, FRAME);
        assert_eq!(c.level, 1.0);
        c.update(0.0, FRAME);
        assert!(c.level < 1.0 && c.level > 0.9, "falls gradually: {}", c.level);
        assert_eq!(c.peak, 1.0, "peak holds");
        for _ in 0..30 {
            c.update(0.0, FRAME); // ~1 s
        }
        assert_eq!(c.peak, 1.0, "still within hold time");
        for _ in 0..200 {
            c.update(0.0, FRAME);
        }
        assert_eq!((c.level, c.peak), (0.0, 0.0), "everything falls to silence");
    }
}
