//! Lock-free audio FIFOs between independently clocked threads.
//!
//! Each physical or virtual device stream runs in its own callback thread
//! with its own clock. A FIFO connects such a callback with the engine's
//! processing cycle:
//!
//! ```text
//!   input device ──FifoWriter──▶ [SPSC ring] ──FifoReader──▶ engine strip
//!   engine bus   ──FifoWriter──▶ [SPSC ring] ──FifoReader──▶ output device
//! ```
//!
//! The *reader* side owns a [`Resampler`] and a [`DriftController`] and
//! converts from the writer's clock domain to its own. It also implements
//! the recovery policy for timing faults:
//!
//! * **underrun** – the missing part is filled with silence, the reader
//!   re-primes to the target fill level and the target is raised (bounded),
//!   so a device that underruns repeatedly converges to a stable latency
//!   instead of glitching forever;
//! * **overrun** – the writer drops what does not fit; the reader trims the
//!   backlog back to the target so latency cannot creep up;
//! * **measured block sizes** – after a warm-up period the target is raised
//!   to at least the largest observed writer block plus the reader block, so
//!   the safety margin follows what the devices actually do rather than what
//!   they were configured to do;
//! * **warm-up** – device streams often deliver irregular bursts right after
//!   starting (e.g. WASAPI pre-filling a render buffer). Shortfalls in the
//!   first [`WARMUP_SECONDS`] re-prime silently without counting as
//!   underruns or inflating the latency.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rtrb::RingBuffer;

use crate::drift::DriftController;
use crate::resampler::{Quality, Resampler};
use crate::stats::FifoStats;

/// Duration after which reader shortfalls count as underruns.
pub const WARMUP_SECONDS: f64 = 0.25;

/// Parameters of a FIFO.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FifoSpec {
    /// Interleaved channel count.
    pub channels: usize,
    /// Sample rate on the writer side.
    pub writer_rate: u32,
    /// Sample rate on the reader side.
    pub reader_rate: u32,
    /// Initial target fill level in writer-rate frames.
    pub target_frames: usize,
    /// Upper bound for the adaptive target.
    pub max_target_frames: usize,
    /// Largest block either side transfers per call.
    pub max_block_frames: usize,
    /// Resampler quality.
    pub quality: Quality,
}

impl FifoSpec {
    /// A spec with sensible bounds for the given target latency.
    #[must_use]
    pub fn new(channels: usize, writer_rate: u32, reader_rate: u32, target_ms: f32) -> Self {
        let target_frames = ((writer_rate as f32 * target_ms / 1000.0) as usize).max(32);
        Self {
            channels,
            writer_rate,
            reader_rate,
            target_frames,
            max_target_frames: (writer_rate as usize / 5).max(target_frames), // 200 ms
            max_block_frames: 4096,
            quality: Quality::Balanced,
        }
    }

    fn capacity_frames(&self) -> usize {
        self.max_target_frames * 2 + self.max_block_frames * 4
    }
}

/// Creates a connected writer/reader pair. Allocates.
#[must_use]
pub fn fifo(spec: FifoSpec) -> (FifoWriter, FifoReader) {
    let ch = spec.channels;
    let (producer, consumer) = RingBuffer::new(spec.capacity_frames() * ch);
    let stats = Arc::new(FifoStats::default());
    stats.target_frames.store(spec.target_frames as u32, Ordering::Relaxed);
    let writer = FifoWriter { producer, channels: ch, stats: Arc::clone(&stats) };
    let reader = FifoReader {
        consumer,
        channels: ch,
        resampler: Resampler::new(ch, spec.writer_rate, spec.reader_rate, spec.quality, spec.max_block_frames),
        drift: DriftController::new(spec.writer_rate, spec.target_frames as f64),
        target: spec.target_frames,
        max_target: spec.max_target_frames,
        max_block: spec.max_block_frames,
        reader_rate: f64::from(spec.reader_rate),
        to_writer_frames: f64::from(spec.writer_rate) / f64::from(spec.reader_rate),
        frames_read: 0,
        warmup_frames: (f64::from(spec.reader_rate) * WARMUP_SECONDS) as u64,
        primed: false,
        stats,
    };
    (writer, reader)
}

