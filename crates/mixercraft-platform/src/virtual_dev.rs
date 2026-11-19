//! Virtual devices: playback and recording endpoints that MixerCraft
//! creates for other applications.
//!
//! On Linux they are PipeWire nodes (`crate::pipewire_dev`); everything
//! below describes Windows.
//!
//! On Windows each one is an emulated USB audio device ([`mixercraft_usbip`])
//! attached through the usbip-win2 client driver, which the user installs
//! (see ADR 0007). A strip whose source is `virtual:<name>` creates a
//! playback endpoint "MixerCraft <name>" that applications play into; a bus
//! whose sink is `virtual:<name>` creates a recording endpoint applications
//! record from. The endpoint exists while its stream is open.
//!
//! Windows makes a newly connected USB audio device the default device; the
//! attach worker snapshots the defaults before connecting and, for a short
//! while afterwards, undoes a switch to an endpoint that did not exist
//! before. A switch to a known endpoint is left alone: that is Windows
//! returning to a default the user chose (e.g. a virtual device made the
//! default output, coming back after an engine restart).
//!
//! Windows remembers a device by its serial number (derived from the name in
//! the id, so app assignments survive restarts) and keeps the product name
//! it first saw. The name users see follows the strip or bus instead: the
//! worker sets the endpoint's own name ("Browser (MixerCraft Input 2)"), the
//! way renaming in the Sound settings does.
//!
//! Windows must remove a device's old copy before it can detect the device
//! again. When the user shut down (which Fast Startup turns into a
//! hibernation of Windows itself), Windows dropped the devices while the
//! engine was still running, the engine reconnected them half a second
//! later, and Windows saved its state with copies it never removed: after
//! the next start every application got "device invalidated" until a real
//! restart (seen twice on hardware). So when the session ends (sign-out,
//! shutdown) the engine unplugs its devices itself, waits until Windows has
//! removed them, and connects nothing any more; it also connects nothing
//! while the system sleeps. The worker waits until an old copy is gone
//! before connecting a device and checks that Windows really detected it
//! again; otherwise it reports the device as failed with a message that
//! says what to do.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime};

use mixercraft_usbip::{AudioPort, DeviceHandle, DeviceSpec, DeviceWatch, Direction, Server, descriptors};
use mixercraft_winaudio::{DeviceState, PowerEvent, PowerWatch, SessionEvent, SessionWatch};

use crate::{DeviceSelector, FaultKind, PlatformError, StreamFault, StreamInfo};

/// Device ids of virtual devices start with this.
pub const ID_PREFIX: &str = "virtual:";
/// Endpoint product names start with this ("MixerCraft Game").
pub const PRODUCT_PREFIX: &str = "MixerCraft ";
/// How long after connecting a device the previous defaults are enforced.
const RESTORE_WINDOW: Duration = Duration::from_secs(6);
/// Longest wait for `usbip attach`.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(20);

/// The virtual device name of a selector (`virtual:<name>`), if any.
#[must_use]
pub fn virtual_name(sel: &DeviceSelector) -> Option<&str> {
    match sel {
        DeviceSelector::Device { id: Some(id), .. } => id.strip_prefix(ID_PREFIX),
        _ => None,
    }
}

/// Whether an endpoint name belongs to a MixerCraft virtual device (e.g.
/// "Speakers (MixerCraft Game)" on Windows, "MixerCraft Game" as a PipeWire
/// node); such endpoints are hidden from the device list so nobody routes
/// MixerCraft into itself.
#[must_use]
pub fn is_virtual_endpoint_name(name: &str) -> bool {
    name.contains(&format!("({PRODUCT_PREFIX}")) || name.starts_with(PRODUCT_PREFIX)
}

/// The usbip-win2 command-line client, if installed.
#[must_use]
pub fn usbip_exe() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let base = std::env::var_os("ProgramFiles").map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from);
    let exe = base.join("USBip").join("usbip.exe");
    exe.is_file().then_some(exe)
}

/// Why virtual devices are unavailable, or `None` if they can be created.
#[must_use]
pub fn unavailable_reason() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    return crate::pipewire_dev::unavailable_reason();
    #[cfg(not(target_os = "linux"))]
    if !cfg!(windows) {
        Some("virtual devices are not implemented on this platform yet")
    } else if usbip_exe().is_none() {
        Some("usbip-win2 is not installed (see ADR 0007)")
    } else {
        None
    }
}

