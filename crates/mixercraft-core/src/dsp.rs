//! Basic gain math and click-free parameter smoothing.

/// Gains at or below this level (in dB) are treated as silence.
pub const SILENCE_DB: f32 = -96.0;

/// Converts decibels to a linear amplitude factor.
///
/// Values at or below [`SILENCE_DB`] map to exactly `0.0`.
#[must_use]
pub fn db_to_gain(db: f32) -> f32 {
    if db <= SILENCE_DB || db.is_nan() { 0.0 } else { 10f32.powf(db / 20.0) }
}

/// Converts a linear amplitude factor to decibels (`-inf` for silence).
#[must_use]
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 { f32::NEG_INFINITY } else { 20.0 * gain.log10() }
}

/// Shape of a gain over one processing block, as produced by
/// [`SmoothedGain::render`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GainShape {
    /// The gain is constant over the whole block.
    Constant(f32),
    /// The gain changes; per-frame values have been written to the ramp buffer.
    Ramp,
}

/// A gain that moves linearly to its target over a fixed number of frames.
///
/// Instant gain jumps cause audible clicks ("zipper noise"). Every gain that
/// can be changed while audio is running — fader, mute, routing switch — goes
/// through a `SmoothedGain`.
#[derive(Debug, Clone)]
pub struct SmoothedGain {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    ramp_frames: u32,
}

impl SmoothedGain {
    /// Creates a gain resting at `initial` which ramps over `ramp_frames`
    /// frames whenever its target changes. A ramp length of 0 is treated as 1.
    #[must_use]
    pub fn new(initial: f32, ramp_frames: u32) -> Self {
        Self { current: initial, target: initial, step: 0.0, remaining: 0, ramp_frames: ramp_frames.max(1) }
    }

    /// Sets a new target. Real-time safe.
    pub fn set_target(&mut self, target: f32) {
        if target == self.target {
            return;
        }
        self.target = target;
        self.remaining = self.ramp_frames;
        self.step = (target - self.current) / self.ramp_frames as f32;
    }

    /// Jumps to `value` immediately, cancelling any ramp.
    pub fn reset(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.remaining = 0;
        self.step = 0.0;
    }

    /// The gain the most recent frame was processed with.
    #[must_use]
    pub fn current(&self) -> f32 {
        self.current
    }

    /// The value the gain is moving towards.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.target
    }

    /// Whether a ramp is in progress.
    #[must_use]
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }

    /// Whether the gain is (and stays) exactly zero for the next block.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        self.remaining == 0 && self.current == 0.0
    }

    /// Advances the gain by `ramp.len()` frames.
    ///
    /// Returns [`GainShape::Constant`] when no ramp is active (and leaves
    /// `ramp` untouched); otherwise writes one gain per frame into `ramp`.
    /// Real-time safe.
    pub fn render(&mut self, ramp: &mut [f32]) -> GainShape {
        if self.remaining == 0 {
            return GainShape::Constant(self.current);
        }
        let frames = ramp.len();
        let ramping = (self.remaining as usize).min(frames);
        let start = self.current;
        for (i, g) in ramp.iter_mut().enumerate() {
            *g = if i < ramping { start + self.step * (i + 1) as f32 } else { self.target };
        }
        self.remaining -= ramping as u32;
        self.current = if self.remaining == 0 { self.target } else { start + self.step * ramping as f32 };
        if self.remaining == 0 {
            // Ensure the tail of the buffer is exactly the target.
            for g in &mut ramp[ramping.saturating_sub(1)..] {
                *g = self.target;
            }
        }
        GainShape::Ramp
    }
}

/// Multiplies `samples` by a gain shape. Real-time safe.
pub fn apply_gain(samples: &mut [f32], shape: GainShape, ramp: &[f32]) {
    match shape {
        GainShape::Constant(1.0) => {}
        GainShape::Constant(0.0) => samples.fill(0.0),
        GainShape::Constant(g) => samples.iter_mut().for_each(|s| *s *= g),
        GainShape::Ramp => samples.iter_mut().zip(ramp).for_each(|(s, g)| *s *= *g),
    }
}