/// The producing end of a FIFO.
#[derive(Debug)]
pub struct FifoWriter {
    producer: rtrb::Producer<f32>,
    channels: usize,
    stats: Arc<FifoStats>,
}

impl FifoWriter {
    /// Interleaved channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Shared statistics of this FIFO.
    #[must_use]
    pub fn stats(&self) -> &Arc<FifoStats> {
        &self.stats
    }

    /// Whether the reader has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.producer.is_abandoned()
    }

    /// Writes interleaved frames. Frames that do not fit are dropped and
    /// counted as an overrun. Returns the number of frames written.
    /// Real-time safe.
    pub fn write(&mut self, interleaved: &[f32]) -> usize {
        let ch = self.channels;
        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        let frames = interleaved.len() / ch;
        self.stats.max_write_frames.fetch_max(frames as u32, Ordering::Relaxed);
        let fit = (self.producer.slots() / ch).min(frames);
        if fit > 0 {
            let (_, rest) = self.producer.push_partial_slice(&interleaved[..fit * ch]);
            debug_assert!(rest.is_empty());
        }
        if fit < frames {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            self.stats.dropped_frames.fetch_add((frames - fit) as u64, Ordering::Relaxed);
        }
        fit
    }
}

/// Result of a [`FifoReader::read`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The full block was produced.
    Ok,
    /// The FIFO is still filling to its target; silence was produced.
    Priming,
    /// Not enough audio arrived; the block was padded with silence.
    Underrun,
}

/// The consuming end of a FIFO, converting into the reader's clock domain.
#[derive(Debug)]
pub struct FifoReader {
    consumer: rtrb::Consumer<f32>,
    channels: usize,
    resampler: Resampler,
    drift: DriftController,
    target: usize,
    max_target: usize,
    max_block: usize,
    reader_rate: f64,
    to_writer_frames: f64,
    frames_read: u64,
    warmup_frames: u64,
    primed: bool,
    stats: Arc<FifoStats>,
}

impl FifoReader {
    /// Interleaved channel count.
    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Shared statistics of this FIFO.
    #[must_use]
    pub fn stats(&self) -> &Arc<FifoStats> {
        &self.stats
    }

    /// Whether the writer has been dropped.
    #[must_use]
    pub fn is_abandoned(&self) -> bool {
        self.consumer.is_abandoned()
    }

    /// Current target fill level in writer-rate frames.
    #[must_use]
    pub fn target_frames(&self) -> usize {
        self.target
    }