fn info(name: &str) -> StreamInfo {
    StreamInfo {
        device_id: format!("{ID_PREFIX}{name}"),
        device_name: format!("{PRODUCT_PREFIX}{name}"),
        sample_rate: mixercraft_usbip::RATE,
        channels: mixercraft_usbip::CHANNELS as u16,
        buffer_frames: Some(mixercraft_usbip::RATE / 100),
        sample_format: "I16 (virtual USB)".into(),
    }
}

/// Stable serial number (FNV-1a of the name), so Windows recognises the
/// device again and keeps its settings. Speakers and microphones of the same
/// name must differ (they are separate USB devices).
fn serial(name: &str, direction: Direction) -> String {
    let mut h: u32 = 0x811c_9dc5;
    let prefix: &[u8] = if direction == Direction::Microphone { b"mic:" } else { b"" };
    for b in prefix.iter().chain(name.as_bytes()) {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    format!("MC{h:08X}")
}

/// Wanted endpoint names: (virtual name, is speaker) -> name to show.
type Labels = Arc<Mutex<HashMap<(String, bool), String>>>;

struct Speaker<D>(D);

impl<D: FnMut(&[f32]) + Send + 'static> AudioPort for Speaker<D> {
    fn played(&mut self, frames: &[f32]) {
        (self.0)(frames);
    }
}

struct Microphone<D>(D);

impl<D: FnMut(&mut [f32]) + Send + 'static> AudioPort for Microphone<D> {
    fn record(&mut self, out: &mut [f32]) {
        (self.0)(out);
    }
}

type OnError = Box<dyn FnMut(StreamFault) + Send>;

struct Job {
    watch: DeviceWatch,
    on_error: OnError,
}

/// The USB/IP server and the worker that connects devices to Windows.
#[cfg_attr(target_os = "linux", allow(dead_code))] // Linux uses PipeWire nodes
pub(crate) struct Virtual {
    server: Arc<Server>,
    jobs: Sender<Job>,
    labels: Labels,
    labels_changed: Arc<AtomicBool>,
    /// Sleep notifications (none if Windows refused them).
    _power: Option<PowerWatch>,
    /// Sign-out and shutdown notifications (none if unavailable).
    _session: Option<SessionWatch>,
}

impl fmt::Debug for Virtual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Virtual").field("server", &self.server).finish_non_exhaustive()
    }
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
impl Virtual {
    pub(crate) fn start(memory_file: Option<PathBuf>) -> Result<Self, String> {
        if let Some(why) = unavailable_reason() {
            return Err(why.into());
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port()));
        let server =
            Arc::new(Server::start(addr).map_err(|e| {
                format!("cannot start the USB/IP server on {addr} (another USB/IP server running?): {e}")
            })?);
        let (jobs, rx) = mpsc::channel();
        let labels: Labels = Arc::default();
        let labels_changed = Arc::new(AtomicBool::new(false));
        let (worker_labels, worker_changed) = (Arc::clone(&labels), Arc::clone(&labels_changed));
        let power: Arc<Mutex<PowerState>> = Arc::new(Mutex::new(PowerState::Awake));
        let notified = Arc::clone(&power);
        let watch = mixercraft_winaudio::watch_power(move |event| {
            *notified.lock().unwrap_or_else(PoisonError::into_inner) = match event {
                PowerEvent::Suspend => PowerState::Suspended,
                PowerEvent::Resume => PowerState::Resumed(Instant::now()),
            };
        })
        .ok();
        let session = watch_session(Arc::clone(&power), Arc::downgrade(&server));
        std::thread::Builder::new()
            .name("mixercraft-virtual-attach".into())
            .spawn(move || {
                attach_worker(&rx, &worker_labels, &worker_changed, DefaultMemory::load(memory_file), &power);
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { server, jobs, labels, labels_changed, _power: watch, _session: session })
    }

