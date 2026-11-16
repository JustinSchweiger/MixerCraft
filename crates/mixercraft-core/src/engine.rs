//! The real-time processing cycle.
//!
//! [`EngineCore`] owns the [`Mixer`] and the engine-side FIFO ends. It is
//! driven by exactly one clock at a time:
//!
//! * normally the **master output device** callback, via
//!   [`EngineCore::render_master`] — the master bus is written straight into
//!   the device buffer with no extra FIFO and no resampling;
//! * when no master device is available, a software clock calling
//!   [`EngineCore::run_cycle`], so routing keeps working (e.g. between
//!   virtual devices) while hardware is missing.
//!
//! Every other device is attached through a [`FifoReader`] (inputs) or
//! [`FifoWriter`] (outputs) and is clock-decoupled by drift-compensated
//! resampling.
//!
//! All reconfiguration arrives through a bounded lock-free [`Command`] queue.
//! Objects removed on the audio thread are returned through a [`Garbage`]
//! queue so that their memory is freed on the control thread.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use rtrb::RingBuffer;

use crate::MAX_CHANNELS;
use crate::fifo::{FifoReader, FifoWriter};
use crate::mixer::{Mixer, MixerCommand, MixerMeters};
use crate::stats::EngineStats;

/// Maximum number of strips that can have an attached input FIFO.
pub const MAX_INPUT_SLOTS: usize = 64;

/// Commands processed at the start of each cycle, bounding per-cycle work.
const COMMANDS_PER_CYCLE: usize = 64;

/// A reconfiguration request for the audio thread.
#[derive(Debug)]
pub enum Command {
    /// A mixer parameter change.
    Mixer(MixerCommand),
    /// Connects a FIFO as the source of a strip, replacing any previous one.
    AttachInput { strip: usize, reader: Box<FifoReader> },
    /// Disconnects the source of a strip (it becomes silent).
    DetachInput { strip: usize },
    /// Connects a FIFO as the destination of a bus, replacing any previous one.
    AttachOutput { bus: usize, writer: Box<FifoWriter> },
    /// Disconnects the destination of a bus.
    DetachOutput { bus: usize },
    /// Swaps in a mixer with a different topology. Attached FIFOs stay
    /// attached by index.
    ReplaceMixer(Box<Mixer>),
    /// Selects which bus is rendered by [`EngineCore::render_master`].
    SetMasterBus(Option<usize>),
    /// Swaps in a mixer with strips or buses added or removed, in one step.
    /// `remove_input` / `remove_output` name a strip / bus index that was
    /// removed: its FIFO is detached and the FIFOs (and master bus) after it
    /// move down by one, matching the new mixer's indices.
    Reshape { mixer: Box<Mixer>, remove_input: Option<usize>, remove_output: Option<usize> },
}

/// Objects handed back from the audio thread for deallocation.
#[derive(Debug)]
pub enum Garbage {
    /// A detached or replaced input FIFO.
    Reader(Box<FifoReader>),
    /// A detached or replaced output FIFO.
    Writer(Box<FifoWriter>),
    /// A replaced mixer.
    Mixer(Box<Mixer>),
}

/// Control-thread side of an engine.
#[derive(Debug)]
pub struct EngineHandle {
    commands: rtrb::Producer<Command>,
    garbage: rtrb::Consumer<Garbage>,
    stats: Arc<EngineStats>,
}

impl EngineHandle {
    /// Queues a command. Returns it back if the queue is full.
    ///
    /// # Errors
    /// The command, if the queue is full.
    pub fn send(&mut self, cmd: Command) -> Result<(), Box<Command>> {
        self.commands.push(cmd).map_err(|rtrb::PushError::Full(c)| Box::new(c))
    }

    /// Frees objects returned by the audio thread. Call regularly.
    pub fn collect_garbage(&mut self) -> usize {
        let mut n = 0;
        while let Ok(g) = self.garbage.pop() {
            drop(g);
            n += 1;
        }
        n
    }

    /// Engine statistics.
    #[must_use]
    pub fn stats(&self) -> &Arc<EngineStats> {
        &self.stats
    }
}

/// Audio-thread side of an engine.
#[derive(Debug)]
pub struct EngineCore {
    mixer: Box<Mixer>,
    sample_rate: u32,
    inputs: Vec<Option<Box<FifoReader>>>,
    outputs: Vec<Option<Box<FifoWriter>>>,
    master_bus: Option<usize>,
    commands: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Garbage>,
    scratch: Vec<f32>,
    stats: Arc<EngineStats>,
}

