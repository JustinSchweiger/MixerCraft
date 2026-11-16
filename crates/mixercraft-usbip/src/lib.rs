//! Virtual audio devices as emulated USB Audio Class 1 devices served over
//! USB/IP.
//!
//! A USB/IP client driver (on Windows the Microsoft-signed usbip-win2)
//! attaches a device from this server as if it were plugged in; the
//! operating system's inbox USB audio driver then creates an ordinary
//! playback or recording endpoint. MixerCraft needs no kernel code of its
//! own. See `docs/adr/0007-windows-driver-signing.md`.
//!
//! Each device carries one direction:
//! * [`Direction::Speaker`]: applications play into it; the audio reaches
//!   [`AudioPort::played`].
//! * [`Direction::Microphone`]: applications record from it; the audio comes
//!   from [`AudioPort::record`].
//!
//! The server is the clock of its devices. While the host streams, audio
//! moves with the host's transfers, which are completed in real time; while
//! it does not (device not attached yet, or idle), a ticker keeps calling
//! the port with silence / discarded audio at the same rate, so the engine
//! sees a continuously running device.
//!
//! The format is fixed: 48 kHz, 16-bit, stereo, 1 ms isochronous packets.
//! These threads are not the engine's audio thread; they may allocate.

pub mod descriptors;
mod server;

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Sample rate of every virtual device.
pub const RATE: u32 = 48_000;
/// Channels of every virtual device.
pub const CHANNELS: usize = 2;
/// Frames per 1 ms packet.
pub const FRAMES_PER_MS: usize = (RATE / 1000) as usize;
/// The standard USB/IP port.
pub const DEFAULT_PORT: u16 = 3240;

/// Length of the chunks the idle ticker delivers (small, so a handover
/// from the host does not leave a long hole).
const IDLE_CHUNK: Duration = Duration::from_millis(2);
/// Speakers: while the host streams, the ticker takes over when no transfer
/// completed for this long (transfers complete every 10 ms plus scheduling
/// jitter), because a hole in played audio is an underrun downstream, while
/// a short overlap is harmless.
const IDLE_AFTER: Duration = Duration::from_millis(13);
/// Microphones are the other way round: reading twice (ticker and host)
/// drains the source, a short pause is harmless. The ticker stops as soon as
/// the host submits and only takes over after this long without transfers.
const MIC_IDLE_AFTER: Duration = Duration::from_millis(100);
// Either way an explicit stop (interface back to alternate setting 0) hands
// over at once.

/// Which way audio flows through a virtual device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A playback endpoint: applications play into it.
    Speaker,
    /// A recording endpoint: applications record from it.
    Microphone,
}

/// Describes a virtual device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    /// Product name; Windows shows it in parentheses after the endpoint
    /// type, e.g. "Speakers (MixerCraft Game)".
    pub product: String,
    /// Serial number; keep it stable so Windows recognises the device.
    pub serial: String,
    /// Speaker or microphone.
    pub direction: Direction,
}

/// Audio side of a virtual device. Called on server threads, never
/// concurrently, always with interleaved stereo `f32` at [`RATE`].
pub trait AudioPort: Send + 'static {
    /// Speaker devices: audio the host played (silence while idle).
    fn played(&mut self, _frames: &[f32]) {}
    /// Microphone devices: fill `out` with audio for the host.
    fn record(&mut self, out: &mut [f32]) {
        out.fill(0.0);
    }
}

pub(crate) struct Device {
    pub(crate) busid: String,
    pub(crate) spec: DeviceSpec,
    pub(crate) port: Mutex<Box<dyn AudioPort>>,
    /// End of the audio the host moved, in ns since the server epoch.
    pub(crate) host_until: AtomicU64,
    /// End of the audio the idle ticker delivered, in ns since the epoch.
    idle_until: AtomicU64,
    /// The import connection, shut down when the device is removed.
    pub(crate) conn: Mutex<Option<TcpStream>>,
    pub(crate) removed: AtomicBool,
    pub(crate) streaming: AtomicBool,
    /// The host has started streaming and not stopped (alternate setting).
    pub(crate) host_active: AtomicBool,
    /// When the host last started streaming, in ns since the epoch.
    pub(crate) host_since: AtomicU64,
}

impl Device {
    /// Runs `f` with the audio port.
    pub(crate) fn with_port<R>(&self, f: impl FnOnce(&mut dyn AudioPort) -> R) -> R {
        let mut port = self.port.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        f(port.as_mut())
    }
}

pub(crate) struct Inner {
    pub(crate) epoch: Instant,
    pub(crate) devices: Mutex<Vec<Arc<Device>>>,
    next_device: AtomicU32,
    pub(crate) stop: AtomicBool,
}

impl Inner {
    pub(crate) fn nanos(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.epoch).as_nanos() as u64
    }

    pub(crate) fn find(&self, busid: &str) -> Option<Arc<Device>> {
        self.devices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|d| d.busid == busid && !d.removed.load(Ordering::Relaxed))
            .cloned()
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<Device>> {
        self.devices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// A running USB/IP server. Dropping it stops the server and removes all
/// devices.
pub struct Server {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server").field("addr", &self.addr).finish_non_exhaustive()
    }
}

