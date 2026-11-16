//! Lock-free runtime statistics shared between the audio thread and
//! diagnostics readers.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Counters describing one FIFO (one device stream).
#[derive(Debug, Default)]
pub struct FifoStats {
    pub(crate) underruns: AtomicU64,
    pub(crate) overruns: AtomicU64,
    pub(crate) dropped_frames: AtomicU64,
    pub(crate) fill_frames: AtomicU32,
    pub(crate) target_frames: AtomicU32,
    pub(crate) ppm_milli: AtomicI64,
    pub(crate) writes: AtomicU64,
    pub(crate) reads: AtomicU64,
    pub(crate) gap_reads: AtomicU64,
    /// Largest writer block since the reader last looked (swapped to 0).
    pub(crate) write_peak: AtomicU32,
    /// Largest writer block within the reader's sliding window.
    pub(crate) max_write_frames: AtomicU32,
}

/// A point-in-time copy of [`FifoStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FifoSnapshot {
    /// Reads that found too little audio (output padded with silence).
    pub underruns: u64,
    /// Times the FIFO overflowed or had to be trimmed back to target.
    pub overruns: u64,
    /// Frames discarded because of overruns.
    pub dropped_frames: u64,
    /// Most recent fill level including audio buffered in the resampler
    /// (writer-rate frames).
    pub fill_frames: u32,
    /// Current target fill level (frames).
    pub target_frames: u32,
    /// Current drift correction in ppm.
    pub drift_ppm: f64,
    /// Number of write calls (progress indicator of the writer side).
    pub writes: u64,
    /// Number of read calls (progress indicator of the reader side).
    pub reads: u64,
    /// Reads after warm-up that output silence for missing audio (underruns
    /// and the re-priming that follows them). Every one is an audible gap.
    pub gap_reads: u64,
    /// Largest block the writer delivered in one call during the last
    /// 30–60 seconds (frames).
    pub max_write_frames: u32,
}

impl FifoStats {
    /// Takes a consistent-enough snapshot (fields are read individually).
    #[must_use]
    pub fn snapshot(&self) -> FifoSnapshot {
        FifoSnapshot {
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            fill_frames: self.fill_frames.load(Ordering::Relaxed),
            target_frames: self.target_frames.load(Ordering::Relaxed),
            drift_ppm: self.ppm_milli.load(Ordering::Relaxed) as f64 / 1000.0,
            writes: self.writes.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            gap_reads: self.gap_reads.load(Ordering::Relaxed),
            max_write_frames: self.max_write_frames.load(Ordering::Relaxed),
        }
    }
}

/// Counters describing the processing engine.
#[derive(Debug, Default)]
pub struct EngineStats {
    pub(crate) cycles: AtomicU64,
    pub(crate) frames: AtomicU64,
    pub(crate) load_permille: AtomicU32,
    pub(crate) peak_load_permille: AtomicU32,
    pub(crate) overloads: AtomicU64,
    pub(crate) non_finite: AtomicU64,
    pub(crate) over: AtomicU64,
    pub(crate) commands_applied: AtomicU64,
    pub(crate) commands_rejected: AtomicU64,
    pub(crate) rt_deallocations: AtomicU64,
    pub(crate) skipped_cycles: AtomicU64,
}

/// A point-in-time copy of [`EngineStats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EngineSnapshot {
    /// Processing cycles run.
    pub cycles: u64,
    /// Frames processed.
    pub frames: u64,
    /// Processing time of the last cycle relative to its real-time budget (‰).
    pub load_permille: u32,
    /// Highest load seen since the last [`EngineStats::reset_peak`] (‰).
    pub peak_load_permille: u32,
    /// Cycles that took longer than their real-time budget.
    pub overloads: u64,
    /// Non-finite samples replaced with silence.
    pub non_finite: u64,
    /// Output samples above full scale.
    pub over: u64,
    /// Control commands applied.
    pub commands_applied: u64,
    /// Control commands rejected as invalid.
    pub commands_rejected: u64,
    /// Objects that had to be freed on the audio thread because the garbage
    /// queue was full. Should stay zero.
    pub rt_deallocations: u64,
    /// Cycles skipped because the engine was being reconfigured.
    pub skipped_cycles: u64,
}

impl EngineStats {
    /// Takes a snapshot.
    #[must_use]
    pub fn snapshot(&self) -> EngineSnapshot {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        EngineSnapshot {
            cycles: l(&self.cycles),
            frames: l(&self.frames),
            load_permille: self.load_permille.load(Ordering::Relaxed),
            peak_load_permille: self.peak_load_permille.load(Ordering::Relaxed),
            overloads: l(&self.overloads),
            non_finite: l(&self.non_finite),
            over: l(&self.over),
            commands_applied: l(&self.commands_applied),
            commands_rejected: l(&self.commands_rejected),
            rt_deallocations: l(&self.rt_deallocations),
            skipped_cycles: l(&self.skipped_cycles),
        }
    }

    /// Resets the peak load indicator.
    pub fn reset_peak(&self) {
        self.peak_load_permille.store(0, Ordering::Relaxed);
    }

    /// Records a cycle skipped by a clock driver. Real-time safe.
    pub fn record_skipped_cycle(&self) {
        self.skipped_cycles.fetch_add(1, Ordering::Relaxed);
    }
}