/// Creates an engine around `mixer`. Allocates.
#[must_use]
pub fn engine(mixer: Mixer, sample_rate: u32, master_bus: Option<usize>) -> (EngineCore, EngineHandle) {
    let (cmd_tx, cmd_rx) = RingBuffer::new(256);
    let (gc_tx, gc_rx) = RingBuffer::new(256);
    let stats = Arc::new(EngineStats::default());
    let max_frames = mixer.max_frames();
    let core = EngineCore {
        mixer: Box::new(mixer),
        sample_rate,
        inputs: (0..MAX_INPUT_SLOTS).map(|_| None).collect(),
        outputs: (0..crate::MAX_BUSES).map(|_| None).collect(),
        master_bus,
        commands: cmd_rx,
        garbage: gc_tx,
        scratch: vec![0.0; max_frames * MAX_CHANNELS],
        stats: Arc::clone(&stats),
    };
    let handle = EngineHandle { commands: cmd_tx, garbage: gc_rx, stats };
    (core, handle)
}

impl EngineCore {
    /// The engine sample rate (that of the clock driving it).
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Meter handles of the current mixer. Allocates; control thread only.
    #[must_use]
    pub fn meters(&self) -> MixerMeters {
        self.mixer.meters()
    }

    /// Renders the master bus into an interleaved device buffer with
    /// `channels` channels, running as many cycles as needed. If no master bus
    /// is set the buffer is silenced but processing still runs.
    /// Real-time safe.
    pub fn render_master(&mut self, out: &mut [f32], channels: usize) {
        let channels = channels.max(1);
        let total = out.len() / channels;
        let max = self.mixer.max_frames();
        let mut done = 0;
        while done < total {
            let frames = (total - done).min(max);
            self.run_cycle(frames);
            let dst = &mut out[done * channels..(done + frames) * channels];
            match self.master_bus.filter(|&b| b < self.mixer.bus_count()) {
                Some(bus) => self.mixer.bus_output(bus).write_interleaved(dst, channels),
                None => dst.fill(0.0),
            }
            done += frames;
        }
    }