impl Server {
    /// Starts a server on `addr` (use port 0 for an ephemeral port).
    ///
    /// # Errors
    /// If the address cannot be bound (e.g. another USB/IP server runs).
    pub fn start(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            epoch: Instant::now(),
            devices: Mutex::new(Vec::new()),
            next_device: AtomicU32::new(1),
            stop: AtomicBool::new(false),
        });
        let accept_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("mixercraft-usbip-accept".into())
            .spawn(move || server::accept_loop(&listener, &accept_inner))?;
        let tick_inner = Arc::clone(&inner);
        std::thread::Builder::new().name("mixercraft-usbip-idle".into()).spawn(move || idle_loop(&tick_inner))?;
        Ok(Self { inner, addr })
    }

    /// The bound address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Adds a device. It is offered to clients until the handle is dropped.
    pub fn add(&self, spec: DeviceSpec, port: Box<dyn AudioPort>) -> DeviceHandle {
        let n = self.inner.next_device.fetch_add(1, Ordering::Relaxed);
        let now = self.inner.nanos(Instant::now());
        let device = Arc::new(Device {
            busid: format!("1-{n}"),
            spec,
            port: Mutex::new(port),
            host_until: AtomicU64::new(0),
            idle_until: AtomicU64::new(now),
            conn: Mutex::new(None),
            removed: AtomicBool::new(false),
            streaming: AtomicBool::new(false),
            host_active: AtomicBool::new(false),
            host_since: AtomicU64::new(0),
        });
        self.inner.devices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Arc::clone(&device));
        DeviceHandle { device, inner: Arc::clone(&self.inner) }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        for d in self.inner.snapshot() {
            remove(&self.inner, &d);
        }
        // Unblock the accept loop.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

fn remove(inner: &Inner, device: &Arc<Device>) {
    device.removed.store(true, Ordering::Relaxed);
    if let Some(conn) = device.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
        // The client sees the device unplugged.
        let _ = conn.shutdown(std::net::Shutdown::Both);
    }
    inner.devices.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|d| !Arc::ptr_eq(d, device));
}

/// A device offered by a [`Server`]. Dropping it unplugs the device.
pub struct DeviceHandle {
    device: Arc<Device>,
    inner: Arc<Inner>,
}

impl fmt::Debug for DeviceHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceHandle").field("busid", &self.device.busid).field("spec", &self.device.spec).finish()
    }
}

impl DeviceHandle {
    /// The USB/IP bus id clients attach (e.g. `1-1`).
    #[must_use]
    pub fn busid(&self) -> &str {
        &self.device.busid
    }

    /// Whether a client has imported the device.
    #[must_use]
    pub fn attached(&self) -> bool {
        self.device.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
    }

    /// Whether the host currently streams audio (rather than the idle
    /// ticker).
    #[must_use]
    pub fn host_streaming(&self) -> bool {
        self.device.streaming.load(Ordering::Relaxed)
    }

    /// A view of the device's state that does not keep it plugged in.
    #[must_use]
    pub fn watch(&self) -> DeviceWatch {
        DeviceWatch(Arc::clone(&self.device))
    }
}

/// Observes a device without owning it (see [`DeviceHandle::watch`]).
pub struct DeviceWatch(Arc<Device>);

impl fmt::Debug for DeviceWatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceWatch").field("busid", &self.0.busid).finish()
    }
}

impl DeviceWatch {
    /// The USB/IP bus id.
    #[must_use]
    pub fn busid(&self) -> &str {
        &self.0.busid
    }

    /// Whether a client has imported the device.
    #[must_use]
    pub fn attached(&self) -> bool {
        self.0.conn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
    }

    /// Whether the device was removed (its handle dropped).
    #[must_use]
    pub fn removed(&self) -> bool {
        self.0.removed.load(Ordering::Relaxed)
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        remove(&self.inner, &self.device);
    }
}

/// Keeps every device's audio moving at [`RATE`] while the host does not.
fn idle_loop(inner: &Inner) {
    let chunk = IDLE_CHUNK.as_nanos() as u64;
    let speaker_after = IDLE_AFTER.as_nanos() as u64;
    let mic_after = MIC_IDLE_AFTER.as_nanos() as u64;
    let mut buf = vec![0.0f32; FRAMES_PER_MS * CHANNELS * IDLE_CHUNK.as_millis() as usize];
    while !inner.stop.load(Ordering::Relaxed) {
        let now = inner.nanos(Instant::now());
        for d in inner.snapshot() {
            let host = d.host_until.load(Ordering::Relaxed);
            let active = d.host_active.load(Ordering::Relaxed);
            let streaming = match d.spec.direction {
                Direction::Speaker => active && host + speaker_after > now,
                Direction::Microphone => active && host.max(d.host_since.load(Ordering::Relaxed)) + mic_after > now,
            };
            d.streaming.store(streaming, Ordering::Relaxed);
            if streaming {
                d.idle_until.store(host, Ordering::Relaxed);
                continue;
            }
            // Continue from wherever the host (or the last idle chunk) left
            // off, so the audio timeline has no gaps.
            let mut until = d.idle_until.load(Ordering::Relaxed).max(host);
            while until + chunk <= now {
                d.with_port(|p| match d.spec.direction {
                    Direction::Speaker => {
                        buf.fill(0.0);
                        p.played(&buf);
                    }
                    Direction::Microphone => p.record(&mut buf),
                });
                until += chunk;
            }
            d.idle_until.store(until, Ordering::Relaxed);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests;
