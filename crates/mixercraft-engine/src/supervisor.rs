//! Device lifecycle, recovery and control.
//!
//! The supervisor runs on the engine's main thread. It owns every device
//! stream, applies the recovery policy, serves IPC requests and persists the
//! configuration. It never touches the real-time core directly except through
//! the lock-free command queue.
//!
//! # Device recovery state machine
//!
//! ```text
//!            open ok                    fault / stall
//!  Waiting ───────────▶ Running ──────────────────────▶ Waiting (backoff)
//!     ▲  open failed       │ healthy 30 s: attempts = 0     │
//!     └────────────────────┘                                │ > MAX_FAST_ATTEMPTS
//!                                                           ▼
//!                                                 Failed (retry every 60 s,
//!                                                 reported as an error)
//! ```
//!
//! Faults are isolated per device: a failing input never restarts other
//! streams. When the master output fails, a software clock takes over so
//! that all other routes keep working, and hands back once the master
//! returns.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mixercraft_config::{BusConfig, Config, DeviceRef, FxConfig, Sink, Source, StripConfig};
use mixercraft_core::engine::{Command, EngineHandle, engine};
use mixercraft_core::fifo::{FifoSpec, fifo};
use mixercraft_core::fx::FxParams;
use mixercraft_core::keepawake::{KeepAwakeMode, KeepAwakeParams};
use mixercraft_core::mixer::{BusSpec, Mixer, MixerCommand, MixerLayout, MixerMeters, StripSpec};
use mixercraft_core::stats::FifoStats;
use mixercraft_ipc::{
    BusStatus, DeviceChoice, DeviceStatus, DeviceSummary, EngineSettings, EngineState, EngineStatus, ErrorCode,
    FifoStatus, LatencyProfile, Level, MeterSnapshot, RecoveryEvent, Request, Response, StripStatus,
};
use mixercraft_platform::{AudioSystem, DeviceSelector, FaultKind, OpenStream, StreamFault, StreamInfo, StreamRequest};
use tracing::{debug, error, info, warn};

use crate::clock::{SharedCore, SoftwareClock, drive};

/// Consecutive quick retries before a device is reported as failed.
const MAX_FAST_ATTEMPTS: u32 = 10;
/// Retry interval for failed devices.
const FAILED_RETRY: Duration = Duration::from_secs(60);
/// A stream running this long without faults resets the retry counter.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// A running stream that makes no progress for this long is restarted.
const STALL_TIMEOUT: Duration = Duration::from_secs(2);
/// Supervisor tick interval.
const TICK: Duration = Duration::from_millis(20);
/// Delay before persisting configuration changes.
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
/// Recovery log length.
const RECOVERY_LOG_LEN: usize = 100;
/// Identical events closer together than this are merged into one entry.
const EVENT_MERGE_SECS: f64 = 10.0;

/// An IPC request forwarded to the supervisor with a reply channel.
#[derive(Debug)]
pub struct IpcCall {
    /// The request.
    pub request: Request,
    /// Where to send the answer.
    pub reply: mpsc::SyncSender<Response>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Input { strip: usize },
    Output { bus: usize },
    Master { bus: usize },
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Input { .. } => "input",
            Self::Output { .. } => "output",
            Self::Master { .. } => "master",
        }
    }
}

enum SlotState {
    Running {
        _stream: Box<OpenStream>,
        since: Instant,
        direct_master: bool,
    },
    Waiting {
        retry_at: Instant,
    },
    Failed {
        retry_at: Instant,
    },
    /// No device selected (the user chose "none"); never opened.
    Unused,
}

struct Slot {
    role: Role,
    label: String,
    selector: DeviceSelector,
    state: SlotState,
    attempts: u32,
    generation: u64,
    info: Option<StreamInfo>,
    fifo: Option<Arc<FifoStats>>,
    last_error: Option<String>,
    progress: (u64, Instant),
    /// Underrun/overrun counts already reported, to log only changes.
    seen_xruns: (u64, u64),
    /// Latency bookkeeping for the recovery log.
    latency: LatencyWatch,
}

/// Turns step-wise FIFO target changes into start/end events instead of one
/// log line per 1 ms step.
#[derive(Debug, Clone, Copy, Default)]
struct LatencyWatch {
    target: u32,
    shrinking_since: Option<u32>,
    last_change: Option<Instant>,
}

/// A shrink is considered finished after this long without another step.
const SHRINK_SETTLED: Duration = Duration::from_secs(15);

/// A successfully opened device stream and how to wire it into the engine.
struct Opened {
    stream: OpenStream,
    fifo: Option<Arc<FifoStats>>,
    direct_master: bool,
    command: Command,
}

/// Identifies an opened stream across all slots (slots can be removed, so
/// events cannot refer to them by index).
fn next_generation() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Longest strip or bus name accepted.
const MAX_NAME_CHARS: usize = 32;

struct DeviceEvent {
    generation: u64,
    fault: StreamFault,
}

/// Owns devices, recovery and control of one engine instance.
pub struct Supervisor {
    audio: AudioSystem,
    config: Config,
    config_path: Option<PathBuf>,
    dirty_since: Option<Instant>,
    core: SharedCore,
    handle: EngineHandle,
    meters: MixerMeters,
    sample_rate: u32,
    slots: Vec<Slot>,
    events_tx: Sender<DeviceEvent>,
    events_rx: Receiver<DeviceEvent>,
    soft_clock: Option<SoftwareClock>,
    started: Instant,
    process_start: Instant,
    first_audio_logged: bool,
    recovery_log: VecDeque<RecoveryEvent>,
    last_error: Option<String>,
    master_progress: (u64, Instant),
    /// Engine settings the engine started with, and the id of the bus that
    /// clocked it then (to tell when a restart is needed).
    started_with: (mixercraft_config::EngineConfig, Option<String>),
}

fn selector_for_source(s: &Source) -> Option<DeviceSelector> {
    match s {
        Source::None => None,
        Source::DefaultInput => Some(DeviceSelector::DefaultInput),
        Source::Device(d) => Some(DeviceSelector::Device { id: d.id.clone(), name: d.name.clone() }),
    }
}

fn selector_for_sink(s: &Sink) -> Option<DeviceSelector> {
    match s {
        Sink::None => None,
        Sink::DefaultOutput => Some(DeviceSelector::DefaultOutput),
        Sink::Device(d) => Some(DeviceSelector::Device { id: d.id.clone(), name: d.name.clone() }),
    }
}

/// Builds the mixer topology described by a configuration.
#[must_use]
pub fn layout_from_config(config: &Config) -> MixerLayout {
    MixerLayout {
        strips: config
            .strips
            .iter()
            .map(|s| StripSpec {
                channels: usize::from(s.channels),
                gain_db: s.gain_db,
                mute: s.mute,
                solo: s.solo,
                routes: s.routes.iter().filter_map(|r| config.bus_index(r)).fold(0u64, |m, b| m | (1u64 << b)),
                fx: fx_params(&s.fx),
            })
            .collect(),
        buses: config
            .buses
            .iter()
            .map(|b| BusSpec {
                channels: usize::from(b.channels),
                gain_db: b.gain_db,
                mute: b.mute,
                keep_awake: keep_awake_params(&b.keep_awake),
            })
            .collect(),
    }
}