    /// Runs one processing cycle of `frames` frames (≤ the mixer's maximum):
    /// applies pending commands, pulls inputs, mixes and pushes outputs.
    /// Real-time safe.
    ///
    /// # Panics
    /// If `frames` exceeds the mixer's maximum block size.
    pub fn run_cycle(&mut self, frames: usize) {
        let start = Instant::now();
        self.apply_commands();

        for strip in 0..self.mixer.strip_count() {
            let input = self.inputs.get_mut(strip).and_then(Option::as_mut);
            let block = self.mixer.strip_input_mut(strip);
            match input {
                Some(reader) => {
                    let ch = reader.channels();
                    let buf = &mut self.scratch[..frames * ch];
                    reader.read(buf);
                    block.copy_from_interleaved(buf, ch, frames);
                }
                None => {
                    block.set_frames(frames);
                    block.clear();
                }
            }
        }

        let report = self.mixer.process(frames);

        for bus in 0..self.mixer.bus_count() {
            if Some(bus) == self.master_bus {
                continue;
            }
            if let Some(writer) = self.outputs[bus].as_mut() {
                let ch = writer.channels();
                let buf = &mut self.scratch[..frames * ch];
                self.mixer.bus_output(bus).write_interleaved(buf, ch);
                writer.write(buf);
            }
        }

        let s = &self.stats;
        s.cycles.fetch_add(1, Ordering::Relaxed);
        s.frames.fetch_add(frames as u64, Ordering::Relaxed);
        if report.non_finite > 0 {
            s.non_finite.fetch_add(u64::from(report.non_finite), Ordering::Relaxed);
        }
        if report.over > 0 {
            s.over.fetch_add(u64::from(report.over), Ordering::Relaxed);
        }
        let budget_ns = frames as f64 * 1e9 / f64::from(self.sample_rate.max(1));
        let load = (start.elapsed().as_nanos() as f64 / budget_ns * 1000.0) as u32;
        s.load_permille.store(load, Ordering::Relaxed);
        s.peak_load_permille.fetch_max(load, Ordering::Relaxed);
        if load > 1000 {
            s.overloads.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn apply_commands(&mut self) {
        for _ in 0..COMMANDS_PER_CYCLE {
            let Ok(cmd) = self.commands.pop() else { break };
            let ok = match cmd {
                Command::Mixer(c) => self.mixer.apply(c).is_ok(),
                Command::AttachInput { strip, reader } => match self.inputs.get_mut(strip) {
                    Some(slot) => {
                        if let Some(old) = slot.replace(reader) {
                            self.dispose(Garbage::Reader(old));
                        }
                        true
                    }
                    None => {
                        self.dispose(Garbage::Reader(reader));
                        false
                    }
                },
                Command::DetachInput { strip } => match self.inputs.get_mut(strip).and_then(Option::take) {
                    Some(old) => {
                        self.dispose(Garbage::Reader(old));
                        true
                    }
                    None => false,
                },
                Command::AttachOutput { bus, writer } => match self.outputs.get_mut(bus) {
                    Some(slot) => {
                        if let Some(old) = slot.replace(writer) {
                            self.dispose(Garbage::Writer(old));
                        }
                        true
                    }
                    None => {
                        self.dispose(Garbage::Writer(writer));
                        false
                    }
                },
                Command::DetachOutput { bus } => match self.outputs.get_mut(bus).and_then(Option::take) {
                    Some(old) => {
                        self.dispose(Garbage::Writer(old));
                        true
                    }
                    None => false,
                },
                Command::ReplaceMixer(mut mixer) => {
                    std::mem::swap(&mut self.mixer, &mut mixer);
                    self.dispose(Garbage::Mixer(mixer));
                    true
                }
                Command::SetMasterBus(bus) => {
                    self.master_bus = bus;
                    true
                }
                Command::Reshape { mut mixer, remove_input, remove_output } => {
                    std::mem::swap(&mut self.mixer, &mut mixer);
                    self.dispose(Garbage::Mixer(mixer));
                    if let Some(i) = remove_input.filter(|&i| i < self.inputs.len()) {
                        if let Some(old) = self.inputs[i].take() {
                            self.dispose(Garbage::Reader(old));
                        }
                        // Moves the now empty slot to the end (no allocation).
                        self.inputs[i..].rotate_left(1);
                    }
                    if let Some(b) = remove_output.filter(|&b| b < self.outputs.len()) {
                        if let Some(old) = self.outputs[b].take() {
                            self.dispose(Garbage::Writer(old));
                        }
                        self.outputs[b..].rotate_left(1);
                        self.master_bus = match self.master_bus {
                            Some(m) if m == b => None,
                            Some(m) if m > b => Some(m - 1),
                            m => m,
                        };
                    }
                    true
                }
            };
            let counter = if ok { &self.stats.commands_applied } else { &self.stats.commands_rejected };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn dispose(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(g)) = self.garbage.push(g) {
            // Last resort: free on the audio thread and make it visible.
            self.stats.rt_deallocations.fetch_add(1, Ordering::Relaxed);
            drop(g);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fifo::{FifoSpec, fifo};
    use crate::mixer::{BusSpec, MixerLayout, StripSpec};

    fn two_by_two() -> Mixer {
        let layout = MixerLayout {
            strips: vec![
                StripSpec { routes: 0b01, ..StripSpec::stereo() },
                StripSpec { routes: 0b10, ..StripSpec::stereo() },
            ],
            buses: vec![BusSpec::stereo(), BusSpec::stereo()],
        };
        Mixer::new(&layout, 512, 48_000).unwrap()
    }

    #[test]
    fn input_fifo_reaches_master_and_secondary_output() {
        let (mut core, mut handle) = engine(two_by_two(), 48_000, Some(0));
        let (mut in_w, in_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        let (out_w, mut out_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        let (mut in2_w, in2_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        handle.send(Command::AttachInput { strip: 0, reader: Box::new(in_r) }).unwrap();
        handle.send(Command::AttachInput { strip: 1, reader: Box::new(in2_r) }).unwrap();
        handle.send(Command::AttachOutput { bus: 1, writer: Box::new(out_w) }).unwrap();

        let src = vec![0.5f32; 480 * 2];
        let src2 = vec![-0.25f32; 480 * 2];
        let mut master = vec![0.0f32; 480 * 2];
        let mut second = vec![0.0f32; 480 * 2];
        for _ in 0..50 {
            in_w.write(&src);
            in2_w.write(&src2);
            core.render_master(&mut master, 2);
            out_r.read(&mut second);
        }
        assert!(master.iter().all(|s| (s - 0.5).abs() < 1e-3), "master carries strip 0");
        assert!(second.iter().all(|s| (s + 0.25).abs() < 1e-3), "bus 1 carries strip 1");
        let snap = handle.stats().snapshot();
        assert_eq!(snap.commands_applied, 3);
        assert_eq!(snap.cycles, 50);
    }

    #[test]
    fn device_buffers_larger_than_block_are_split() {
        let (mut core, _h) = engine(two_by_two(), 48_000, Some(0));
        let mut out = vec![1.0f32; 1500 * 2];
        core.render_master(&mut out, 2);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(core.stats.snapshot().frames, 1500);
    }

    #[test]
    fn detached_objects_are_returned_for_deallocation() {
        let (mut core, mut handle) = engine(two_by_two(), 48_000, None);
        let (_w, r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        handle.send(Command::AttachInput { strip: 0, reader: Box::new(r) }).unwrap();
        handle.send(Command::DetachInput { strip: 0 }).unwrap();
        handle.send(Command::ReplaceMixer(Box::new(two_by_two()))).unwrap();
        handle.send(Command::DetachInput { strip: 0 }).unwrap(); // nothing attached
        core.run_cycle(64);
        assert_eq!(handle.collect_garbage(), 2);
        let snap = handle.stats().snapshot();
        assert_eq!(snap.commands_applied, 3);
        assert_eq!(snap.commands_rejected, 1);
        assert_eq!(snap.rt_deallocations, 0);
    }

    #[test]
    fn reshape_removing_a_strip_and_bus_shifts_later_fifos_down() {
        // Three strips feeding bus 0, three buses; master is bus 2.
        let layout = |strips: usize, buses: usize| MixerLayout {
            strips: vec![StripSpec { routes: 0b111, ..StripSpec::stereo() }; strips],
            buses: vec![BusSpec::stereo(); buses],
        };
        let (mut core, mut handle) = engine(Mixer::new(&layout(3, 3), 512, 48_000).unwrap(), 48_000, Some(2));
        let mut writers = Vec::new();
        for strip in 0..3 {
            let (w, r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
            writers.push(w);
            handle.send(Command::AttachInput { strip, reader: Box::new(r) }).unwrap();
        }
        let (out_w, mut out_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        handle.send(Command::AttachOutput { bus: 1, writer: Box::new(out_w) }).unwrap();

        let levels = [0.1f32, 0.2, 0.4];
        let mut master = vec![0.0f32; 480 * 2];
        let mut second = vec![0.0f32; 480 * 2];
        let mut run = |core: &mut EngineCore, master: &mut Vec<f32>, second: &mut Vec<f32>| {
            for _ in 0..50 {
                for (w, l) in writers.iter_mut().zip(levels) {
                    w.write(&vec![l; 480 * 2]);
                }
                core.render_master(master, 2);
                out_r.read(second);
            }
        };
        run(&mut core, &mut master, &mut second);
        assert!(master.iter().all(|s| (s - 0.7).abs() < 1e-3), "all three strips");

        // Remove strip 1 and bus 1: strip 2's FIFO becomes strip 1, bus 2
        // (the master) becomes bus 1, the old bus 1 output is detached.
        let smaller = Mixer::new(&layout(2, 2), 512, 48_000).unwrap();
        handle
            .send(Command::Reshape { mixer: Box::new(smaller), remove_input: Some(1), remove_output: Some(1) })
            .unwrap();
        run(&mut core, &mut master, &mut second);
        assert!(master.iter().all(|s| (s - 0.5).abs() < 1e-3), "strips 0 and 2 remain: {}", master[0]);
        assert_eq!(handle.collect_garbage(), 3, "mixer, strip 1 reader and bus 1 writer");
        assert_eq!(handle.stats().snapshot().commands_rejected, 0);
    }

    #[test]
    fn mixer_commands_take_effect() {
        let (mut core, mut handle) = engine(two_by_two(), 48_000, Some(0));
        let (mut w, r) = fifo(FifoSpec::new(2, 48_000, 48_000, 5.0));
        handle.send(Command::AttachInput { strip: 0, reader: Box::new(r) }).unwrap();
        handle.send(Command::Mixer(MixerCommand::StripMute { strip: 0, mute: true })).unwrap();
        let mut out = vec![0.0f32; 480 * 2];
        for _ in 0..10 {
            w.write(&vec![0.5; 480 * 2]);
            core.render_master(&mut out, 2);
        }
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
