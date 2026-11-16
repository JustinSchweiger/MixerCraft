//! Mapping between fader position (0..1) and gain in dB.
//!
//! Linear in dB from [`MIN_DB`] to [`MAX_DB`]; the bottom position means
//! silence. 0 dB sits at about 83 % of the travel, which leaves usable
//! resolution around unity gain and +12 dB of headroom.

/// Gain at the bottom of the fader (treated as silence).
pub const MIN_DB: f32 = -60.0;
/// Gain at the top of the fader.
pub const MAX_DB: f32 = 12.0;
/// What the engine is sent for "silence" (its own silence threshold).
pub const SILENCE_DB: f32 = -96.0;

/// Fader position for a gain.
#[must_use]
pub fn db_to_pos(db: f32) -> f32 {
    if db <= MIN_DB || db.is_nan() { 0.0 } else { ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0) }
}

/// Gain for a fader position (bottom = [`SILENCE_DB`]).
#[must_use]
pub fn pos_to_db(pos: f32) -> f32 {
    if pos <= 0.0 {
        SILENCE_DB
    } else {
        // Snap to 0.1 dB so the engine and the display agree.
        let db = MIN_DB + pos.clamp(0.0, 1.0) * (MAX_DB - MIN_DB);
        (db * 10.0).round() / 10.0
    }
}

/// Short text for the fader knob.
#[must_use]
pub fn knob_text(db: f32) -> String {
    if db <= MIN_DB {
        "-inf".into()
    } else if db.abs() < 0.05 {
        "0dB".into()
    } else if db.abs() < 10.0 {
        format!("{db:+.1}")
    } else {
        format!("{db:+.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_anchors() {
        assert_eq!(db_to_pos(MIN_DB), 0.0);
        assert_eq!(db_to_pos(SILENCE_DB), 0.0);
        assert_eq!(db_to_pos(MAX_DB), 1.0);
        assert!((db_to_pos(0.0) - 0.8333).abs() < 1e-3);
        for db in [-59.0f32, -24.0, -6.0, 0.0, 3.5, 12.0] {
            assert!((pos_to_db(db_to_pos(db)) - db).abs() < 0.051, "{db}");
        }
        assert_eq!(pos_to_db(0.0), SILENCE_DB);
    }

    #[test]
    fn knob_labels() {
        assert_eq!(knob_text(0.0), "0dB");
        assert_eq!(knob_text(-6.04), "-6.0");
        assert_eq!(knob_text(4.0), "+4.0");
        assert_eq!(knob_text(-24.0), "-24");
        assert_eq!(knob_text(-60.0), "-inf");
        assert_eq!(knob_text(-96.0), "-inf");
    }
}
