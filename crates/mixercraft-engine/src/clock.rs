//! Software clock used while no master output device is available.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use mixercraft_core::engine::EngineCore;
use mixercraft_core::stats::EngineStats;

/// The engine core shared between its possible clock drivers.
///
/// Exactly one driver (the master device callback *or* the software clock)
/// runs at a time in steady state. Drivers only ever `try_lock`, so a driver
/// never blocks; the supervisor never locks the core at all. The mutex only
/// arbitrates the brief overlap while switching drivers.
pub type SharedCore = Arc<Mutex<EngineCore>>;

/// Runs one engine cycle if the core is free. Real-time safe (never blocks).
/// Returns `false` if the cycle was skipped.
pub fn drive<F: FnOnce(&mut EngineCore)>(core: &Mutex<EngineCore>, f: F) -> bool {
    match core.try_lock() {
        Ok(mut c) => {
            f(&mut c);
            true
        }
        // A panic while processing poisoned the lock. The core's state is
        // plain data that stays consistent between cycles, so keep running
        // rather than going silent forever.
        Err(TryLockError::Poisoned(p)) => {
            f(&mut p.into_inner());
            true
        }
        Err(TryLockError::WouldBlock) => false,
    }
}

/// A thread that drives the engine at a fixed block rate using the system
/// timer. Its timing jitter is absorbed by the device FIFOs.
#[derive(Debug)]
pub struct SoftwareClock {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SoftwareClock {
    /// Starts the clock.
    ///
    /// # Errors
    /// If the thread cannot be spawned.
    pub fn start(
        core: SharedCore,
        stats: Arc<EngineStats>,
        sample_rate: u32,
        block_frames: usize,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let period = Duration::from_secs_f64(block_frames as f64 / f64::from(sample_rate.max(1)));
        let thread = std::thread::Builder::new().name("mixercraft-softclock".into()).spawn(move || {
            // This thread drives the engine like a device callback would;
            // give it the same real-time audio priority (MMCSS on Windows).
            let _ = audio_thread_priority::promote_current_thread_to_real_time(block_frames as u32, sample_rate);
            let mut next = Instant::now() + period;
            while !stop_flag.load(Ordering::Relaxed) {
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
                if !drive(&core, |c| c.run_cycle(block_frames)) {
                    stats.record_skipped_cycle();
                }
                next += period;
                // After a long stall (system suspend, debugger) restart the
                // schedule instead of running a burst of catch-up cycles.
                if Instant::now() > next + period * 20 {
                    next = Instant::now() + period;
                }
            }
        })?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for SoftwareClock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