fn keep_awake_params(k: &mixercraft_config::KeepAwakeConfig) -> KeepAwakeParams {
    KeepAwakeParams {
        enabled: k.enabled,
        mode: match k.mode {
            mixercraft_config::KeepAwakeMode::Pulse => KeepAwakeMode::Pulse,
            mixercraft_config::KeepAwakeMode::Infrasound => KeepAwakeMode::Infrasound,
        },
        level_db: k.level_db,
        frequency_hz: k.frequency_hz,
        interval_s: k.interval_s,
    }
}

/// The configuration form of keep-awake settings from a client.
fn keep_awake_config(k: &mixercraft_ipc::KeepAwake) -> mixercraft_config::KeepAwakeConfig {
    let p = KeepAwakeParams {
        enabled: k.enabled,
        mode: match k.mode {
            mixercraft_ipc::KeepAwakeMode::Pulse => KeepAwakeMode::Pulse,
            mixercraft_ipc::KeepAwakeMode::Infrasound => KeepAwakeMode::Infrasound,
        },
        level_db: k.level_db,
        frequency_hz: k.frequency_hz,
        interval_s: k.interval_s,
    }
    .sanitized();
    mixercraft_config::KeepAwakeConfig {
        enabled: p.enabled,
        mode: match p.mode {
            KeepAwakeMode::Pulse => mixercraft_config::KeepAwakeMode::Pulse,
            KeepAwakeMode::Infrasound => mixercraft_config::KeepAwakeMode::Infrasound,
        },
        level_db: p.level_db,
        frequency_hz: p.frequency_hz,
        interval_s: p.interval_s,
    }
}

/// The client form of configured keep-awake settings.
fn keep_awake_status(k: &mixercraft_config::KeepAwakeConfig) -> mixercraft_ipc::KeepAwake {
    mixercraft_ipc::KeepAwake {
        enabled: k.enabled,
        mode: match k.mode {
            mixercraft_config::KeepAwakeMode::Pulse => mixercraft_ipc::KeepAwakeMode::Pulse,
            mixercraft_config::KeepAwakeMode::Infrasound => mixercraft_ipc::KeepAwakeMode::Infrasound,
        },
        level_db: k.level_db,
        frequency_hz: k.frequency_hz,
        interval_s: k.interval_s,
    }
}

fn fx_params(fx: &mixercraft_config::FxConfig) -> FxParams {
    FxParams {
        gate_threshold_db: fx.gate_threshold_db,
        comp_amount: fx.comp_amount,
        eq_low_db: fx.eq_low_db,
        eq_mid_db: fx.eq_mid_db,
        eq_high_db: fx.eq_high_db,
        noise_suppression: fx.noise_suppression,
    }
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(250u64.saturating_mul(1 << attempt.min(6))).min(Duration::from_secs(10))
}

impl Supervisor {
    /// Builds the engine for `config`. Devices are opened by [`Self::run`].
    ///
    /// # Errors
    /// If the mixer cannot be built from the configuration.
    pub fn new(config: Config, config_path: Option<PathBuf>, process_start: Instant) -> anyhow::Result<Self> {
        let audio = AudioSystem::new();
        info!(backend = %audio.backend_name(), "audio backend selected");
        if let Some(path) = &config_path {
            audio.set_virtual_memory_file(path.with_file_name("virtual-defaults.txt"));
        }
        let master_bus = config.master_bus_index();
        let request = Self::request(&config, config.engine.sample_rate);

        // Run the engine at the master device's native rate so the master
        // path needs no resampling at all.
        let sample_rate = master_bus
            .and_then(|b| selector_for_sink(&config.buses[b].sink))
            .and_then(|sel| match audio.probe_output(&sel, request) {
                Ok(info) => Some(info.sample_rate),
                Err(e) => {
                    warn!("master device unavailable at startup: {e}");
                    None
                }
            })
            .unwrap_or(config.engine.sample_rate);

        let mixer = Mixer::new(&layout_from_config(&config), config.engine.max_block_frames as usize, sample_rate)
            .map_err(|e| anyhow::anyhow!("cannot build mixer: {e}"))?;
        let meters = mixer.meters();
        let (core, handle) = engine(mixer, sample_rate, None);

        let now = Instant::now();
        let mut slots = Vec::new();
        for (strip, s) in config.strips.iter().enumerate() {
            if let Some(selector) = selector_for_source(&s.source) {
                slots.push(Slot::new(Role::Input { strip }, s.name.clone(), selector, now));
            }
        }
        for (bus, b) in config.buses.iter().enumerate() {
            if let Some(selector) = selector_for_sink(&b.sink) {
                let role = if Some(bus) == master_bus { Role::Master { bus } } else { Role::Output { bus } };
                slots.push(Slot::new(role, b.name.clone(), selector, now));
            }
        }
        // Open the master first: it is the clock everything else follows.
        slots.sort_by_key(|s| !matches!(s.role, Role::Master { .. }));

        let (events_tx, events_rx) = mpsc::channel();
        info!(sample_rate, devices = slots.len(), "engine configured");
        let started_with = (config.engine.clone(), master_bus.map(|b| config.buses[b].id.clone()));
        Ok(Self {
            audio,
            config,
            config_path,
            dirty_since: None,
            core: Arc::new(Mutex::new(core)),
            handle,
            meters,
            sample_rate,
            slots,
            events_tx,
            events_rx,
            soft_clock: None,
            started: now,
            process_start,
            first_audio_logged: false,
            recovery_log: VecDeque::new(),
            last_error: None,
            master_progress: (0, now),
            started_with,
        })
    }

    fn request(config: &Config, sample_rate: u32) -> StreamRequest {
        StreamRequest {
            sample_rate: Some(sample_rate),
            buffer_frames: config.engine.latency_profile.device_buffer_frames(),
        }
    }

    /// Runs until `shutdown` is set or a client requests shutdown.
    pub fn run(mut self, shutdown: &Arc<AtomicBool>, ipc: &Receiver<IpcCall>) {
        while !shutdown.load(Ordering::Relaxed) {
            self.tick();
            let deadline = Instant::now() + TICK;
            loop {
                match ipc.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(call) => {
                        let stop = call.request == Request::Shutdown;
                        let resp = self.handle_request(call.request);
                        let _ = call.reply.send(resp);
                        if stop {
                            shutdown.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                        break;
                    }
                }
            }
        }
        info!("shutting down");
        for slot in &mut self.slots {
            slot.state = SlotState::Waiting { retry_at: Instant::now() };
        }
        self.soft_clock = None;
        self.save_config(true);
    }

    fn tick(&mut self) {
        let now = Instant::now();
        while let Ok(ev) = self.events_rx.try_recv() {
            self.on_fault(ev);
        }
        for i in 0..self.slots.len() {
            self.watchdog(i, now);
            self.report_fifo_events(i);
            let due = match self.slots[i].state {
                SlotState::Waiting { retry_at } | SlotState::Failed { retry_at } => retry_at <= now,
                SlotState::Unused => false,
                SlotState::Running { since, .. } => {
                    if now - since > HEALTHY_AFTER {
                        self.slots[i].attempts = 0;
                    }
                    false
                }
            };
            if due {
                self.open_slot(i);
            }
        }
        self.manage_clock();
        self.handle.collect_garbage();
        self.save_config(false);
    }