    /// Sets the name Windows shows for a virtual device (speaker: a strip's
    /// device; microphone: a bus's device).
    pub(crate) fn set_label(&self, name: &str, speaker: bool, label: &str) {
        let mut labels = self.labels.lock().unwrap_or_else(PoisonError::into_inner);
        if labels.get(&(name.to_owned(), speaker)).map(String::as_str) != Some(label) {
            labels.insert((name.to_owned(), speaker), label.to_owned());
            self.labels_changed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn info(name: &str) -> StreamInfo {
        info(name)
    }

    fn add(&self, name: &str, direction: Direction, port: Box<dyn AudioPort>, on_error: OnError) -> DeviceHandle {
        let spec =
            DeviceSpec { product: format!("{PRODUCT_PREFIX}{name}"), serial: serial(name, direction), direction };
        let handle = self.server.add(spec, port);
        let _ = self.jobs.send(Job { watch: handle.watch(), on_error });
        handle
    }

    pub(crate) fn open_speaker<D, E>(&self, name: &str, on_data: D, on_error: E) -> DeviceHandle
    where
        D: FnMut(&[f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        self.add(name, Direction::Speaker, Box::new(Speaker(on_data)), Box::new(on_error))
    }

    pub(crate) fn open_microphone<D, E>(&self, name: &str, on_data: D, on_error: E) -> DeviceHandle
    where
        D: FnMut(&mut [f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        self.add(name, Direction::Microphone, Box::new(Microphone(on_data)), Box::new(on_error))
    }
}

/// Longest wait at sign-out or shutdown for Windows to remove the devices
/// (Windows waits about 5 s for an application before ending it).
const SESSION_END_WAIT: Duration = Duration::from_secs(4);

/// When the user session ends, stops connecting devices and unplugs them,
/// so Windows does not save (Fast Startup) or leave behind copies it has
/// not finished removing.
fn watch_session(power: Arc<Mutex<PowerState>>, server: Weak<Server>) -> Option<SessionWatch> {
    let set = move |s| *power.lock().unwrap_or_else(PoisonError::into_inner) = s;
    mixercraft_winaudio::watch_session_end(move |event| match event {
        SessionEvent::Ending => set(PowerState::Ending),
        SessionEvent::Cancelled => set(PowerState::Awake),
        SessionEvent::Ended => {
            set(PowerState::Ending);
            if let Some(server) = server.upgrade() {
                let serials = server.unplug_all();
                wait_until_removed(&serials, SESSION_END_WAIT);
            }
        }
    })
    .ok()
}

/// Waits until Windows no longer has any of these devices plugged in.
fn wait_until_removed(serials: &[String], limit: Duration) {
    let until = Instant::now() + limit;
    while Instant::now() < until && serials.iter().any(|s| windows_state(s).is_some_and(|d| d.present)) {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The USB/IP server port: 3240, or `MIXERCRAFT_USBIP_PORT` (for test
/// instances running next to a normal engine).
fn port() -> u16 {
    std::env::var("MIXERCRAFT_USBIP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .filter(|p| *p >= 1024)
        .unwrap_or(mixercraft_usbip::DEFAULT_PORT)
}

/// Whether this usbip-win2 can receive in low-latency mode (`attach
/// --receive-mode low-latency`, newer 0.9.x releases).
///
/// The default ("zero-copy") made Windows' audio passes on a virtual device
/// irregular under CPU load: with a game running, Windows asked applications
/// for audio 14 times a minute more than 15 ms late (once 32 ms) and skipped
/// passes, so the game's own stream ran dry and crackled. In low-latency
/// mode it was 1 to 4 times, at most 20 ms, about as even as a real USB
/// speaker on the same machine (measured on hardware, alternating runs).
fn low_latency_supported(exe: &Path) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let mut cmd = std::process::Command::new(exe);
        cmd.args(["attach", "--help"]).stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd.output().is_ok_and(|out| {
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            help_offers_low_latency(&text)
        })
    })
}

fn help_offers_low_latency(help: &str) -> bool {
    help.contains("--receive-mode") && help.contains("low-latency")
}

/// Runs `usbip attach` for a device; returns the client's message on failure.
fn attach(busid: &str) -> Result<(), String> {
    let exe = usbip_exe().ok_or("usbip-win2 is not installed")?;
    let mut cmd = std::process::Command::new(&exe);
    // `--once`: no background reattach attempts by the driver; the engine's
    // own recovery reopens devices, with fresh bus ids.
    cmd.args(["-t", &port().to_string(), "attach", "--once", "-r", "127.0.0.1", "-b", busid]);
    if low_latency_supported(&exe) {
        cmd.args(["--receive-mode", "low-latency"]);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run usbip.exe: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < ATTACH_TIMEOUT => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                return Err("usbip attach timed out".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        Err(format!("usbip attach failed: {}", text.trim()))
    }
}

struct Watched {
    watch: DeviceWatch,
    on_error: OnError,
    /// When `usbip attach` returned; the import must show up soon after.
    since: Instant,
    /// When the attach started, on the wall clock (Windows' arrival times
    /// use it).
    attached_at: SystemTime,
    seen: bool,
    /// Windows detected the device after `attached_at`.
    renewed: bool,
}

/// A device waiting to be connected.
struct Pending {
    job: Job,
    /// When the worker first could have connected it.
    waiting_since: Option<Instant>,
}

/// Whether the worker may connect devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerState {
    Awake,
    /// The system is going to sleep (or sleeps).
    Suspended,
    /// The system woke at this moment.
    Resumed(Instant),
    /// The user session ends (sign-out, shutdown).
    Ending,
}

/// After waking up, Windows first brings back its own USB and audio
/// devices; connect nothing for this long.
const RESUME_SETTLE: Duration = Duration::from_secs(3);
/// Longest wait for Windows to finish removing a device's old copy.
const OLD_COPY_TIMEOUT: Duration = Duration::from_secs(20);
/// Clock tolerance when comparing Windows' arrival time with ours.
const ARRIVAL_SLACK: Duration = Duration::from_secs(2);

const OLD_COPY_STUCK: &str = "Windows still holds an earlier copy of this virtual device, so applications \
cannot use it (this can happen after shutting down with Fast Startup, or around sleep). Restart Windows \
(Restart, not Shut down) to fix it; MixerCraft keeps trying meanwhile.";
const NOT_RENEWED: &str = "Windows did not set up this virtual device again and still uses an earlier copy, so \
applications cannot use it (this can happen after shutting down with Fast Startup, or around sleep). \
Restart Windows (Restart, not Shut down) to fix it; MixerCraft keeps trying meanwhile.";

fn attach_allowed(state: PowerState, now: Instant) -> bool {
    match state {
        PowerState::Awake => true,
        PowerState::Suspended | PowerState::Ending => false,
        PowerState::Resumed(at) => now.saturating_duration_since(at) >= RESUME_SETTLE,
    }
}

/// What to do with a pending device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Attach,
    /// Windows is still removing the device's old copy.
    Wait,
    /// The old copy did not go away.
    Stuck,
}

/// `windows`: Windows' view of the device's serial number (`None`: never
/// seen, or unknown).
fn readiness(windows: Option<DeviceState>, waited: Duration) -> Readiness {
    match windows {
        Some(s) if s.present && waited >= OLD_COPY_TIMEOUT => Readiness::Stuck,
        Some(s) if s.present => Readiness::Wait,
        _ => Readiness::Attach,
    }
}

/// Whether Windows detected the device after `attached_at`. A device
/// Windows cannot tell us about counts as detected: nothing to hold
/// against it.
fn arrived_since(windows: Option<DeviceState>, attached_at: SystemTime) -> bool {
    match windows {
        Some(DeviceState { arrived: Some(t), .. }) => t + ARRIVAL_SLACK >= attached_at,
        _ => true,
    }
}

/// Windows' view of a virtual device's USB device (`None` off Windows).
fn windows_state(serial: &str) -> Option<DeviceState> {
    let id = format!(r"USB\VID_{:04X}&PID_{:04X}\{serial}", descriptors::VENDOR_ID, descriptors::PRODUCT_ID);
    mixercraft_winaudio::device_state(&id).ok().flatten()
}

/// A device that never imports within this time after `usbip attach` is
/// reported as failed.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often endpoint names are checked (devices appear asynchronously).
const LABEL_CHECK: Duration = Duration::from_secs(2);

/// Gives every MixerCraft endpoint its wanted name.
fn apply_labels(labels: &Labels) {
    let wanted = labels.lock().unwrap_or_else(PoisonError::into_inner).clone();
    if wanted.is_empty() {
        return;
    }
    let Ok(endpoints) = mixercraft_winaudio::endpoints() else { return };
    for e in endpoints {
        let Some(name) = e.device_name.strip_prefix(PRODUCT_PREFIX) else { continue };
        let speaker = e.flow == mixercraft_winaudio::Flow::Render;
        if let Some(label) = wanted.get(&(name.to_owned(), speaker))
            && e.description != *label
        {
            let _ = mixercraft_winaudio::set_description(&e.id, label);
        }
    }
}

/// Default-device roles the user gave to MixerCraft virtual devices.
///
/// The devices disappear while the engine is stopped, and Windows moves
/// those roles to other devices and does not move them back. This keeps
/// `(flow, role) -> device name` (e.g. "MixerCraft Input 1"), saved in a
/// small file, and gives the role back once per reappearance of the device.
/// A role the user moves away while the device is present is forgotten.
struct DefaultMemory {
    file: Option<PathBuf>,
    roles: HashMap<(Flow, Role), String>,
    /// Roles already given back since their device (re)appeared.
    restored: HashSet<(Flow, Role)>,
}

use mixercraft_winaudio::{Flow, Role};

fn flow_name(f: Flow) -> &'static str {
    if f == Flow::Render { "render" } else { "capture" }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Console => "console",
        Role::Multimedia => "multimedia",
        Role::Communications => "communications",
    }
}

impl DefaultMemory {
    fn load(file: Option<PathBuf>) -> Self {
        let mut roles = HashMap::new();
        if let Some(text) = file.as_deref().and_then(|f| std::fs::read_to_string(f).ok()) {
            roles = parse_memory(&text);
        }
        Self { file, roles, restored: HashSet::new() }
    }

    fn save(&self) {
        let Some(file) = &self.file else { return };
        let mut lines: Vec<String> =
            self.roles.iter().map(|((f, r), name)| format!("{}\t{}\t{name}", flow_name(*f), role_name(*r))).collect();
        lines.sort();
        let text = format!(
            "# Default devices given to MixerCraft virtual devices (restored after a restart).\n{}\n",
            lines.join("\n")
        );
        let _ = write_atomically(file, &text);
    }

    /// Gives remembered roles back to devices that (re)appeared, then
    /// records the current choice.
    fn sync(&mut self) {
        let (Ok(endpoints), Ok(mut current)) = (mixercraft_winaudio::endpoints(), mixercraft_winaudio::snapshot())
        else {
            return;
        };
        let ours: Vec<(&str, Flow, &str)> = endpoints
            .iter()
            .filter(|e| e.device_name.starts_with(PRODUCT_PREFIX))
            .map(|e| (e.id.as_str(), e.flow, e.device_name.as_str()))
            .collect();
        let find =
            |flow: Flow, name: &str| ours.iter().find(|(_, f, n)| *f == flow && *n == name).map(|(id, _, _)| *id);

        let mut gave_back = false;
        for ((flow, role), name) in &self.roles {
            match find(*flow, name) {
                Some(id) => {
                    if self.restored.insert((*flow, *role)) && current.get(*flow, *role) != Some(id) {
                        gave_back |= mixercraft_winaudio::set_default(id, *role).is_ok();
                    }
                }
                None => {
                    self.restored.remove(&(*flow, *role));
                }
            }
        }
        if gave_back && let Ok(now) = mixercraft_winaudio::snapshot() {
            current = now;
        }

        let mut changed = false;
        for (flow, role, id) in &current.entries {
            if let Some((_, _, name)) = ours.iter().find(|(i, _, _)| i == id) {
                if self.roles.get(&(*flow, *role)).map(String::as_str) != Some(*name) {
                    self.roles.insert((*flow, *role), (*name).to_owned());
                    self.restored.insert((*flow, *role));
                    changed = true;
                }
            } else if let Some(name) = self.roles.get(&(*flow, *role))
                && find(*flow, name).is_some()
                && self.restored.contains(&(*flow, *role))
            {
                // The device is here and was given its role back, yet the
                // user chose another device: forget.
                self.roles.remove(&(*flow, *role));
                changed = true;
            }
        }
        if changed {
            self.save();
        }
    }
}

fn parse_memory(text: &str) -> HashMap<(Flow, Role), String> {
    let mut roles = HashMap::new();
    for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
        let mut parts = line.splitn(3, '\t');
        let (Some(f), Some(r), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let flow = match f {
            "render" => Flow::Render,
            "capture" => Flow::Capture,
            _ => continue,
        };
        let role = match r {
            "console" => Role::Console,
            "multimedia" => Role::Multimedia,
            "communications" => Role::Communications,
            _ => continue,
        };
        if name.starts_with(PRODUCT_PREFIX) {
            roles.insert((flow, role), name.to_owned());
        }
    }
    roles
}

fn write_atomically(file: &Path, text: &str) -> std::io::Result<()> {
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, file)
}

/// Connects devices one at a time, keeps the user's default devices, names
/// the endpoints, and reports devices that Windows dropped.
fn attach_worker(
    rx: &Receiver<Job>,
    labels: &Labels,
    labels_changed: &AtomicBool,
    mut memory: DefaultMemory,
    power: &Mutex<PowerState>,
) {
    let mut watched: Vec<Watched> = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    let mut next_label_check = Instant::now();
    // Defaults before the current burst of attaches, and until when to
    // enforce them.
    let mut saved: Option<(mixercraft_winaudio::Defaults, Instant, HashSet<String>)> = None;
    loop {
        // Check the defaults often while a device is being connected (Windows
        // switches about a second after it appears); otherwise just watch.
        let busy = saved.is_some() || !pending.is_empty();
        let tick = if busy { Duration::from_millis(50) } else { Duration::from_millis(250) };
        match rx.recv_timeout(tick) {
            Ok(job) => pending.push(Pending { job, waiting_since: None }),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let state = *power.lock().unwrap_or_else(PoisonError::into_inner);
        if attach_allowed(state, Instant::now()) {
            for mut p in std::mem::take(&mut pending) {
                if p.job.watch.removed() {
                    continue;
                }
                let waited = p.waiting_since.get_or_insert_with(Instant::now).elapsed();
                match readiness(windows_state(p.job.watch.serial()), waited) {
                    Readiness::Wait => pending.push(p),
                    Readiness::Stuck => {
                        (p.job.on_error)(StreamFault { kind: FaultKind::DeviceLost, message: OLD_COPY_STUCK.into() });
                    }
                    Readiness::Attach => {
                        let Job { watch, mut on_error } = p.job;
                        if saved.is_none() {
                            let known = mixercraft_winaudio::known_endpoint_ids().unwrap_or_default();
                            saved = mixercraft_winaudio::snapshot().ok().map(|d| (d, Instant::now(), known));
                        }
                        let attached_at = SystemTime::now();
                        match attach(watch.busid()) {
                            Ok(()) => {
                                if let Some((_, until, _)) = saved.as_mut() {
                                    *until = Instant::now() + RESTORE_WINDOW;
                                }
                                watched.push(Watched {
                                    watch,
                                    on_error,
                                    since: Instant::now(),
                                    attached_at,
                                    seen: false,
                                    renewed: false,
                                });
                            }
                            Err(message) => on_error(StreamFault { kind: FaultKind::Other, message }),
                        }
                    }
                }
            }
        }
        if let Some((defaults, until, known)) = &saved {
            let _ = mixercraft_winaudio::restore_where(defaults, |now| !known.contains(now));
            if Instant::now() > *until {
                saved = None;
            }
        }
        if labels_changed.swap(false, Ordering::Relaxed) || Instant::now() >= next_label_check {
            apply_labels(labels);
            memory.sync();
            next_label_check = Instant::now() + LABEL_CHECK;
        }
        watched.retain_mut(|w| {
            if w.watch.removed() {
                return false;
            }
            let attached = w.watch.attached();
            if attached {
                w.seen = true;
                if !w.renewed {
                    if arrived_since(windows_state(w.watch.serial()), w.attached_at) {
                        w.renewed = true;
                    } else if w.since.elapsed() > IMPORT_TIMEOUT {
                        (w.on_error)(StreamFault { kind: FaultKind::DeviceLost, message: NOT_RENEWED.into() });
                        return false;
                    }
                }
                return true;
            }
            let message = if w.seen {
                "the virtual device was disconnected from Windows"
            } else if w.since.elapsed() > IMPORT_TIMEOUT {
                "Windows did not connect the virtual device"
            } else {
                return true;
            };
            (w.on_error)(StreamFault { kind: FaultKind::DeviceLost, message: message.into() });
            false
        });
    }
}

/// Error for a virtual selector when virtual devices are unavailable.
pub(crate) fn unavailable(name: &str, why: &str) -> PlatformError {
    PlatformError::DeviceNotFound(format!("{ID_PREFIX}{name} ({why})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn windows(present: bool, arrived_secs_ago: Option<u64>) -> Option<DeviceState> {
        let arrived = arrived_secs_ago.map(|s| SystemTime::now() - Duration::from_secs(s));
        Some(DeviceState { present, arrived, removed: None })
    }

    #[test]
    fn nothing_connects_while_the_system_sleeps() {
        let now = Instant::now();
        assert!(attach_allowed(PowerState::Awake, now));
        assert!(!attach_allowed(PowerState::Suspended, now));
        assert!(!attach_allowed(PowerState::Resumed(now), now + Duration::from_secs(1)));
        assert!(attach_allowed(PowerState::Resumed(now), now + RESUME_SETTLE));
        // Sign-out or shutdown: never again in this session.
        assert!(!attach_allowed(PowerState::Ending, now + Duration::from_secs(3600)));
    }

    #[test]
    fn waiting_for_removal_ends() {
        // Unknown devices are not plugged in: no wait.
        let t = Instant::now();
        wait_until_removed(&["MCNOSUCHDEVICE".into()], Duration::from_secs(5));
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn devices_wait_for_their_old_copy_to_go() {
        // Never seen, or removed: connect.
        assert_eq!(readiness(None, Duration::ZERO), Readiness::Attach);
        assert_eq!(readiness(windows(false, Some(60)), Duration::ZERO), Readiness::Attach);
        // Windows still holds the old copy: wait, then give up with a message.
        assert_eq!(readiness(windows(true, Some(60)), Duration::from_secs(1)), Readiness::Wait);
        assert_eq!(readiness(windows(true, Some(60)), OLD_COPY_TIMEOUT), Readiness::Stuck);
    }

    #[test]
    fn a_connected_device_must_be_detected_again() {
        let attached_at = SystemTime::now() - Duration::from_secs(5);
        // Detected after the attach (within the clock tolerance): fine.
        assert!(arrived_since(windows(true, Some(4)), attached_at));
        assert!(arrived_since(windows(true, Some(6)), attached_at));
        // Seen on hardware: still the copy from the night before.
        assert!(!arrived_since(windows(true, Some(14 * 3600)), attached_at));
        // Nothing to go by (not Windows, or no arrival time): no failure.
        assert!(arrived_since(None, attached_at));
        assert!(arrived_since(windows(true, None), attached_at));
    }

    #[test]
    fn low_latency_mode_is_detected_from_the_help_text() {
        // usbip-win2 0.9.8.1 `attach --help` (abridged).
        let help = "--once Excludes: --stop\n  Do not start automatic attach attempts if cannot connect\n\
                    --receive-mode TEXT:{zero-copy,low-latency} [zero-copy]  Excludes: --stop\n  How to receive data";
        assert!(help_offers_low_latency(help));
        assert!(!help_offers_low_latency("--once Excludes: --stop\n  Do not start automatic attach attempts"));
    }

    #[test]
    fn default_memory_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mixercraft-defaults-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("virtual-defaults.txt");
        let mut m = DefaultMemory::load(Some(file.clone()));
        assert!(m.roles.is_empty());
        m.roles.insert((Flow::Render, Role::Console), "MixerCraft Input 1".into());
        m.roles.insert((Flow::Capture, Role::Communications), "MixerCraft Chat".into());
        m.save();
        let back = DefaultMemory::load(Some(file));
        assert_eq!(back.roles, m.roles);
        // Foreign or malformed lines are ignored.
        let parsed = parse_memory("render\tconsole\tSpeakers\nbogus\ncapture\tconsole\tMixerCraft X\n");
        assert_eq!(parsed.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_and_ids() {
        let sel = DeviceSelector::Device { id: Some("virtual:Game".into()), name: None };
        assert_eq!(virtual_name(&sel), Some("Game"));
        assert_eq!(virtual_name(&DeviceSelector::DefaultInput), None);
        assert!(is_virtual_endpoint_name("Lautsprecher (MixerCraft Game)"));
        assert!(!is_virtual_endpoint_name("Speakers (Realtek)"));
        assert_eq!(serial("Game", Direction::Speaker), serial("Game", Direction::Speaker));
        assert_ne!(serial("Game", Direction::Speaker), serial("Chat", Direction::Speaker));
        assert_ne!(serial("Game", Direction::Speaker), serial("Game", Direction::Microphone));
        // Speakers keep the serial of the bare name (devices created before
        // microphones got their own serials keep their Windows identity).
        let fnv = "Input 1".bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
        assert_eq!(serial("Input 1", Direction::Speaker), format!("MC{fnv:08X}"));
        assert_eq!(info("Game").device_name, "MixerCraft Game");
    }
}
