//! Clock-drift compensation.
//!
//! Every audio device runs from its own crystal. Two devices that both report
//! 48 kHz typically differ by 10–300 ppm, so a FIFO between them slowly fills
//! up or drains — after a few minutes to hours the result is an overrun or an
//! underrun: exactly the "crackling after extended use" failure this project
//! exists to prevent.
//!
//! [`DriftController`] measures the FIFO fill level and steers the
//! resampler's ratio with a PI controller so that the fill level stays at its
//! target indefinitely. Because the integrator learns the true clock ratio,
//! the steady-state correction is constant and the pitch deviation is far
//! below audibility (100 ppm ≈ 0.17 cent).
//!
//! The loop is deliberately slow (natural frequency 0.1 rad/s, critically
//! damped) so that bursty producers — e.g. a device delivering 10 ms
//! periods into a consumer pulling 2.7 ms — do not modulate the pitch.

/// PI controller that converts FIFO fill level into a resampling ratio.
#[derive(Debug, Clone)]
pub struct DriftController {
    sample_rate: f64,
    target: f64,
    filtered: f64,
    integral: f64,
    adjust: f64,
    initialized: bool,
    kp: f64,
    ki: f64,
    tau: f64,
    limit: f64,
}

impl DriftController {
    /// Maximum correction (±2000 ppm). Real crystals are within ±300 ppm;
    /// the margin allows recovering from transient fill errors quickly.
    pub const DEFAULT_LIMIT: f64 = 0.002;

    /// Creates a controller for a FIFO holding audio at `sample_rate` that
    /// should be kept at `target_frames`.
    #[must_use]
    pub fn new(sample_rate: u32, target_frames: f64) -> Self {
        let omega = 0.1; // rad/s
        let zeta = 1.0;
        Self {
            sample_rate: f64::from(sample_rate.max(1)),
            target: target_frames,
            filtered: 0.0,
            integral: 0.0,
            adjust: 0.0,
            initialized: false,
            kp: 2.0 * zeta * omega,
            ki: omega * omega,
            tau: 0.5,
            limit: Self::DEFAULT_LIMIT,
        }
    }

    /// The fill level the controller steers towards (frames).
    #[must_use]
    pub fn target(&self) -> f64 {
        self.target
    }

    /// Changes the target fill level. The learned clock ratio is kept.
    pub fn set_target(&mut self, target_frames: f64) {
        self.target = target_frames;
    }

    /// Forgets the measured fill level but keeps the learned clock ratio,
    /// e.g. after a resynchronisation.
    pub fn reset_measurement(&mut self) {
        self.initialized = false;
    }

    /// Current correction as a ratio factor (1.0 = nominal).
    #[must_use]
    pub fn ratio(&self) -> f64 {
        1.0 + self.adjust
    }

    /// Current correction in parts per million.
    #[must_use]
    pub fn ppm(&self) -> f64 {
        self.adjust * 1e6
    }

    /// Feeds one measurement of the FIFO fill level (in frames, including
    /// audio buffered inside the resampler) taken `dt_seconds` after the
    /// previous one and returns the new ratio factor. Real-time safe.
    pub fn update(&mut self, fill_frames: f64, dt_seconds: f64) -> f64 {
        if !self.initialized {
            self.filtered = fill_frames;
            self.initialized = true;
        } else {
            let alpha = 1.0 - (-dt_seconds / self.tau).exp();
            self.filtered += alpha * (fill_frames - self.filtered);
        }
        let error = (self.filtered - self.target) / self.sample_rate; // seconds
        let candidate_integral = self.integral + error * dt_seconds;
        let unclamped = self.kp * error + self.ki * candidate_integral;
        // Anti-windup: only integrate while not saturated, or when the
        // integration moves the output back into range.
        if unclamped.abs() <= self.limit || (unclamped.signum() != error.signum()) {
            self.integral = candidate_integral;
        }
        self.adjust = (self.kp * error + self.ki * self.integral).clamp(-self.limit, self.limit);
        1.0 + self.adjust
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SimResult {
        min_fill: f64,
        max_deviation: f64,
        mean_ppm: f64,
        max_ppm_ripple: f64,
    }

    /// Simulates a FIFO between a producer clock running `drift` faster than
    /// nominal and a consumer steered by the controller. Statistics other
    /// than `min_fill` cover the settled part (after 120 s).
    fn simulate(drift: f64, producer_block: f64, consumer_block: f64, seconds: f64) -> SimResult {
        let rate = 48_000.0;
        let target = 1440.0; // 30 ms
        let mut c = DriftController::new(48_000, target);
        let mut fill = target;
        let mut producer_phase = 0.0;
        let mut t = 0.0;
        let dt = consumer_block / rate;
        let mut r = SimResult { min_fill: f64::MAX, max_deviation: 0.0, mean_ppm: 0.0, max_ppm_ripple: 0.0 };
        let mut n = 0.0;
        while t < seconds {
            // Producer delivers whole blocks according to its own clock.
            producer_phase += consumer_block * (1.0 + drift);
            while producer_phase >= producer_block {
                fill += producer_block;
                producer_phase -= producer_block;
            }
            let ratio = c.update(fill, dt);
            fill -= consumer_block * ratio;
            r.min_fill = r.min_fill.min(fill);
            if t > 120.0 {
                r.max_deviation = r.max_deviation.max((fill - target).abs());
                r.mean_ppm += c.ppm();
                r.max_ppm_ripple = r.max_ppm_ripple.max((c.ppm() - drift * 1e6).abs());
                n += 1.0;
            }
            t += dt;
        }
        r.mean_ppm /= n;
        r
    }

    #[test]
    fn learns_positive_and_negative_drift_without_xruns() {
        for drift in [-300e-6, -50e-6, 0.0, 120e-6, 300e-6] {
            let r = simulate(drift, 480.0, 128.0, 1800.0);
            assert!(r.min_fill > 0.0, "underrun at drift {drift}");
            assert!(r.max_deviation < 600.0, "fill deviation {} at drift {drift}", r.max_deviation);
            assert!((r.mean_ppm - drift * 1e6).abs() < 1.0, "mean ppm {} vs {}", r.mean_ppm, drift * 1e6);
        }
    }

    #[test]
    fn bursty_producer_causes_only_inaudible_ratio_ripple() {
        // 10 ms producer periods vs 64-frame consumer periods. The sampled
        // fill level aliases against the burst pattern, which the loop sees
        // as a slow disturbance. ±150 ppm is ±0.26 cent: far below audibility.
        let r = simulate(80e-6, 480.0, 64.0, 600.0);
        assert!((r.mean_ppm - 80.0).abs() < 1.0, "mean ppm {}", r.mean_ppm);
        assert!(r.max_ppm_ripple < 150.0, "ripple {}", r.max_ppm_ripple);
    }

    #[test]
    fn correction_is_bounded() {
        let mut c = DriftController::new(48_000, 1000.0);
        for _ in 0..10_000 {
            let r = c.update(1_000_000.0, 0.01);
            assert!(r <= 1.0 + DriftController::DEFAULT_LIMIT + 1e-12);
        }
        // Anti-windup: once the error disappears the controller must not
        // stay pinned at the limit for long.
        let mut settled = false;
        for _ in 0..3_000 {
            if (c.update(1000.0, 0.01) - 1.0).abs() < 1e-4 {
                settled = true;
                break;
            }
        }
        assert!(settled, "integrator wound up");
    }
}