    fn manage_clock(&mut self) {
        let master_running =
            self.slots.iter().any(|s| matches!(s.state, SlotState::Running { direct_master: true, .. }));
        if master_running {
            if self.soft_clock.take().is_some() {
                info!("master device clock active; software clock stopped");
            }
        } else if self.soft_clock.is_none() {
            let block = (self.sample_rate / 100) as usize; // 10 ms
            let block = block.min(self.config.engine.max_block_frames as usize);
            match SoftwareClock::start(Arc::clone(&self.core), Arc::clone(self.handle.stats()), self.sample_rate, block)
            {
                Ok(c) => {
                    info!("no master device; software clock active");
                    self.soft_clock = Some(c);
                }
                Err(e) => error!("cannot start software clock: {e}"),
            }
        }
        if master_running && !self.first_audio_logged {
            self.first_audio_logged = true;
            info!(startup_ms = self.process_start.elapsed().as_millis() as u64, "audio running");
        }
    }

    fn watchdog(&mut self, i: usize, now: Instant) {
        let slot = &self.slots[i];
        let SlotState::Running { since, direct_master, .. } = slot.state else { return };
        if now - since < STALL_TIMEOUT {
            return;
        }
        let counter = if direct_master {
            self.handle.stats().snapshot().cycles
        } else {
            match (&slot.fifo, slot.role) {
                (Some(f), Role::Input { .. }) => f.snapshot().writes,
                (Some(f), _) => f.snapshot().reads,
                (None, _) => return,
            }
        };
        let progress = if direct_master { &mut self.master_progress } else { &mut self.slots[i].progress };
        if counter != progress.0 {
            *progress = (counter, now);
        } else if now - progress.1 > STALL_TIMEOUT {
            self.fail_slot(i, "stream stalled (no callbacks)".into());
        }
    }

    /// Turns FIFO counter changes into timestamped recovery-log entries so
    /// glitches can be correlated with system events.
    fn report_fifo_events(&mut self, i: usize) {
        self.report_latency_changes(i);
        let slot = &mut self.slots[i];
        let Some(f) = &slot.fifo else { return };
        let snap = f.snapshot();
        let (under, over) = slot.seen_xruns;
        if (snap.underruns, snap.overruns) == (under, over) {
            return;
        }
        slot.seen_xruns = (snap.underruns, snap.overruns);
        let label = slot.label.clone();
        let detail = format!(
            "{} underrun(s), {} overrun(s) in total; FIFO target now {} frames",
            snap.underruns, snap.overruns, snap.target_frames
        );
        warn!(device = %label, underruns = snap.underruns, overruns = snap.overruns, target = snap.target_frames, "FIFO xrun");
        let event = if snap.underruns > under { "underrun" } else { "overrun" };
        self.log_event(&label, event, &detail);
    }

    /// Logs FIFO target increases, and the start and end of latency
    /// recovery (the reader shrinks its target in 1 ms steps).
    fn report_latency_changes(&mut self, i: usize) {
        let now = Instant::now();
        let slot = &mut self.slots[i];
        let Some(f) = &slot.fifo else { return };
        let target = f.snapshot().target_frames;
        let w = &mut slot.latency;
        let rate = slot.info.as_ref().map_or(48_000, |i| i.sample_rate).max(1);
        let ms = |frames: u32| f64::from(frames) * 1000.0 / f64::from(rate);
        let mut event = None;
        if w.target == 0 {
            w.target = target; // first observation after (re)opening
        } else if target > w.target {
            w.shrinking_since = None;
            event = Some(("latency raised", format!("FIFO target {:.1} → {:.1} ms", ms(w.target), ms(target))));
            w.target = target;
            w.last_change = Some(now);
        } else if target < w.target {
            if w.shrinking_since.is_none() {
                w.shrinking_since = Some(w.target);
                event = Some(("latency recovery started", format!("FIFO target {:.1} ms, shrinking", ms(w.target))));
            }
            w.target = target;
            w.last_change = Some(now);
        } else if let (Some(from), Some(at)) = (w.shrinking_since, w.last_change)
            && now - at > SHRINK_SETTLED
        {
            w.shrinking_since = None;
            event = Some(("latency recovered", format!("FIFO target {:.1} → {:.1} ms", ms(from), ms(target))));
        }
        if let Some((name, detail)) = event {
            let label = slot.label.clone();
            info!(device = %label, "{name}: {detail}");
            self.log_event(&label, name, &detail);
        }
    }

    fn on_fault(&mut self, ev: DeviceEvent) {
        let Some(i) = self.slots.iter().position(|s| s.generation == ev.generation) else { return };
        let slot = &self.slots[i];
        if !matches!(slot.state, SlotState::Running { .. }) {
            return; // stale event from a stream that was already replaced
        }
        match ev.fault.kind {
            FaultKind::Rerouted => {
                let label = slot.label.clone();
                self.log_event(&label, "rerouted", &ev.fault.message);
            }
            FaultKind::Xrun => {
                let label = slot.label.clone();
                self.log_event(&label, "backend xrun", &ev.fault.message);
            }
            FaultKind::DeviceLost | FaultKind::StreamInvalidated | FaultKind::Other => {
                self.fail_slot(i, ev.fault.message);
            }
        }
    }

    fn fail_slot(&mut self, i: usize, reason: String) {
        let now = Instant::now();
        let slot = &mut self.slots[i];
        let was_direct = matches!(slot.state, SlotState::Running { direct_master: true, .. });
        slot.attempts += 1;
        slot.state = if slot.attempts > MAX_FAST_ATTEMPTS {
            SlotState::Failed { retry_at: now + FAILED_RETRY }
        } else {
            SlotState::Waiting { retry_at: now + backoff(slot.attempts) }
        };
        slot.last_error = Some(reason.clone());
        slot.fifo = None;
        let (role, label, attempts) = (slot.role, slot.label.clone(), slot.attempts);
        let cmd = match role {
            Role::Input { strip } => Some(Command::DetachInput { strip }),
            Role::Output { bus } => Some(Command::DetachOutput { bus }),
            Role::Master { .. } if was_direct => Some(Command::SetMasterBus(None)),
            Role::Master { bus } => Some(Command::DetachOutput { bus }),
        };
        if let Some(c) = cmd {
            self.send(c);
        }
        if attempts > MAX_FAST_ATTEMPTS {
            error!(device = %label, attempts, "device keeps failing; retrying every {}s: {reason}", FAILED_RETRY.as_secs());
            self.log_event(&label, "failed", &reason);
        } else {
            warn!(device = %label, attempts, "device fault, will reopen: {reason}");
            self.log_event(&label, "fault", &reason);
        }
        self.last_error = Some(format!("{label}: {reason}"));
        if was_direct {
            self.manage_clock();
        }
    }