    /// Produces exactly `out.len() / channels` interleaved frames at the
    /// reader rate. Real-time safe.
    pub fn read(&mut self, out: &mut [f32]) -> ReadOutcome {
        let ch = self.channels;
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        let frames = out.len() / ch;
        let out = &mut out[..frames * ch];
        let warming_up = self.frames_read < self.warmup_frames;
        self.frames_read += frames as u64;

        if !warming_up {
            // The FIFO must hold at least one writer block plus one reader
            // block, or it underruns by construction.
            let max_write = self.stats.max_write_frames.load(Ordering::Relaxed) as usize;
            let need = max_write + (frames as f64 * self.to_writer_frames).ceil() as usize;
            if need > self.target && self.target < self.max_target {
                self.set_target(need.min(self.max_target));
                self.primed = false; // re-prime once to the new margin
            }
        }

        let ring = self.consumer.slots() / ch;
        let total = ring as f64 + self.resampler.buffered_frames();
        self.stats.fill_frames.store(total as u32, Ordering::Relaxed);

        if !self.primed {
            if ring < self.target {
                out.fill(0.0);
                return ReadOutcome::Priming;
            }
            self.primed = true;
            self.resampler.reset();
            self.drift.reset_measurement();
            // Start exactly at the target so the drift loop begins settled.
            self.discard(ring - self.target);
        } else if total > self.high_water() as f64 {
            let excess = (total as usize).saturating_sub(self.target);
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
            self.stats.dropped_frames.fetch_add(excess as u64, Ordering::Relaxed);
            self.resampler.reset();
            self.discard(ring.saturating_sub(self.target));
            self.drift.reset_measurement();
        }

        let fill = (self.consumer.slots() / ch) as f64 + self.resampler.buffered_frames();
        let ratio = self.drift.update(fill, frames as f64 / self.reader_rate);
        self.resampler.set_adjust(ratio);
        self.stats.ppm_milli.store((self.drift.ppm() * 1000.0) as i64, Ordering::Relaxed);

        let mut produced = 0;
        // Each iteration either fills the output or drains the ring; the
        // bound only guards against logic errors.
        for _ in 0..16 {
            if produced == frames {
                break;
            }
            let avail = self.consumer.slots();
            let Ok(chunk) = self.consumer.read_chunk(avail) else { break };
            let (a, b) = chunk.as_slices();
            let (c1, p1) = self.resampler.process(a, &mut out[produced * ch..]);
            produced += p1;
            let mut consumed = c1;
            if c1 * ch == a.len() && produced < frames {
                let (c2, p2) = self.resampler.process(b, &mut out[produced * ch..]);
                produced += p2;
                consumed += c2;
            }
            chunk.commit(consumed * ch);
            if consumed == 0 && avail == 0 {
                // Ring empty and the resampler cannot produce more.
                let (_, p) = self.resampler.process(&[], &mut out[produced * ch..]);
                produced += p;
                break;
            }
        }

        if produced < frames {
            out[produced * ch..].fill(0.0);
            self.primed = false;
            if warming_up {
                return ReadOutcome::Priming;
            }
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            // Adaptive latency: grow the safety margin by 50 %, bounded.
            self.set_target((self.target + self.target / 2).min(self.max_target));
            return ReadOutcome::Underrun;
        }
        ReadOutcome::Ok
    }

    fn set_target(&mut self, target: usize) {
        self.target = target;
        self.drift.set_target(target as f64);
        self.stats.target_frames.store(target as u32, Ordering::Relaxed);
    }

    fn high_water(&self) -> usize {
        (self.target * 3).max(self.target + 2 * self.max_block)
    }

