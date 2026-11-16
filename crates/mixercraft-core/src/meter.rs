//! Lock-free level metering.
//!
//! The audio thread publishes per-channel peak and RMS values into atomics;
//! any number of readers (IPC, diagnostics) can poll them without ever
//! blocking the audio thread. Peak values are *peak-hold since last take*:
//! [`MeterBank::take_peaks`] returns the highest value since the previous
//! call, so short transients are never missed regardless of poll rate.
//! Visual ballistics (decay, hold) are a presentation concern of the UI.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::MAX_CHANNELS;
use crate::buffer::AudioBlock;

/// One meter channel. Values are linear amplitudes stored as `f32` bits.
#[derive(Debug, Default)]
struct MeterChannel {
    peak: AtomicU32,
    rms: AtomicU32,
}

/// Meters for one strip or bus.
#[derive(Debug)]
pub struct MeterBank {
    channels: usize,
    cells: [MeterChannel; MAX_CHANNELS],
}

/// A snapshot of one channel's levels (linear amplitude).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Level {
    /// Highest absolute sample value since the previous read.
    pub peak: f32,
    /// RMS of the most recently processed block.
    pub rms: f32,
}

impl MeterBank {
    /// Creates a meter bank with `channels` active channels.
    #[must_use]
    pub fn new(channels: usize) -> Self {
        Self { channels: channels.clamp(1, MAX_CHANNELS), cells: Default::default() }
    }

    /// Number of active channels.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Measures `block` and publishes the result. Real-time safe.
    pub fn publish(&self, block: &AudioBlock) {
        let n = self.channels.min(block.channels());
        for (ch, cell) in self.cells.iter().enumerate().take(n) {
            let samples = block.channel(ch);
            let mut peak = 0.0f32;
            let mut sum_sq = 0.0f32;
            for &s in samples {
                peak = peak.max(s.abs());
                sum_sq += s * s;
            }
            let rms = if samples.is_empty() { 0.0 } else { (sum_sq / samples.len() as f32).sqrt() };
            // For non-negative finite floats the IEEE-754 bit pattern is
            // monotonic, so an integer max is a float max.
            if peak.is_finite() {
                cell.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
            }
            if rms.is_finite() {
                cell.rms.store(rms.to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// Reads and resets the peak-hold values. Fills `out` up to the number of
    /// active channels and returns how many entries were written.
    pub fn take_levels(&self, out: &mut [Level]) -> usize {
        let n = self.channels.min(out.len());
        for (ch, level) in out.iter_mut().enumerate().take(n) {
            let cell = &self.cells[ch];
            level.peak = f32::from_bits(cell.peak.swap(0, Ordering::Relaxed));
            level.rms = f32::from_bits(cell.rms.load(Ordering::Relaxed));
        }
        n
    }

    /// Convenience wrapper around [`Self::take_levels`] that allocates.
    /// Not for use on the audio thread.
    #[must_use]
    pub fn take_peaks(&self) -> Vec<Level> {
        let mut v = vec![Level::default(); self.channels];
        self.take_levels(&mut v);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_hold_until_taken() {
        let m = MeterBank::new(2);
        let mut b = AudioBlock::new(2, 4);
        b.channel_mut(0).copy_from_slice(&[0.1, -0.8, 0.2, 0.0]);
        b.channel_mut(1).copy_from_slice(&[0.5, 0.5, 0.5, 0.5]);
        m.publish(&b);
        b.clear();
        m.publish(&b); // quieter block must not lower the held peak
        let l = m.take_peaks();
        assert_eq!(l[0].peak, 0.8);
        assert_eq!(l[1].peak, 0.5);
        assert_eq!(l[1].rms, 0.0, "rms reflects the latest block");
        assert_eq!(m.take_peaks()[0].peak, 0.0, "peak resets after take");
    }

    #[test]
    fn rms_of_full_scale_square() {
        let m = MeterBank::new(1);
        let mut b = AudioBlock::new(1, 4);
        b.channel_mut(0).copy_from_slice(&[1.0, -1.0, 1.0, -1.0]);
        m.publish(&b);
        assert!((m.take_peaks()[0].rms - 1.0).abs() < 1e-6);
    }
}