    fn open_slot(&mut self, i: usize) {
        let request = Self::request(&self.config, self.sample_rate);
        let profile_ms = self.config.engine.latency_profile.fifo_target_ms();
        let engine_rate = self.sample_rate;
        let slot = &mut self.slots[i];
        slot.generation = next_generation();
        let gen_ = slot.generation;
        let (label_selector, label_text, label_speaker) =
            (slot.selector.clone(), slot.label.clone(), matches!(slot.role, Role::Input { .. }));
        let tx = self.events_tx.clone();
        let on_error = move |fault: StreamFault| {
            let _ = tx.send(DeviceEvent { generation: gen_, fault });
        };
        let selector = slot.selector.clone();
        let role = slot.role;

        let result: Result<Opened, String> = (|| match role {
            Role::Input { strip } => {
                let info = self.audio.probe_input(&selector, request).map_err(|e| e.to_string())?;
                let spec = FifoSpec::new(usize::from(info.channels), info.sample_rate, engine_rate, profile_ms);
                let (mut w, r) = fifo(spec);
                let stats = Arc::clone(w.stats());
                let stream = self
                    .audio
                    .open_input(
                        &selector,
                        request,
                        move |d| {
                            w.write(d);
                        },
                        on_error,
                    )
                    .map_err(|e| e.to_string())?;
                check_matches(&info, &stream.info)?;
                Ok(Opened {
                    stream,
                    fifo: Some(stats),
                    direct_master: false,
                    command: Command::AttachInput { strip, reader: Box::new(r) },
                })
            }
            Role::Master { bus } | Role::Output { bus } => {
                let info = self.audio.probe_output(&selector, request).map_err(|e| e.to_string())?;
                // A virtual device never clocks the engine: its server
                // threads are not real-time threads.
                let is_virtual = mixercraft_platform::virtual_dev::virtual_name(&selector).is_some();
                if matches!(role, Role::Master { .. }) && info.sample_rate == engine_rate && !is_virtual {
                    // Direct path: the device callback drives the engine.
                    self.soft_clock = None;
                    let core = Arc::clone(&self.core);
                    let stats = Arc::clone(self.handle.stats());
                    let ch = usize::from(info.channels);
                    let stream = self
                        .audio
                        .open_output(
                            &selector,
                            request,
                            move |out| {
                                if !drive(&core, |c| c.render_master(out, ch)) {
                                    out.fill(0.0);
                                    stats.record_skipped_cycle();
                                }
                            },
                            on_error,
                        )
                        .map_err(|e| e.to_string())?;
                    check_matches(&info, &stream.info)?;
                    return Ok(Opened {
                        stream,
                        fifo: None,
                        direct_master: true,
                        command: Command::SetMasterBus(Some(bus)),
                    });
                }
                // Clock-decoupled path through a drift-compensated FIFO.
                let spec = FifoSpec::new(usize::from(info.channels), engine_rate, info.sample_rate, profile_ms);
                let (w, mut r) = fifo(spec);
                let stats = Arc::clone(r.stats());
                let stream = self
                    .audio
                    .open_output(
                        &selector,
                        request,
                        move |out| {
                            r.read(out);
                        },
                        on_error,
                    )
                    .map_err(|e| e.to_string())?;
                check_matches(&info, &stream.info)?;
                Ok(Opened {
                    stream,
                    fifo: Some(stats),
                    direct_master: false,
                    command: Command::AttachOutput { bus, writer: Box::new(w) },
                })
            }
        })();

        match result {
            Ok(Opened { stream, fifo: fifo_stats, direct_master, command }) => {
                // Virtual devices show the strip's or bus's name in Windows.
                self.audio.set_virtual_label(&label_selector, label_speaker, &label_text);
                let slot = &mut self.slots[i];
                info!(
                    device = %stream.info.device_name,
                    role = role.name(),
                    rate = stream.info.sample_rate,
                    channels = stream.info.channels,
                    format = %stream.info.sample_format,
                    buffer = ?stream.info.buffer_frames,
                    "device opened"
                );
                let reopened = slot.last_error.is_some();
                slot.info = Some(stream.info.clone());
                slot.fifo = fifo_stats;
                slot.progress = (0, Instant::now());
                slot.seen_xruns = (0, 0);
                slot.latency = LatencyWatch::default();
                slot.state = SlotState::Running { _stream: Box::new(stream), since: Instant::now(), direct_master };
                let label = slot.label.clone();
                if direct_master {
                    self.master_progress = (self.handle.stats().snapshot().cycles, Instant::now());
                }
                self.send(command);
                if reopened {
                    self.log_event(&label, "recovered", "stream reopened");
                }
            }
            Err(reason) => {
                // `fail_slot` expects a running slot; mark the attempt here.
                let now = Instant::now();
                let slot = &mut self.slots[i];
                slot.attempts += 1;
                let failed = slot.attempts > MAX_FAST_ATTEMPTS;
                slot.state = if failed {
                    SlotState::Failed { retry_at: now + FAILED_RETRY }
                } else {
                    SlotState::Waiting { retry_at: now + backoff(slot.attempts) }
                };
                let first = slot.last_error.as_deref() != Some(reason.as_str());
                slot.last_error = Some(reason.clone());
                let label = slot.label.clone();
                if first || failed {
                    warn!(device = %label, "cannot open device: {reason}");
                    self.log_event(&label, if failed { "failed" } else { "open failed" }, &reason);
                }
                self.last_error = Some(format!("{label}: {reason}"));
            }
        }
    }

    /// Closes slot `i`'s stream (if any) and detaches it from the engine.
    /// Pending device events for the old stream become stale.
    fn release_slot(&mut self, i: usize) {
        let slot = &mut self.slots[i];
        let running = matches!(slot.state, SlotState::Running { .. });
        let was_direct = matches!(slot.state, SlotState::Running { direct_master: true, .. });
        slot.generation = next_generation();
        slot.state = SlotState::Unused;
        slot.fifo = None;
        slot.info = None;
        slot.attempts = 0;
        slot.last_error = None;
        slot.seen_xruns = (0, 0);
        slot.latency = LatencyWatch::default();
        let role = slot.role;
        if running {
            let cmd = match role {
                Role::Input { strip } => Command::DetachInput { strip },
                Role::Output { bus } => Command::DetachOutput { bus },
                Role::Master { .. } if was_direct => Command::SetMasterBus(None),
                Role::Master { bus } => Command::DetachOutput { bus },
            };
            self.send(cmd);
        }
    }