    fn discard(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        if let Ok(chunk) = self.consumer.read_chunk(frames * self.channels) {
            chunk.commit_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_signal(start: usize, frames: usize, ch: usize) -> Vec<f32> {
        (start..start + frames).flat_map(|n| std::iter::repeat_n(((n % 1000) as f32) / 1000.0, ch)).collect()
    }

    #[test]
    fn primes_then_streams() {
        let spec = FifoSpec::new(2, 48_000, 48_000, 10.0);
        let (mut w, mut r) = fifo(spec);
        let mut out = vec![0.0; 256 * 2];
        assert_eq!(r.read(&mut out), ReadOutcome::Priming);
        w.write(&ramp_signal(0, 480, 2));
        assert_eq!(r.read(&mut out), ReadOutcome::Ok);
    }

    /// Streams `reads` blocks of `block` frames with matching writes.
    fn stream(w: &mut FifoWriter, r: &mut FifoReader, block: usize, reads: usize) {
        let mut out = vec![0.0; block * w.channels()];
        let data = vec![0.5; block * w.channels()];
        for _ in 0..reads {
            w.write(&data);
            r.read(&mut out);
        }
    }

    #[test]
    fn startup_shortfall_is_not_an_underrun() {
        let spec = FifoSpec::new(1, 48_000, 48_000, 2.0);
        let (mut w, mut r) = fifo(spec);
        let target = r.target_frames();
        w.write(&vec![0.5; target]);
        // A render stream's first callback asking for a whole buffer.
        let mut out = vec![1.0; 2048];
        assert_eq!(r.read(&mut out), ReadOutcome::Priming);
        assert!(out[1024..].iter().all(|&s| s == 0.0), "shortfall padded with silence");
        assert_eq!(r.stats().snapshot().underruns, 0);
        assert_eq!(r.target_frames(), target);
    }

    #[test]
    fn target_follows_measured_block_sizes() {
        let spec = FifoSpec::new(1, 48_000, 48_000, 2.0); // 96 frames: too small
        let (mut w, mut r) = fifo(spec);
        stream(&mut w, &mut r, 256, 100);
        assert!(r.target_frames() >= 512, "target {}", r.target_frames());
        assert_eq!(r.stats().snapshot().underruns, 0);
    }

    #[test]
    fn underrun_after_warmup_pads_with_silence_and_raises_target() {
        let spec = FifoSpec::new(1, 48_000, 48_000, 2.0);
        let (mut w, mut r) = fifo(spec);
        stream(&mut w, &mut r, 256, 100);
        let before = r.target_frames();
        let mut out = vec![1.0; 256];
        let mut outcome = ReadOutcome::Ok;
        for _ in 0..10 {
            outcome = r.read(&mut out); // the writer has stalled
            if outcome == ReadOutcome::Underrun {
                break;
            }
        }
        assert_eq!(outcome, ReadOutcome::Underrun);
        assert_eq!(*out.last().unwrap(), 0.0);
        assert!(r.target_frames() > before);
        assert_eq!(r.stats().snapshot().underruns, 1);
    }

    #[test]
    fn writer_overflow_is_counted_and_reader_trims_backlog() {
        let mut spec = FifoSpec::new(1, 48_000, 48_000, 2.0);
        spec.max_target_frames = 200;
        spec.max_block_frames = 64;
        let (mut w, mut r) = fifo(spec);
        let cap = spec.capacity_frames();
        let written = w.write(&vec![0.1; cap + 100]);
        assert_eq!(written, cap);
        assert_eq!(w.stats().snapshot().overruns, 1);
        let mut out = vec![0.0; 64];
        assert_eq!(r.read(&mut out), ReadOutcome::Ok); // priming trims to target
        w.write(&vec![0.1; cap - 200]);
        r.read(&mut out);
        assert!(r.stats().snapshot().overruns >= 2, "reader trimmed backlog");
    }

    /// End-to-end: a 44.1 kHz producer with +250 ppm clock error feeding a
    /// 48 kHz consumer for 60 simulated seconds must not glitch once primed.
    #[test]
    fn long_run_with_rate_conversion_and_drift_is_glitch_free() {
        let mut spec = FifoSpec::new(2, 44_100, 48_000, 20.0);
        spec.quality = Quality::Fast;
        let (mut w, mut r) = fifo(spec);
        let producer_rate = 44_100.0 * (1.0 + 250e-6);
        let consumer_block = 256;
        let producer_block = 441;
        let mut produced_total = 0usize;
        let mut owed = 0.0f64;
        let mut out = vec![0.0; consumer_block * 2];
        let block = vec![0.25f32; producer_block * 2];
        let mut outcomes = [0usize; 3];
        let cycles = (60.0 * 48_000.0 / consumer_block as f64) as usize;
        for cycle in 0..cycles {
            owed += consumer_block as f64 * producer_rate / 48_000.0;
            while owed >= producer_block as f64 {
                w.write(&block);
                produced_total += producer_block;
                owed -= producer_block as f64;
            }
            let o = r.read(&mut out);
            outcomes[o as usize] += 1;
            if cycle > 100 {
                assert_eq!(o, ReadOutcome::Ok, "glitch at cycle {cycle}");
                assert!(out.iter().all(|s| (s - 0.25).abs() < 1e-3));
            }
        }
        assert!(produced_total > 0);
        let s = r.stats().snapshot();
        assert_eq!(s.underruns, 0);
        assert_eq!(s.overruns, 0);
        assert!((s.drift_ppm - 250.0).abs() < 40.0, "ppm {}", s.drift_ppm);
    }
}
