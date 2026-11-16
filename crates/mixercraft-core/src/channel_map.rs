//! Channel mapping between sources and destinations with different layouts.

use crate::MAX_CHANNELS;
use crate::buffer::AudioBlock;
use crate::dsp::GainShape;

/// A `dst × src` gain matrix describing how source channels feed
/// destination channels.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMap {
    src: usize,
    dst: usize,
    gains: [[f32; MAX_CHANNELS]; MAX_CHANNELS],
}

impl ChannelMap {
    /// The default mapping between `src` and `dst` channels:
    ///
    /// * equal counts – identity,
    /// * mono source – copied to every destination channel,
    /// * mono destination – average of all source channels,
    /// * otherwise – identity on the shared channels; extra source channels
    ///   are dropped, extra destination channels stay silent.
    #[must_use]
    pub fn default_for(src: usize, dst: usize) -> Self {
        let src = src.clamp(1, MAX_CHANNELS);
        let dst = dst.clamp(1, MAX_CHANNELS);
        let mut gains = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
        if src == 1 {
            for row in gains.iter_mut().take(dst) {
                row[0] = 1.0;
            }
        } else if dst == 1 {
            for g in gains[0].iter_mut().take(src) {
                *g = 1.0 / src as f32;
            }
        } else {
            for (i, row) in gains.iter_mut().enumerate().take(src.min(dst)) {
                row[i] = 1.0;
            }
        }
        Self { src, dst, gains }
    }

    /// Creates a map from an explicit matrix (`matrix[dst][src]`).
    /// Returns `None` if dimensions are out of range.
    #[must_use]
    pub fn from_matrix(matrix: &[&[f32]]) -> Option<Self> {
        let dst = matrix.len();
        let src = matrix.first()?.len();
        if !(1..=MAX_CHANNELS).contains(&dst)
            || !(1..=MAX_CHANNELS).contains(&src)
            || matrix.iter().any(|r| r.len() != src)
        {
            return None;
        }
        let mut gains = [[0.0; MAX_CHANNELS]; MAX_CHANNELS];
        for (d, row) in matrix.iter().enumerate() {
            gains[d][..src].copy_from_slice(row);
        }
        Some(Self { src, dst, gains })
    }

    /// Number of source channels.
    #[must_use]
    pub fn src_channels(&self) -> usize {
        self.src
    }

    /// Number of destination channels.
    #[must_use]
    pub fn dst_channels(&self) -> usize {
        self.dst
    }

    /// Gain from source channel `s` to destination channel `d`.
    #[must_use]
    pub fn gain(&self, d: usize, s: usize) -> f32 {
        self.gains[d][s]
    }

    /// Adds `src` into `dst` through this map, scaled by `shape`
    /// (per-frame gains are taken from `ramp` when `shape` is a ramp).
    /// Real-time safe.
    pub fn mix_into(&self, src: &AudioBlock, dst: &mut AudioBlock, shape: GainShape, ramp: &[f32]) {
        let frames = src.frames().min(dst.frames());
        let srcs = self.src.min(src.channels());
        let dsts = self.dst.min(dst.channels());
        for d in 0..dsts {
            for s in 0..srcs {
                let m = self.gains[d][s];
                if m == 0.0 {
                    continue;
                }
                let input = &src.channel(s)[..frames];
                let out = &mut dst.channel_mut(d)[..frames];
                match shape {
                    GainShape::Constant(g) => {
                        let k = m * g;
                        if k == 0.0 {
                            continue;
                        }
                        for (o, i) in out.iter_mut().zip(input) {
                            *o += i * k;
                        }
                    }
                    GainShape::Ramp => {
                        for ((o, i), g) in out.iter_mut().zip(input).zip(ramp) {
                            *o += i * m * g;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(chans: &[&[f32]]) -> AudioBlock {
        let mut b = AudioBlock::new(chans.len(), chans[0].len());
        for (i, c) in chans.iter().enumerate() {
            b.channel_mut(i).copy_from_slice(c);
        }
        b
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let src = block(&[&[1.0, 2.0]]);
        let mut dst = AudioBlock::new(2, 2);
        dst.clear();
        ChannelMap::default_for(1, 2).mix_into(&src, &mut dst, GainShape::Constant(1.0), &[]);
        assert_eq!(dst.channel(0), &[1.0, 2.0]);
        assert_eq!(dst.channel(1), &[1.0, 2.0]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let src = block(&[&[1.0, 0.0], &[0.0, 1.0]]);
        let mut dst = AudioBlock::new(1, 2);
        dst.clear();
        ChannelMap::default_for(2, 1).mix_into(&src, &mut dst, GainShape::Constant(1.0), &[]);
        assert_eq!(dst.channel(0), &[0.5, 0.5]);
    }

    #[test]
    fn stereo_to_quad_leaves_rear_silent_and_accumulates() {
        let src = block(&[&[1.0], &[2.0]]);
        let mut dst = AudioBlock::new(4, 1);
        dst.clear();
        let map = ChannelMap::default_for(2, 4);
        map.mix_into(&src, &mut dst, GainShape::Constant(0.5), &[]);
        map.mix_into(&src, &mut dst, GainShape::Constant(0.5), &[]);
        assert_eq!([dst.channel(0)[0], dst.channel(1)[0], dst.channel(2)[0], dst.channel(3)[0]], [1.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn explicit_swap_matrix() {
        let map = ChannelMap::from_matrix(&[&[0.0, 1.0], &[1.0, 0.0]]).unwrap();
        let src = block(&[&[1.0], &[2.0]]);
        let mut dst = AudioBlock::new(2, 1);
        dst.clear();
        map.mix_into(&src, &mut dst, GainShape::Ramp, &[0.5]);
        assert_eq!([dst.channel(0)[0], dst.channel(1)[0]], [1.0, 0.5]);
        assert!(ChannelMap::from_matrix(&[&[1.0], &[1.0, 2.0]]).is_none());
    }
}
