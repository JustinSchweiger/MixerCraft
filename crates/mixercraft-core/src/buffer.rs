//! Fixed-capacity planar audio buffers.

use crate::MAX_CHANNELS;

/// A block of planar (non-interleaved) `f32` audio with a fixed capacity.
///
/// Storage is allocated once in [`AudioBlock::new`]; afterwards every method
/// is real-time safe. The number of valid frames can change per cycle via
/// [`AudioBlock::set_frames`] up to the capacity.
#[derive(Debug, Clone)]
pub struct AudioBlock {
    data: Vec<f32>,
    channels: usize,
    capacity: usize,
    frames: usize,
}

impl AudioBlock {
    /// Allocates a silent block. `channels` is clamped to `1..=MAX_CHANNELS`.
    #[must_use]
    pub fn new(channels: usize, capacity: usize) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        Self { data: vec![0.0; channels * capacity], channels, capacity, frames: capacity }
    }

    /// Number of channels.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Number of valid frames.
    #[must_use]
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Maximum number of frames.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Sets the number of valid frames.
    ///
    /// # Panics
    /// If `frames` exceeds the capacity. Callers must split larger device
    /// buffers into capacity-sized chunks.
    pub fn set_frames(&mut self, frames: usize) {
        assert!(frames <= self.capacity, "block capacity exceeded");
        self.frames = frames;
    }

    /// The valid samples of one channel.
    #[must_use]
    pub fn channel(&self, ch: usize) -> &[f32] {
        let start = ch * self.capacity;
        &self.data[start..start + self.frames]
    }

    /// The valid samples of one channel, mutably.
    pub fn channel_mut(&mut self, ch: usize) -> &mut [f32] {
        let start = ch * self.capacity;
        &mut self.data[start..start + self.frames]
    }

    /// Sets all valid samples to zero.
    pub fn clear(&mut self) {
        for ch in 0..self.channels {
            self.channel_mut(ch).fill(0.0);
        }
    }

    /// Copies `frames` interleaved frames with `src_channels` channels into
    /// this block and sets the frame count. A mono source is copied to every
    /// channel; otherwise channels map one-to-one, missing channels are
    /// silenced and surplus source channels ignored.
    pub fn copy_from_interleaved(&mut self, src: &[f32], src_channels: usize, frames: usize) {
        self.set_frames(frames);
        for ch in 0..self.channels {
            let start = ch * self.capacity;
            let dst = &mut self.data[start..start + frames];
            if ch < src_channels || src_channels == 1 {
                let sc = if src_channels == 1 { 0 } else { ch };
                for (i, d) in dst.iter_mut().enumerate() {
                    *d = src[i * src_channels + sc];
                }
            } else {
                dst.fill(0.0);
            }
        }
    }

    /// Writes the valid frames interleaved into `dst` with `dst_channels`
    /// channels. A mono block is written to every destination channel;
    /// otherwise channels map one-to-one and surplus destination channels
    /// are silenced.
    pub fn write_interleaved(&self, dst: &mut [f32], dst_channels: usize) {
        let frames = self.frames.min(dst.len() / dst_channels.max(1));
        for ch in 0..dst_channels {
            if ch < self.channels || self.channels == 1 {
                let src = self.channel(if self.channels == 1 { 0 } else { ch });
                for i in 0..frames {
                    dst[i * dst_channels + ch] = src[i];
                }
            } else {
                for i in 0..frames {
                    dst[i * dst_channels + ch] = 0.0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleave_roundtrip() {
        let src: Vec<f32> = (0..12).map(|i| i as f32).collect(); // 6 frames stereo
        let mut b = AudioBlock::new(2, 8);
        b.copy_from_interleaved(&src, 2, 6);
        assert_eq!(b.frames(), 6);
        assert_eq!(b.channel(0), &[0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
        assert_eq!(b.channel(1), &[1.0, 3.0, 5.0, 7.0, 9.0, 11.0]);
        let mut out = vec![0.0; 12];
        b.write_interleaved(&mut out, 2);
        assert_eq!(out, src);
    }

    #[test]
    fn channel_count_mismatch_is_silenced() {
        let mut b = AudioBlock::new(3, 4);
        b.copy_from_interleaved(&[1.0, 2.0, 1.0, 2.0], 2, 2);
        assert_eq!(b.channel(2), &[0.0, 0.0]);
        let mut out = vec![9.0; 8];
        b.write_interleaved(&mut out, 4);
        assert_eq!(out, [1.0, 2.0, 0.0, 0.0, 1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn mono_is_spread_to_all_channels() {
        let mut b = AudioBlock::new(2, 3);
        b.copy_from_interleaved(&[0.1, 0.2, 0.3], 1, 3);
        assert_eq!(b.channel(1), &[0.1, 0.2, 0.3]);
        let mut m = AudioBlock::new(1, 2);
        m.copy_from_interleaved(&[0.5, 0.6], 1, 2);
        let mut out = vec![0.0; 6];
        m.write_interleaved(&mut out, 3);
        assert_eq!(out, [0.5, 0.5, 0.5, 0.6, 0.6, 0.6]);
    }

    #[test]
    #[should_panic(expected = "capacity")]
    fn over_capacity_panics() {
        AudioBlock::new(1, 4).set_frames(5);
    }
}
