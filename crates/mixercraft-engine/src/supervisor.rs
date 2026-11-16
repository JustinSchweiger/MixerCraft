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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mixercraft_config::{Config, Sink, Source};
use mixercraft_core::engine::{Command, EngineHandle, engine};
use mixercraft_core::fifo::{FifoSpec, fifo};
use mixercraft_core::fx::FxParams;
use mixercraft_core::mixer::{BusSpec, Mixer, MixerCommand, MixerLayout, MixerMeters, StripSpec};
use mixercraft_core::stats::FifoStats;
use mixercraft_ipc::{
    BusStatus, DeviceStatus, DeviceSummary, EngineState, EngineStatus, ErrorCode, FifoStatus, Level, MeterSnapshot,
    RecoveryEvent, Request, Response, StripStatus,
};
use mixercraft_platform::{AudioSystem, DeviceSelector, FaultKind, OpenStream, StreamFault, StreamInfo, StreamRequest};
use tracing::{error, info, warn};

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
    Running { _stream: Box<OpenStream>, since: Instant, direct_master: bool },
    Waiting { retry_at: Instant },
    Failed { retry_at: Instant },
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

struct DeviceEvent {
    slot: usize,
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
            .map(|b| BusSpec { channels: usize::from(b.channels), gain_db: b.gain_db, mute: b.mute })
            .collect(),
    }
}

fn fx_params(fx: &mixercraft_config::FxConfig) -> FxParams {
    FxParams {
        gate_threshold_db: fx.gate_threshold_db,
        comp_amount: fx.comp_amount,
        eq_low_db: fx.eq_low_db,
        eq_mid_db: fx.eq_mid_db,
        eq_high_db: fx.eq_high_db,
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
        let Some(slot) = self.slots.get(ev.slot) else { return };
        if slot.generation != ev.generation || !matches!(slot.state, SlotState::Running { .. }) {
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
                self.fail_slot(ev.slot, ev.fault.message);
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
        slot.generation += 1;
        let (gen_, idx) = (slot.generation, i);
        let tx = self.events_tx.clone();
        let on_error = move |fault: StreamFault| {
            let _ = tx.send(DeviceEvent { slot: idx, generation: gen_, fault });
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
                if matches!(role, Role::Master { .. }) && info.sample_rate == engine_rate {
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
        let invalid = |m: &str| Response::Error { code: ErrorCode::InvalidArgument, message: m.into() };
        let strips = self.config.strips.len();
        let buses = self.config.buses.len();
        let gain_ok = |db: f32| db.is_finite() && (-200.0..=24.0).contains(&db);
        let cmd = match req {
            Request::Hello { .. } | Request::Shutdown => return Response::Ok,
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
                }
                .sanitized();
                s.fx = mixercraft_config::FxConfig {
                    gate_threshold_db: p.gate_threshold_db,
                    comp_amount: p.comp_amount,
                    eq_low_db: p.eq_low_db,
                    eq_mid_db: p.eq_mid_db,
                    eq_high_db: p.eq_high_db,
                };
                MixerCommand::StripFx { strip, params: p }
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
        let all_running = self.slots.iter().all(|s| matches!(s.state, SlotState::Running { .. }));
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
                .map(|s| DeviceStatus {
                    name: s.info.as_ref().map_or_else(|| s.selector.to_string(), |i| i.device_name.clone()),
                    role: s.role.name().into(),
                    state: match s.state {
                        SlotState::Running { .. } => "running",
                        SlotState::Waiting { .. } if s.attempts == 0 => "starting",
                        SlotState::Waiting { .. } => "recovering",
                        SlotState::Failed { .. } => "failed",
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
                    },
                })
                .collect(),
            buses: self
                .config
                .buses
                .iter()
                .map(|b| BusStatus {
                    id: b.id.clone(),
                    name: b.name.clone(),
                    sink: sink_label(&b.sink),
                    channels: b.channels,
                    gain_db: b.gain_db,
                    mute: b.mute,
                })
                .collect(),
            recovery_events: self.recovery_log.iter().cloned().collect(),
            last_error: self.last_error.clone(),
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
        });
        c.strips[0].routes = vec!["B1".into(), "A1".into()];
        let l = layout_from_config(&c);
        assert_eq!(l.strips[0].routes, 0b11);
        assert_eq!(l.buses[1].channels, 1);
        assert!(l.buses[1].mute);
    }

    #[test]
    fn backoff_grows_and_is_bounded() {
        assert_eq!(backoff(0), Duration::from_millis(250));
        assert_eq!(backoff(2), Duration::from_secs(1));
        assert_eq!(backoff(30), Duration::from_secs(10));
    }
}