    /// Points a strip (`bus == false`) or bus at a different device, or at
    /// none. The new device is opened on the next tick.
    fn reassign(&mut self, bus: bool, index: usize, selector: Option<DeviceSelector>, label: String) {
        let matches = |r: Role| match r {
            Role::Input { strip } => !bus && strip == index,
            Role::Output { bus: b } | Role::Master { bus: b } => bus && b == index,
        };
        let now = Instant::now();
        if let Some(i) = self.slots.iter().position(|s| matches(s.role)) {
            self.release_slot(i);
            let detail = selector.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string);
            let slot = &mut self.slots[i];
            slot.label = label.clone();
            if let Some(sel) = selector {
                slot.selector = sel;
                slot.state = SlotState::Waiting { retry_at: now };
            }
            self.log_event(&label, "device changed", &detail);
        } else if let Some(sel) = selector {
            let role = if bus {
                let has_master = self.slots.iter().any(|s| matches!(s.role, Role::Master { .. }));
                if !has_master && self.config.master_bus_index() == Some(index) {
                    Role::Master { bus: index }
                } else {
                    Role::Output { bus: index }
                }
            } else {
                Role::Input { strip: index }
            };
            self.log_event(&label, "device changed", &sel.to_string());
            self.slots.push(Slot::new(role, label, sel, now));
        }
        self.manage_clock();
    }

    /// Swaps in a mixer built from `config`, which differs from the current
    /// one by an added strip/bus or the removed `remove_input` /
    /// `remove_output`. Device slots are renumbered to match.
    fn reshape(&mut self, config: Config, remove_input: Option<usize>, remove_output: Option<usize>) -> Response {
        if let Err(e) = config.validate() {
            return Response::Error { code: ErrorCode::InvalidArgument, message: e.to_string() };
        }
        let mixer =
            match Mixer::new(&layout_from_config(&config), config.engine.max_block_frames as usize, self.sample_rate) {
                Ok(m) => m,
                Err(e) => return Response::Error { code: ErrorCode::InvalidArgument, message: e.to_string() },
            };
        let meters = mixer.meters();
        if let Err(c) = self.handle.send(Command::Reshape { mixer: Box::new(mixer), remove_input, remove_output }) {
            drop(c);
            return Response::Error { code: ErrorCode::Busy, message: "engine command queue full".into() };
        }
        // The engine has dropped the removed FIFO; close its device and
        // renumber the remaining slots.
        let shift = |idx: &mut usize, removed: Option<usize>| {
            if removed.is_some_and(|r| *idx > r) {
                *idx -= 1;
            }
        };
        self.slots.retain(|s| match s.role {
            Role::Input { strip } => Some(strip) != remove_input,
            Role::Output { bus } | Role::Master { bus } => Some(bus) != remove_output,
        });
        for slot in &mut self.slots {
            match &mut slot.role {
                Role::Input { strip } => shift(strip, remove_input),
                Role::Output { bus } | Role::Master { bus } => shift(bus, remove_output),
            }
        }
        self.meters = meters;
        self.config = config;
        self.dirty_since = Some(Instant::now());
        self.manage_clock();
        Response::Ok
    }

    fn add_strip(&mut self, name: &str) -> Response {
        let mut config = self.config.clone();
        let name =
            if name.trim().is_empty() { format!("Input {}", config.strips.len() + 1) } else { name.trim().into() };
        config.strips.push(StripConfig {
            name: name.clone(),
            source: Source::None,
            channels: 2,
            gain_db: 0.0,
            mute: false,
            solo: false,
            routes: Vec::new(),
            fx: FxConfig::default(),
        });
        let r = self.reshape(config, None, None);
        if matches!(r, Response::Ok) {
            info!(strip = %name, "strip added");
        }
        r
    }

    fn add_bus(&mut self, name: &str) -> Response {
        let mut config = self.config.clone();
        let id = (1..).map(|n| format!("A{n}")).find(|id| config.bus_index(id).is_none()).unwrap_or_default();
        let name =
            if name.trim().is_empty() { format!("Output {}", config.buses.len() + 1) } else { name.trim().into() };
        config.buses.push(BusConfig {
            id,
            name: name.clone(),
            sink: Sink::None,
            channels: 2,
            gain_db: 0.0,
            mute: false,
            keep_awake: mixercraft_config::KeepAwakeConfig::default(),
        });
        let r = self.reshape(config, None, None);
        if matches!(r, Response::Ok) {
            info!(bus = %name, "bus added");
        }
        r
    }

    fn remove_strip(&mut self, i: usize) -> Response {
        if i >= self.config.strips.len() {
            return Response::Error { code: ErrorCode::InvalidArgument, message: "no such strip".into() };
        }
        let mut config = self.config.clone();
        let removed = config.strips.remove(i);
        let r = self.reshape(config, Some(i), None);
        if matches!(r, Response::Ok) {
            info!(strip = %removed.name, "strip removed");
            self.log_event(&removed.name, "removed", "strip removed");
        }
        r
    }

    fn remove_bus(&mut self, b: usize) -> Response {
        if b >= self.config.buses.len() {
            return Response::Error { code: ErrorCode::InvalidArgument, message: "no such bus".into() };
        }
        if self.config.master_bus_index() == Some(b) {
            return Response::Error {
                code: ErrorCode::InvalidArgument,
                message: "the master bus clocks the engine and cannot be removed".into(),
            };
        }
        let mut config = self.config.clone();
        let removed = config.buses.remove(b);
        for s in &mut config.strips {
            s.routes.retain(|r| *r != removed.id);
        }
        // Keep the same master bus (it is chosen by id, or as the first bus
        // with a device).
        if config.engine.master_bus.is_none() {
            let old_master = self.config.master_bus_index().map(|m| self.config.buses[m].id.clone());
            config.engine.master_bus = old_master;
        }
        let r = self.reshape(config, None, Some(b));
        if matches!(r, Response::Ok) {
            info!(bus = %removed.name, "bus removed");
            self.log_event(&removed.name, "removed", "bus removed");
        }
        r
    }

    /// Stores engine-wide settings; they apply at the next engine start.
    fn set_engine_settings(&mut self, settings: &EngineSettings) -> Response {
        let mut config = self.config.clone();
        config.engine.sample_rate = settings.sample_rate;
        config.engine.latency_profile = match settings.latency_profile {
            LatencyProfile::LowLatency => mixercraft_config::LatencyProfile::LowLatency,
            LatencyProfile::Balanced => mixercraft_config::LatencyProfile::Balanced,
            LatencyProfile::MaxStability => mixercraft_config::LatencyProfile::MaxStability,
        };
        config.engine.master_bus = settings.master_bus.clone().filter(|id| !id.is_empty());
        if let Err(e) = config.validate() {
            return Response::Error { code: ErrorCode::InvalidArgument, message: e.to_string() };
        }
        if config.engine != self.config.engine {
            info!(?settings, "engine settings changed (apply at next start)");
            self.config = config;
            self.dirty_since = Some(Instant::now());
            self.save_config(true);
        }
        Response::Ok
    }

    /// Whether the configured engine settings differ from those the engine
    /// started with.
    fn restart_required(&self) -> bool {
        let (started, started_master) = &self.started_with;
        let now = &self.config.engine;
        let master_id = self.config.master_bus_index().map(|b| self.config.buses[b].id.clone());
        now.sample_rate != started.sample_rate
            || now.latency_profile != started.latency_profile
            // Removing a bus pins an automatic master by id; only a change
            // of the clocking bus itself needs a restart.
            || (now.master_bus != started.master_bus && master_id != *started_master)
    }

    fn rename(&mut self, bus: bool, i: usize, name: &str) -> Response {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
            return Response::Error {
                code: ErrorCode::InvalidArgument,
                message: format!("names must have 1 to {MAX_NAME_CHARS} characters"),
            };
        }
        let target = if bus {
            self.config.buses.get_mut(i).map(|b| &mut b.name)
        } else {
            self.config.strips.get_mut(i).map(|s| &mut s.name)
        };
        let Some(target) = target else {
            return Response::Error { code: ErrorCode::InvalidArgument, message: "no such strip or bus".into() };
        };
        *target = name.into();
        for slot in &mut self.slots {
            let ours = match slot.role {
                Role::Input { strip } => !bus && strip == i,
                Role::Output { bus: b } | Role::Master { bus: b } => bus && b == i,
            };
            if ours {
                slot.label = name.into();
                self.audio.set_virtual_label(&slot.selector, !bus, name);
            }
        }
        self.dirty_since = Some(Instant::now());
        Response::Ok
    }

    fn send(&mut self, cmd: Command) {
        if let Err(c) = self.handle.send(cmd) {
            // The queue holds 256 commands and is drained every cycle; being
            // full means the audio thread is not running. Dropping a
            // structural command here would desynchronise state, so report.
            error!("engine command queue full; dropped {:?}", std::mem::discriminant(&*c));
        }
    }

    fn log_event(&mut self, device: &str, event: &str, detail: &str) {
        let now = self.started.elapsed().as_secs_f64();
        // Merge bursts of identical events (e.g. backend xruns while the
        // system is overloaded) so they cannot flush the log.
        if let Some(last) = self.recovery_log.back_mut()
            && last.device == device
            && last.event == event
            && last.detail == detail
            && now - last.at_secs < EVENT_MERGE_SECS
        {
            last.count = last.count.max(1) + 1;
            last.at_secs = now;
            return;
        }
        if self.recovery_log.len() == RECOVERY_LOG_LEN {
            self.recovery_log.pop_front();
        }
        self.recovery_log.push_back(RecoveryEvent {
            at_secs: now,
            device: device.into(),
            event: event.into(),
            detail: detail.into(),
            count: 1,
        });
    }

    fn save_config(&mut self, force: bool) {
        let Some(since) = self.dirty_since else { return };
        if !force && since.elapsed() < SAVE_DEBOUNCE {
            return;
        }
        self.dirty_since = None;
        if let Some(path) = &self.config_path
            && let Err(e) = mixercraft_config::save_file(path, &self.config)
        {
            error!("cannot save configuration: {e}");
        }
    }

    fn handle_request(&mut self, req: Request) -> Response {
        if !matches!(req, Request::Status | Request::Meters | Request::Devices) {
            debug!(?req, "request");
        }
        let invalid = |m: &str| Response::Error { code: ErrorCode::InvalidArgument, message: m.into() };
        let strips = self.config.strips.len();
        let buses = self.config.buses.len();
        let gain_ok = |db: f32| db.is_finite() && (-200.0..=24.0).contains(&db);
        let cmd = match req {
            Request::Hello { .. } | Request::Shutdown => return Response::Ok,
            Request::SetStripSource { strip, choice } => {
                let i = usize::from(strip);
                let Some(s) = self.config.strips.get_mut(i) else { return invalid("no such strip") };
                s.source = match choice {
                    DeviceChoice::None => Source::None,
                    DeviceChoice::Default => Source::DefaultInput,
                    DeviceChoice::Device { id, name } => Source::Device(DeviceRef { id, name }),
                };
                let (selector, label) = (selector_for_source(&s.source), s.name.clone());
                self.reassign(false, i, selector, label);
                self.dirty_since = Some(Instant::now());
                return Response::Ok;
            }
            Request::SetBusSink { bus, choice } => {
                let i = usize::from(bus);
                let Some(b) = self.config.buses.get_mut(i) else { return invalid("no such bus") };
                b.sink = match choice {
                    DeviceChoice::None => Sink::None,
                    DeviceChoice::Default => Sink::DefaultOutput,
                    DeviceChoice::Device { id, name } => Sink::Device(DeviceRef { id, name }),
                };
                let (selector, label) = (selector_for_sink(&b.sink), b.name.clone());
                self.reassign(true, i, selector, label);
                self.dirty_since = Some(Instant::now());
                return Response::Ok;
            }
            Request::SetEngineSettings { settings } => return self.set_engine_settings(&settings),
            Request::AddStrip { name } => return self.add_strip(&name),
            Request::AddBus { name } => return self.add_bus(&name),
            Request::RemoveStrip { strip } => return self.remove_strip(usize::from(strip)),
            Request::RemoveBus { bus } => return self.remove_bus(usize::from(bus)),
            Request::RenameStrip { strip, name } => return self.rename(false, usize::from(strip), &name),
            Request::RenameBus { bus, name } => return self.rename(true, usize::from(bus), &name),
            Request::TestBusKeepAwake { bus } => {
                if usize::from(bus) >= self.config.buses.len() {
                    return invalid("no such bus");
                }
                // A test changes nothing worth saving.
                return match self.handle.send(Command::Mixer(MixerCommand::BusKeepAwakeTest { bus })) {
                    Ok(()) => Response::Ok,
                    Err(_) => Response::Error { code: ErrorCode::Busy, message: "engine command queue full".into() },
                };
            }
            Request::Status => return Response::Status(Box::new(self.status())),
            Request::Meters => return Response::Meters(self.meter_snapshot()),
            Request::Devices => {
                return match self.audio.devices() {
                    Ok(d) => Response::Devices {
                        devices: d
                            .into_iter()
                            .map(|d| DeviceSummary {
                                id: d.id,
                                name: d.name,
                                is_input: d.is_input,
                                is_output: d.is_output,
                                is_default_input: d.is_default_input,
                                is_default_output: d.is_default_output,
                                default_sample_rate: d.default_sample_rate,
                                default_channels: d.default_channels,
                            })
                            .collect(),
                    },
                    Err(e) => Response::Error { code: ErrorCode::Internal, message: e.to_string() },
                };
            }
            Request::SetStripGain { strip, db } => {
                let Some(s) = self.config.strips.get_mut(usize::from(strip)) else { return invalid("no such strip") };
                if !gain_ok(db) {
                    return invalid("gain out of range");
                }
                s.gain_db = db;
                MixerCommand::StripGain { strip, db }
            }
            Request::SetStripMute { strip, mute } => {
                let Some(s) = self.config.strips.get_mut(usize::from(strip)) else { return invalid("no such strip") };
                s.mute = mute;
                MixerCommand::StripMute { strip, mute }
            }
            Request::SetStripSolo { strip, solo } => {
                let Some(s) = self.config.strips.get_mut(usize::from(strip)) else { return invalid("no such strip") };
                s.solo = solo;
                MixerCommand::StripSolo { strip, solo }
            }
            Request::SetRoute { strip, bus, enabled } => {
                if usize::from(strip) >= strips || usize::from(bus) >= buses {
                    return invalid("no such strip or bus");
                }
                let id = self.config.buses[usize::from(bus)].id.clone();
                let routes = &mut self.config.strips[usize::from(strip)].routes;
                routes.retain(|r| *r != id);
                if enabled {
                    routes.push(id);
                }
                MixerCommand::Route { strip, bus, enabled }
            }
            Request::SetBusGain { bus, db } => {
                let Some(b) = self.config.buses.get_mut(usize::from(bus)) else { return invalid("no such bus") };
                if !gain_ok(db) {
                    return invalid("gain out of range");
                }
                b.gain_db = db;
                MixerCommand::BusGain { bus, db }
            }
            Request::SetBusMute { bus, mute } => {
                let Some(b) = self.config.buses.get_mut(usize::from(bus)) else { return invalid("no such bus") };
                b.mute = mute;
                MixerCommand::BusMute { bus, mute }
            }
            Request::SetStripFx { strip, fx } => {
                let Some(s) = self.config.strips.get_mut(usize::from(strip)) else { return invalid("no such strip") };
                // Clamp to the valid ranges so the stored config stays valid.
                let p = FxParams {
                    gate_threshold_db: fx.gate_threshold_db,
                    comp_amount: fx.comp_amount,
                    eq_low_db: fx.eq_low_db,
                    eq_mid_db: fx.eq_mid_db,
                    eq_high_db: fx.eq_high_db,
                    noise_suppression: fx.noise_suppression,
                }
                .sanitized();
                s.fx = mixercraft_config::FxConfig {
                    gate_threshold_db: p.gate_threshold_db,
                    comp_amount: p.comp_amount,
                    eq_low_db: p.eq_low_db,
                    eq_mid_db: p.eq_mid_db,
                    eq_high_db: p.eq_high_db,
                    noise_suppression: p.noise_suppression,
                };
                MixerCommand::StripFx { strip, params: p }
            }
            Request::SetBusKeepAwake { bus, keep_awake } => {
                let Some(b) = self.config.buses.get_mut(usize::from(bus)) else { return invalid("no such bus") };
                b.keep_awake = keep_awake_config(&keep_awake);
                MixerCommand::BusKeepAwake { bus, params: keep_awake_params(&b.keep_awake) }
            }
        };
        match self.handle.send(Command::Mixer(cmd)) {
            Ok(()) => {
                self.dirty_since = Some(Instant::now());
                Response::Ok
            }
            Err(_) => Response::Error { code: ErrorCode::Busy, message: "engine command queue full".into() },
        }
    }

    fn meter_snapshot(&self) -> MeterSnapshot {
        let conv = |banks: &[Arc<mixercraft_core::meter::MeterBank>]| {
            banks
                .iter()
                .map(|b| b.take_peaks().into_iter().map(|l| Level { peak: l.peak, rms: l.rms }).collect())
                .collect()
        };
        MeterSnapshot { strips: conv(&self.meters.strips), buses: conv(&self.meters.buses) }
    }

    fn status(&self) -> EngineStatus {
        let e = self.handle.stats().snapshot();
        let all_running = self.slots.iter().all(|s| matches!(s.state, SlotState::Running { .. } | SlotState::Unused));
        let state = if all_running {
            EngineState::Running
        } else if self.started.elapsed() < Duration::from_secs(3) && self.recovery_log.is_empty() {
            EngineState::Starting
        } else {
            EngineState::Degraded
        };
        let clock_source = self
            .slots
            .iter()
            .find(|s| matches!(s.state, SlotState::Running { direct_master: true, .. }))
            .and_then(|s| s.info.as_ref().map(|i| i.device_name.clone()))
            .unwrap_or_else(|| "software clock".into());
        let source_label = |s: &Source| match s {
            Source::None => "none".to_string(),
            Source::DefaultInput => "default input".into(),
            Source::Device(d) => d.name.clone().or_else(|| d.id.clone()).unwrap_or_default(),
        };
        let sink_label = |s: &Sink| match s {
            Sink::None => "none".to_string(),
            Sink::DefaultOutput => "default output".into(),
            Sink::Device(d) => d.name.clone().or_else(|| d.id.clone()).unwrap_or_default(),
        };
        EngineStatus {
            engine_version: env!("CARGO_PKG_VERSION").into(),
            state,
            uptime_secs: self.started.elapsed().as_secs_f64(),
            backend: self.audio.backend_name(),
            sample_rate: self.sample_rate,
            max_block_frames: self.config.engine.max_block_frames,
            clock_source,
            load_permille: e.load_permille,
            peak_load_permille: e.peak_load_permille,
            overloads: e.overloads,
            cycles: e.cycles,
            non_finite_samples: e.non_finite,
            over_samples: e.over,
            rt_deallocations: e.rt_deallocations,
            skipped_cycles: e.skipped_cycles,
            devices: self
                .slots
                .iter()
                .filter(|s| !matches!(s.state, SlotState::Unused))
                .map(|s| DeviceStatus {
                    name: s.info.as_ref().map_or_else(|| s.selector.to_string(), |i| i.device_name.clone()),
                    role: s.role.name().into(),
                    state: match s.state {
                        SlotState::Running { .. } => "running",
                        SlotState::Waiting { .. } if s.attempts == 0 => "starting",
                        SlotState::Waiting { .. } => "recovering",
                        SlotState::Failed { .. } => "failed",
                        SlotState::Unused => "unused",
                    }
                    .into(),
                    sample_rate: s.info.as_ref().map_or(0, |i| i.sample_rate),
                    channels: s.info.as_ref().map_or(0, |i| i.channels),
                    buffer_frames: s.info.as_ref().and_then(|i| i.buffer_frames),
                    fifo: s.fifo.as_ref().map(|f| {
                        let f = f.snapshot();
                        FifoStatus {
                            fill_frames: f.fill_frames,
                            target_frames: f.target_frames,
                            underruns: f.underruns,
                            overruns: f.overruns,
                            dropped_frames: f.dropped_frames,
                            gap_reads: f.gap_reads,
                            drift_ppm: f.drift_ppm,
                        }
                    }),
                    error: s.last_error.clone(),
                })
                .collect(),
            strips: self
                .config
                .strips
                .iter()
                .map(|s| StripStatus {
                    name: s.name.clone(),
                    source: source_label(&s.source),
                    channels: s.channels,
                    gain_db: s.gain_db,
                    mute: s.mute,
                    solo: s.solo,
                    routes: s.routes.iter().filter_map(|r| self.config.bus_index(r)).map(|b| b as u16).collect(),
                    fx: mixercraft_ipc::StripFx {
                        gate_threshold_db: s.fx.gate_threshold_db,
                        comp_amount: s.fx.comp_amount,
                        eq_low_db: s.fx.eq_low_db,
                        eq_mid_db: s.fx.eq_mid_db,
                        eq_high_db: s.fx.eq_high_db,
                        noise_suppression: s.fx.noise_suppression,
                    },
                    source_choice: match &s.source {
                        Source::None => DeviceChoice::None,
                        Source::DefaultInput => DeviceChoice::Default,
                        Source::Device(d) => DeviceChoice::Device { id: d.id.clone(), name: d.name.clone() },
                    },
                })
                .collect(),
            buses: self
                .config
                .buses
                .iter()
                .enumerate()
                .map(|(i, b)| BusStatus {
                    master: self.config.master_bus_index() == Some(i),
                    keep_awake: keep_awake_status(&b.keep_awake),
                    id: b.id.clone(),
                    name: b.name.clone(),
                    sink: sink_label(&b.sink),
                    channels: b.channels,
                    gain_db: b.gain_db,
                    mute: b.mute,
                    sink_choice: match &b.sink {
                        Sink::None => DeviceChoice::None,
                        Sink::DefaultOutput => DeviceChoice::Default,
                        Sink::Device(d) => DeviceChoice::Device { id: d.id.clone(), name: d.name.clone() },
                    },
                })
                .collect(),
            recovery_events: self.recovery_log.iter().cloned().collect(),
            last_error: self.last_error.clone(),
            virtual_devices: self.audio.virtual_devices_available().is_ok(),
            virtual_devices_note: self.audio.virtual_devices_available().err().unwrap_or_default(),
            noise_suppression_available: Some(self.sample_rate == mixercraft_core::denoise::DENOISE_RATE),
            settings: Some(EngineSettings {
                sample_rate: self.config.engine.sample_rate,
                latency_profile: match self.config.engine.latency_profile {
                    mixercraft_config::LatencyProfile::LowLatency => LatencyProfile::LowLatency,
                    mixercraft_config::LatencyProfile::Balanced => LatencyProfile::Balanced,
                    mixercraft_config::LatencyProfile::MaxStability => LatencyProfile::MaxStability,
                },
                master_bus: self.config.engine.master_bus.clone(),
            }),
            restart_required: self.restart_required(),
            config_path: self.config_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        }
    }
}

