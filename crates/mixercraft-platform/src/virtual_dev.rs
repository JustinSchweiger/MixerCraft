//! Virtual devices: playback and recording endpoints that MixerCraft
//! creates for other applications.
//!
//! On Windows each one is an emulated USB audio device ([`mixercraft_usbip`])
//! attached through the usbip-win2 client driver, which the user installs
//! (see ADR 0007). A strip whose source is `virtual:<name>` creates a
//! playback endpoint "MixerCraft <name>" that applications play into; a bus
//! whose sink is `virtual:<name>` creates a recording endpoint applications
//! record from. The endpoint exists while its stream is open.
//!
//! Windows makes a newly connected USB audio device the default device; the
//! attach worker snapshots the defaults before connecting and restores them
//! for a short while afterwards.

use std::fmt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use mixercraft_usbip::{AudioPort, DeviceHandle, DeviceSpec, DeviceWatch, Direction, Server};

use crate::{DeviceSelector, FaultKind, PlatformError, StreamFault, StreamInfo};

/// Device ids of virtual devices start with this.
pub const ID_PREFIX: &str = "virtual:";
/// Endpoint product names start with this ("MixerCraft Game").
pub const PRODUCT_PREFIX: &str = "MixerCraft ";
/// How long after connecting a device the previous defaults are enforced.
const RESTORE_WINDOW: Duration = Duration::from_secs(15);
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
/// "Speakers (MixerCraft Game)"); such endpoints are hidden from the device
/// list so nobody routes MixerCraft into itself.
#[must_use]
pub fn is_virtual_endpoint_name(name: &str) -> bool {
    name.contains(&format!("({PRODUCT_PREFIX}"))
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

/// Stable serial number for a name (FNV-1a), so Windows recognises the
/// device again and keeps its settings.
fn serial(name: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    format!("MC{h:08X}")
}

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
pub(crate) struct Virtual {
    server: Server,
    jobs: Sender<Job>,
}

impl fmt::Debug for Virtual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Virtual").field("server", &self.server).finish_non_exhaustive()
    }
}

impl Virtual {
    pub(crate) fn start() -> Result<Self, String> {
        if let Some(why) = unavailable_reason() {
            return Err(why.into());
        }
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], mixercraft_usbip::DEFAULT_PORT));
        let server = Server::start(addr)
            .map_err(|e| format!("cannot start the USB/IP server on {addr} (another USB/IP server running?): {e}"))?;
        let (jobs, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("mixercraft-virtual-attach".into())
            .spawn(move || attach_worker(&rx))
            .map_err(|e| e.to_string())?;
        Ok(Self { server, jobs })
    }

    pub(crate) fn info(name: &str) -> StreamInfo {
        info(name)
    }

    fn add(&self, name: &str, direction: Direction, port: Box<dyn AudioPort>, on_error: OnError) -> DeviceHandle {
        let spec = DeviceSpec { product: format!("{PRODUCT_PREFIX}{name}"), serial: serial(name), direction };
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

/// Runs `usbip attach` for a device; returns the client's message on failure.
fn attach(busid: &str) -> Result<(), String> {
    let exe = usbip_exe().ok_or("usbip-win2 is not installed")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["attach", "-r", "127.0.0.1", "-b", busid])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
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
    seen: bool,
}

/// A device that never imports within this time after `usbip attach` is
/// reported as failed.
const IMPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connects devices one at a time, keeps the user's default devices, and
/// reports devices that Windows dropped.
fn attach_worker(rx: &Receiver<Job>) {
    let mut watched: Vec<Watched> = Vec::new();
    // Defaults before the current burst of attaches, and until when to
    // enforce them.
    let mut saved: Option<(mixercraft_winaudio::Defaults, Instant)> = None;
    loop {
        // Check the defaults often while a device is being connected (Windows
        // switches about a second after it appears); otherwise just watch.
        let tick = if saved.is_some() { Duration::from_millis(50) } else { Duration::from_millis(250) };
        match rx.recv_timeout(tick) {
            Ok(Job { watch, mut on_error }) => {
                if watch.removed() {
                    continue;
                }
                if saved.is_none() {
                    saved = mixercraft_winaudio::snapshot().ok().map(|d| (d, Instant::now()));
                }
                match attach(watch.busid()) {
                    Ok(()) => {
                        if let Some((_, until)) = saved.as_mut() {
                            *until = Instant::now() + RESTORE_WINDOW;
                        }
                        watched.push(Watched { watch, on_error, since: Instant::now(), seen: false });
                    }
                    Err(message) => on_error(StreamFault { kind: FaultKind::Other, message }),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some((defaults, until)) = &saved {
            let _ = mixercraft_winaudio::restore(defaults);
            if Instant::now() > *until {
                saved = None;
            }
        }
        watched.retain_mut(|w| {
            if w.watch.removed() {
                return false;
            }
            let attached = w.watch.attached();
            if attached {
                w.seen = true;
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

    #[test]
    fn names_and_ids() {
        let sel = DeviceSelector::Device { id: Some("virtual:Game".into()), name: None };
        assert_eq!(virtual_name(&sel), Some("Game"));
        assert_eq!(virtual_name(&DeviceSelector::DefaultInput), None);
        assert!(is_virtual_endpoint_name("Lautsprecher (MixerCraft Game)"));
        assert!(!is_virtual_endpoint_name("Speakers (Realtek)"));
        assert_eq!(serial("Game"), serial("Game"));
        assert_ne!(serial("Game"), serial("Chat"));
        assert_eq!(info("Game").device_name, "MixerCraft Game");
    }
}