/// Replaces NaN and infinite samples with silence and returns how many were
/// replaced. A single NaN fed into a recursive filter or a device would
/// otherwise corrupt audio until the stream is restarted. Real-time safe.
pub fn sanitize(samples: &mut [f32]) -> u32 {
    let mut replaced = 0;
    for s in samples {
        if !s.is_finite() {
            *s = 0.0;
            replaced += 1;
        }
    }
    replaced
}

/// Counts samples whose magnitude exceeds full scale. Real-time safe.
#[must_use]
pub fn count_over(samples: &[f32]) -> u32 {
    samples.iter().filter(|s| s.abs() > 1.0).count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        for db in [-60.0f32, -12.0, -6.0, 0.0, 6.0, 12.0] {
            let g = db_to_gain(db);
            assert!((gain_to_db(g) - db).abs() < 1e-4, "{db}");
        }
        assert_eq!(db_to_gain(SILENCE_DB), 0.0);
        assert_eq!(db_to_gain(-200.0), 0.0);
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn smoothed_gain_reaches_target_exactly() {
        let mut g = SmoothedGain::new(0.0, 100);
        g.set_target(1.0);
        let mut ramp = [0.0f32; 64];
        assert_eq!(g.render(&mut ramp), GainShape::Ramp);
        assert!(ramp.windows(2).all(|w| w[1] > w[0]), "monotonic ramp");
        assert!(g.is_ramping());
        assert_eq!(g.render(&mut ramp), GainShape::Ramp);
        assert_eq!(ramp[35..], [1.0; 29]);
        assert!(!g.is_ramping());
        assert_eq!(g.current(), 1.0);
        assert_eq!(g.render(&mut ramp), GainShape::Constant(1.0));
    }

    #[test]
    fn smoothed_gain_ramp_is_continuous_across_blocks() {
        let mut g = SmoothedGain::new(1.0, 480);
        g.set_target(0.0);
        let mut all = Vec::new();
        let mut ramp = [0.0f32; 100];
        for _ in 0..6 {
            match g.render(&mut ramp) {
                GainShape::Ramp => all.extend_from_slice(&ramp),
                GainShape::Constant(c) => all.extend(std::iter::repeat_n(c, ramp.len())),
            }
        }
        let max_step = all.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(max_step <= 1.0 / 480.0 + 1e-6, "max step {max_step}");
        assert_eq!(*all.last().unwrap(), 0.0);
        assert!(g.is_silent());
    }

    #[test]
    fn retarget_mid_ramp_starts_from_current_value() {
        let mut g = SmoothedGain::new(0.0, 10);
        g.set_target(1.0);
        let mut ramp = [0.0f32; 5];
        g.render(&mut ramp);
        let mid = g.current();
        assert!((mid - 0.5).abs() < 1e-6);
        g.set_target(0.0);
        g.render(&mut ramp);
        assert!(ramp[0] < mid && ramp[0] > 0.4);
    }

    #[test]
    fn apply_gain_shapes() {
        let mut s = [1.0f32; 4];
        apply_gain(&mut s, GainShape::Constant(0.5), &[]);
        assert_eq!(s, [0.5; 4]);
        apply_gain(&mut s, GainShape::Ramp, &[1.0, 2.0, 0.0, 4.0]);
        assert_eq!(s, [0.5, 1.0, 0.0, 2.0]);
        apply_gain(&mut s, GainShape::Constant(0.0), &[]);
        assert_eq!(s, [0.0; 4]);
    }

    #[test]
    fn sanitize_replaces_non_finite() {
        let mut s = [0.1, f32::NAN, f32::INFINITY, -0.2, f32::NEG_INFINITY];
        assert_eq!(sanitize(&mut s), 3);
        assert_eq!(s, [0.1, 0.0, 0.0, -0.2, 0.0]);
        assert_eq!(count_over(&[0.5, 1.0, 1.5, -2.0]), 2);
    }
}