impl Slot {
    fn new(role: Role, label: String, selector: DeviceSelector, now: Instant) -> Self {
        Self {
            role,
            label,
            selector,
            state: SlotState::Waiting { retry_at: now },
            attempts: 0,
            generation: 0,
            info: None,
            fifo: None,
            last_error: None,
            progress: (0, now),
            seen_xruns: (0, 0),
            latency: LatencyWatch::default(),
        }
    }
}

/// The stream must have been opened with what the FIFO was built for.
fn check_matches(probed: &StreamInfo, opened: &StreamInfo) -> Result<(), String> {
    if probed.sample_rate == opened.sample_rate && probed.channels == opened.channels {
        Ok(())
    } else {
        Err(format!(
            "device format changed while opening ({} Hz/{} ch → {} Hz/{} ch)",
            probed.sample_rate, probed.channels, opened.sample_rate, opened.channels
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_maps_routes_by_bus_id() {
        let mut c = Config::default();
        c.buses.push(mixercraft_config::BusConfig {
            id: "B1".into(),
            name: "Stream".into(),
            sink: Sink::None,
            channels: 1,
            gain_db: -3.0,
            mute: true,
            keep_awake: mixercraft_config::KeepAwakeConfig { enabled: true, ..Default::default() },
        });
        c.strips[0].routes = vec!["B1".into(), "A1".into()];
        let l = layout_from_config(&c);
        assert_eq!(l.strips[0].routes, 0b11);
        assert_eq!(l.buses[1].channels, 1);
        assert!(l.buses[1].mute);
        assert!(l.buses[1].keep_awake.enabled, "keep-awake reaches the mixer layout");
    }

    /// A supervisor with no devices configured (nothing is opened).
    fn deviceless() -> Supervisor {
        let mut c = Config::default();
        c.strips[0].source = Source::None;
        c.buses[0].sink = Sink::None;
        c.engine.master_bus = Some("A1".into());
        Supervisor::new(c, None, Instant::now()).unwrap()
    }

    fn ok(r: Response) {
        assert!(matches!(r, Response::Ok), "{r:?}");
    }

    #[test]
    fn engine_settings_are_stored_and_flag_a_restart() {
        let mut s = deviceless();
        ok(s.handle_request(Request::AddBus { name: String::new() }));
        let st = s.status();
        let current = st.settings.clone().expect("engine reports its settings");
        assert_eq!(current.sample_rate, 48_000);
        assert_eq!(current.master_bus.as_deref(), Some("A1"));
        assert!(!st.restart_required);

        // The same settings again: nothing to restart.
        ok(s.handle_request(Request::SetEngineSettings { settings: current }));
        assert!(!s.status().restart_required);

        let settings = EngineSettings {
            sample_rate: 44_100,
            latency_profile: LatencyProfile::MaxStability,
            master_bus: Some("A2".into()),
        };
        ok(s.handle_request(Request::SetEngineSettings { settings: settings.clone() }));
        assert_eq!(s.config.engine.sample_rate, 44_100);
        assert_eq!(s.config.engine.latency_profile, mixercraft_config::LatencyProfile::MaxStability);
        assert_eq!(s.config.master_bus_index(), Some(1));
        let st = s.status();
        assert_eq!(st.settings, Some(settings));
        assert!(st.restart_required);
        // The running engine keeps its rate until it restarts.
        assert_eq!(st.sample_rate, 48_000);

        // Back to the start values: no restart needed.
        let back = EngineSettings {
            sample_rate: 48_000,
            latency_profile: LatencyProfile::Balanced,
            master_bus: Some("A1".into()),
        };
        ok(s.handle_request(Request::SetEngineSettings { settings: back }));
        assert!(!s.status().restart_required);

        // Invalid values are refused and change nothing.
        let bad = EngineSettings { master_bus: Some("Z9".into()), ..EngineSettings::default() };
        assert!(matches!(s.handle_request(Request::SetEngineSettings { settings: bad }), Response::Error { .. }));
        let bad = EngineSettings { sample_rate: 1, ..EngineSettings::default() };
        assert!(matches!(s.handle_request(Request::SetEngineSettings { settings: bad }), Response::Error { .. }));
        assert_eq!(s.config.engine.sample_rate, 48_000);
    }

    #[test]
    fn keep_awake_settings_are_stored_and_testing_saves_nothing() {
        let mut s = deviceless();
        let keep_awake = mixercraft_ipc::KeepAwake {
            enabled: true,
            mode: mixercraft_ipc::KeepAwakeMode::Pulse,
            level_db: -10.0, // clamped to -20
            interval_s: 60.0,
            ..Default::default()
        };
        ok(s.handle_request(Request::SetBusKeepAwake { bus: 0, keep_awake }));
        let k = s.config.buses[0].keep_awake;
        assert_eq!((k.mode, k.level_db, k.interval_s), (mixercraft_config::KeepAwakeMode::Pulse, -20.0, 60.0));
        assert_eq!(s.status().buses[0].keep_awake.interval_s, 60.0);
        s.dirty_since = None;
        ok(s.handle_request(Request::TestBusKeepAwake { bus: 0 }));
        assert!(s.dirty_since.is_none(), "a test is not a change");
        assert!(matches!(s.handle_request(Request::TestBusKeepAwake { bus: 9 }), Response::Error { .. }));
    }

    #[test]
    fn strips_and_buses_can_be_added_renamed_and_removed() {
        let mut s = deviceless();
        ok(s.handle_request(Request::AddStrip { name: "Game".into() }));
        ok(s.handle_request(Request::AddStrip { name: String::new() }));
        ok(s.handle_request(Request::AddBus { name: "Stream".into() }));
        ok(s.handle_request(Request::AddBus { name: "Record".into() }));
        assert_eq!(s.config.strips.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Input", "Game", "Input 3"]);
        assert_eq!(s.config.buses.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), ["A1", "A2", "A3"]);
        assert_eq!(s.meters.strips.len(), 3);
        assert_eq!(s.meters.buses.len(), 3);

        ok(s.handle_request(Request::SetRoute { strip: 1, bus: 1, enabled: true }));
        ok(s.handle_request(Request::SetRoute { strip: 1, bus: 2, enabled: true }));
        ok(s.handle_request(Request::RenameStrip { strip: 1, name: "  Discord ".into() }));
        ok(s.handle_request(Request::RenameBus { bus: 2, name: "OBS".into() }));
        assert_eq!(s.config.strips[1].name, "Discord");
        assert_eq!(s.config.buses[2].name, "OBS");
        assert!(matches!(
            s.handle_request(Request::RenameStrip { strip: 0, name: " ".into() }),
            Response::Error { .. }
        ));

        // Removing bus A2 drops it from routes; the master cannot go.
        ok(s.handle_request(Request::RemoveBus { bus: 1 }));
        assert_eq!(s.config.strips[1].routes, ["A3"]);
        assert!(matches!(s.handle_request(Request::RemoveBus { bus: 0 }), Response::Error { .. }));
        ok(s.handle_request(Request::RemoveStrip { strip: 0 }));
        assert_eq!(s.config.strips[0].name, "Discord");
        assert_eq!(s.meters.strips.len(), 2);
        assert_eq!(s.meters.buses.len(), 2);
        // The next bus gets the free id again.
        ok(s.handle_request(Request::AddBus { name: String::new() }));
        assert_eq!(s.config.buses[2].id, "A2");
        assert!(matches!(s.handle_request(Request::RemoveStrip { strip: 9 }), Response::Error { .. }));
    }

    #[test]
    fn removing_a_strip_renumbers_device_slots() {
        let mut s = deviceless();
        ok(s.handle_request(Request::AddStrip { name: "B".into() }));
        ok(s.handle_request(Request::AddStrip { name: "C".into() }));
        s.reassign(false, 2, Some(DeviceSelector::DefaultInput), "C".into());
        s.reassign(false, 0, Some(DeviceSelector::DefaultInput), "Input".into());
        ok(s.handle_request(Request::RemoveStrip { strip: 1 }));
        let roles: Vec<Role> = s.slots.iter().map(|x| x.role).collect();
        assert!(roles.contains(&Role::Input { strip: 0 }));
        assert!(roles.contains(&Role::Input { strip: 1 }), "strip 2 became strip 1: {roles:?}");
        ok(s.handle_request(Request::RemoveStrip { strip: 0 }));
        let roles: Vec<Role> = s.slots.iter().map(|x| x.role).collect();
        assert_eq!(roles, [Role::Input { strip: 0 }]);
        assert_eq!(s.slots[0].label, "C");
    }

    #[test]
    fn backoff_grows_and_is_bounded() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_secs(1));
        assert_eq!(backoff(30), Duration::from_secs(10));
    }
}
