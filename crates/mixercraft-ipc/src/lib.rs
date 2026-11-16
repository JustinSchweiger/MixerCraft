//! Versioned IPC protocol between the MixerCraft engine and its clients.
//!
//! # Wire format
//!
//! Newline-delimited JSON over a local socket (a named pipe on Windows, a
//! Unix domain socket elsewhere). Every message is one compact JSON object
//! terminated by `\n`, at most [`MAX_MESSAGE_BYTES`] long. The format is
//! deliberately simple: it is debuggable with standard tools, and control
//! traffic (a few messages per second plus ~30 Hz meter polling) is far below
//! anything where encoding cost matters.
//!
//! # Versioning
//!
//! A client must open with [`Request::Hello`]. The engine answers with its
//! own protocol version and rejects clients whose [`PROTOCOL_VERSION`]
//! differs. Within one protocol version changes are additive only: new
//! optional fields (always `#[serde(default)]`) and new message variants.
//! Unknown fields are ignored so that an older GUI keeps working with a newer
//! engine of the same protocol version.
//!
//! # Security
//!
//! The endpoint is per-user: on Unix it lives in the user's runtime directory
//! (`$XDG_RUNTIME_DIR`, mode 0700) and on Windows the pipe name contains the
//! user name. Hardening the Windows pipe with an explicit security
//! descriptor is required before the engine runs as a service (see
//! `docs/architecture/overview.md`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use interprocess::local_socket::{Listener, ListenerOptions, Name, Stream, prelude::*};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Protocol version spoken by this build.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound for one encoded message, protecting both sides from memory
/// exhaustion by a misbehaving peer.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// A request from a client to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Handshake; must be the first message on a connection.
    Hello { protocol: u32, client: String },
    /// Full engine status and diagnostics.
    Status,
    /// Current levels of all strips and buses (peak-hold since last call).
    Meters,
    /// Audio devices currently visible to the engine.
    Devices,
    /// Sets a strip fader.
    SetStripGain { strip: u16, db: f32 },
    /// Mutes or unmutes a strip.
    SetStripMute { strip: u16, mute: bool },
    /// Solos or unsolos a strip.
    SetStripSolo { strip: u16, solo: bool },
    /// Connects or disconnects a strip from a bus.
    SetRoute { strip: u16, bus: u16, enabled: bool },
    /// Sets a bus fader.
    SetBusGain { bus: u16, db: f32 },
    /// Mutes or unmutes a bus.
    SetBusMute { bus: u16, mute: bool },
    /// Sets a strip's gate, EQ and compressor.
    SetStripFx { strip: u16, fx: StripFx },
    /// Chooses the capture device feeding a strip.
    SetStripSource { strip: u16, choice: DeviceChoice },
    /// Chooses the playback device a bus feeds.
    SetBusSink { bus: u16, choice: DeviceChoice },
    /// Appends an input strip (no device, not routed).
    AddStrip { name: String },
    /// Appends an output bus (no device) with a new id such as `A3`.
    AddBus { name: String },
    /// Removes a strip; later strips move down by one.
    RemoveStrip { strip: u16 },
    /// Removes a bus (not the master bus); later buses move down by one.
    RemoveBus { bus: u16 },
    /// Renames a strip.
    RenameStrip { strip: u16, name: String },
    /// Renames a bus (its id stays).
    RenameBus { bus: u16, name: String },
    /// Sets a bus's keep-awake tone (keeps auto-standby speakers on).
    SetBusKeepAwake { bus: u16, keep_awake: KeepAwake },
    /// Stores engine-wide settings in the configuration. They take effect
    /// when the engine restarts ([`EngineStatus::restart_required`]).
    SetEngineSettings { settings: EngineSettings },
    /// Asks the engine to stop gracefully.
    Shutdown,
}

/// The engine's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Handshake answer.
    Hello { protocol: u32, engine_version: String },
    /// Answer to [`Request::Status`].
    Status(Box<EngineStatus>),
    /// Answer to [`Request::Meters`].
    Meters(MeterSnapshot),
    /// Answer to [`Request::Devices`].
    Devices { devices: Vec<DeviceSummary> },
    /// The request was applied.
    Ok,
    /// The request failed.
    Error { code: ErrorCode, message: String },
}

/// Machine-readable error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The client's protocol version is not supported.
    IncompatibleProtocol,
    /// The first message was not a handshake.
    HandshakeRequired,
    /// The request could not be decoded.
    BadRequest,
    /// An index or value is out of range.
    InvalidArgument,
    /// The engine is busy; retry later.
    Busy,
    /// An internal failure.
    Internal,
}

/// Lifecycle state of the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    /// Opening devices.
    #[default]
    Starting,
    /// All configured devices are running.
    Running,
    /// Running, but at least one device is failed or recovering.
    Degraded,
    /// Shutting down.
    Stopping,
}

/// Engine status and diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EngineStatus {
    pub engine_version: String,
    pub state: EngineState,
    pub uptime_secs: f64,
    pub backend: String,
    pub sample_rate: u32,
    pub max_block_frames: u32,
    /// What currently clocks the engine (device name or "software clock").
    pub clock_source: String,
    pub load_permille: u32,
    pub peak_load_permille: u32,
    pub overloads: u64,
    pub cycles: u64,
    pub non_finite_samples: u64,
    pub over_samples: u64,
    pub rt_deallocations: u64,
    pub skipped_cycles: u64,
    pub devices: Vec<DeviceStatus>,
    pub strips: Vec<StripStatus>,
    pub buses: Vec<BusStatus>,
    pub recovery_events: Vec<RecoveryEvent>,
    pub last_error: Option<String>,
    /// Virtual devices (`virtual:<name>` sources and sinks) can be created.
    pub virtual_devices: bool,
    /// Why virtual devices are unavailable (empty when they are).
    pub virtual_devices_note: String,
    /// Strip noise suppression works at the current engine rate (48 kHz).
    /// `None`: the engine predates noise suppression (an older engine still
    /// running after an update).
    pub noise_suppression_available: Option<bool>,
    /// Engine-wide settings as configured (they apply at the next start).
    /// `None`: the engine predates settings over IPC.
    pub settings: Option<EngineSettings>,
    /// The configured settings differ from those the engine started with.
    pub restart_required: bool,
    /// The configuration file the engine loads and saves (empty if none).
    pub config_path: String,
}

/// State of one opened device stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DeviceStatus {
    /// Display name.
    pub name: String,
    /// `"input"`, `"output"` or `"master"`.
    pub role: String,
    /// `"running"`, `"recovering"`, `"failed"` or `"missing"`.
    pub state: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Device buffer size if known (frames).
    pub buffer_frames: Option<u32>,
    pub fifo: Option<FifoStatus>,
    pub error: Option<String>,
}

/// FIFO health of a clock-decoupled device stream.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FifoStatus {
    pub fill_frames: u32,
    pub target_frames: u32,
    pub underruns: u64,
    pub overruns: u64,
    pub dropped_frames: u64,
    /// Device callbacks that contained a gap of silence (after warm-up).
    pub gap_reads: u64,
    pub drift_ppm: f64,
}

/// Mixer state of one strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StripStatus {
    pub name: String,
    pub source: String,
    pub channels: u8,
    pub gain_db: f32,
    pub mute: bool,
    pub solo: bool,
    /// Indices of the buses this strip feeds.
    pub routes: Vec<u16>,
    /// Gate, EQ and compressor settings.
    pub fx: StripFx,
    /// The configured source device.
    pub source_choice: DeviceChoice,
}

/// Which device a strip or bus uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceChoice {
    /// No device.
    #[default]
    None,
    /// The system default device (input for strips, output for buses).
    Default,
    /// A specific device, matched by id first, then by name.
    Device { id: Option<String>, name: Option<String> },
}

/// How much latency the engine trades for robustness (the configuration's
/// `latency_profile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LatencyProfile {
    /// Smallest buffers; may glitch on busy systems.
    LowLatency,
    /// Comfortably glitch-free on typical systems.
    #[default]
    Balanced,
    /// Large safety margins for heavily loaded or unusual systems.
    MaxStability,
}

/// Engine-wide settings that apply when the engine starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineSettings {
    /// Preferred engine sample rate (Hz). The master device runs at it if it
    /// supports it; otherwise the device's own rate wins.
    pub sample_rate: u32,
    pub latency_profile: LatencyProfile,
    /// Id of the bus whose device clocks the engine; `None` = the first bus
    /// with a device.
    pub master_bus: Option<String>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self { sample_rate: 48_000, latency_profile: LatencyProfile::default(), master_bus: None }
    }
}

/// Keep-awake tone of a bus: after 10 s of silence a quiet infrasonic sine
/// keeps speakers with auto-standby from switching off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeepAwake {
    /// Whether the tone is used.
    pub enabled: bool,
    /// Tone level (dBFS peak), -70 to -20.
    pub level_db: f32,
    /// Tone frequency (Hz), 5 to 40.
    pub frequency_hz: f32,
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self { enabled: false, level_db: -45.0, frequency_hz: 15.0 }
    }
}

/// Gate, EQ and compressor settings of a strip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripFx {
    /// Gate threshold in dBFS; -80 or below means off. Up to -10.
    pub gate_threshold_db: f32,
    /// Compressor amount, 0 (off) to 10.
    pub comp_amount: f32,
    /// Low shelf gain (dB, ±12).
    pub eq_low_db: f32,
    /// Mid peak gain (dB, ±12).
    pub eq_mid_db: f32,
    /// High shelf gain (dB, ±12).
    pub eq_high_db: f32,
    /// RNNoise speech noise suppression.
    pub noise_suppression: bool,
}

impl Default for StripFx {
    fn default() -> Self {
        Self {
            gate_threshold_db: -80.0,
            comp_amount: 0.0,
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            noise_suppression: false,
        }
    }
}

/// Mixer state of one bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BusStatus {
    pub id: String,
    pub name: String,
    pub sink: String,
    pub channels: u8,
    pub gain_db: f32,
    pub mute: bool,
    /// The configured output device.
    pub sink_choice: DeviceChoice,
    /// This bus clocks the engine (it cannot be removed).
    pub master: bool,
    /// Keep-awake tone settings.
    pub keep_awake: KeepAwake,
}

/// One entry in the recovery log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RecoveryEvent {
    /// Seconds since engine start.
    pub at_secs: f64,
    pub device: String,
    pub event: String,
    pub detail: String,
    /// How many identical consecutive events this entry stands for
    /// (`at_secs` is the latest). 0 from older engines means 1.
    pub count: u32,
}

/// Levels of one channel (linear amplitude).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Level {
    pub peak: f32,
    pub rms: f32,
}

/// Levels of every strip and bus, indexed like the status lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MeterSnapshot {
    pub strips: Vec<Vec<Level>>,
    pub buses: Vec<Vec<Level>>,
}

/// A device visible to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DeviceSummary {
    pub id: String,
    pub name: String,
    pub is_input: bool,
    pub is_output: bool,
    pub is_default_input: bool,
    pub is_default_output: bool,
    pub default_sample_rate: Option<u32>,
    pub default_channels: Option<u16>,
}

/// IPC failures.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// Transport failure.
    #[error("IPC I/O error: {0}")]
    Io(#[from] io::Error),
    /// The peer sent something undecodable.
    #[error("IPC protocol error: {0}")]
    Protocol(String),
    /// The peer closed the connection.
    #[error("connection closed by peer")]
    Closed,
    /// The engine reported an error.
    #[error("engine error ({code:?}): {message}")]
    Remote { code: ErrorCode, message: String },
}

/// Writes one message. Blocking.
///
/// # Errors
/// Encoding or I/O failures.
pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<(), IpcError> {
    let mut bytes = serde_json::to_vec(msg).map_err(|e| IpcError::Protocol(e.to_string()))?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::Protocol("message too large".into()));
    }
    bytes.push(b'\n');
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

/// Reads one message, reusing `buf`. Blocking. Returns `Ok(None)` on a clean
/// end of stream.
///
/// # Errors
/// Oversized or malformed messages and I/O failures.
pub fn read_message<R: BufRead, T: DeserializeOwned>(r: &mut R, buf: &mut String) -> Result<Option<T>, IpcError> {
    buf.clear();
    let n = r.by_ref().take(MAX_MESSAGE_BYTES as u64 + 1).read_line(buf)?;
    if n == 0 {
        return Ok(None);
    }
    if n > MAX_MESSAGE_BYTES || !buf.ends_with('\n') {
        return Err(IpcError::Protocol("message too large or truncated".into()));
    }
    serde_json::from_str(buf.trim_end()).map(Some).map_err(|e| IpcError::Protocol(e.to_string()))
}

/// Human-readable description of the endpoint (for logs).
#[must_use]
pub fn endpoint_description() -> String {
    endpoint_string()
}

fn endpoint_string() -> String {
    endpoint_for(&format!("mixercraft-engine-v{PROTOCOL_VERSION}"))
}

/// Per-user endpoint string for `base` (a pipe name on Windows, a socket
/// path in the user's runtime directory elsewhere).
#[cfg(windows)]
fn endpoint_for(base: &str) -> String {
    let user: String =
        std::env::var("USERNAME").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).collect();
    format!("{base}-{user}")
}

#[cfg(unix)]
fn endpoint_for(base: &str) -> String {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let user: String = std::env::var("USER").unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).collect();
    dir.join(format!("{base}-{user}.sock")).to_string_lossy().into_owned()
}

/// Binds `s`, removing a stale Unix socket file nobody answers on.
///
/// If binding fails while another process answers on the endpoint, the
/// error is reported as [`io::ErrorKind::AddrInUse`]. (On Windows a second
/// first-instance named pipe fails with "access denied", which would
/// otherwise be indistinguishable from a permissions problem.)
fn bind_endpoint(s: &str) -> io::Result<Listener> {
    #[cfg(unix)]
    {
        if std::path::Path::new(s).exists() && Stream::connect(endpoint_name(s)?).is_err() {
            let _ = std::fs::remove_file(s);
        }
    }
    ListenerOptions::new().name(endpoint_name(s)?).create_sync().map_err(|e| {
        if e.kind() != io::ErrorKind::AddrInUse && endpoint_name(s).and_then(Stream::connect).is_ok() {
            io::Error::new(io::ErrorKind::AddrInUse, format!("{s} is in use by another process"))
        } else {
            e
        }
    })
}

/// Desktop-app single-instance support: a second launch asks the first one
/// to show its window instead of starting another copy.
pub mod gui_instance {
    use super::{BufRead, BufReader, Listener, Stream, Write, bind_endpoint, endpoint_for, endpoint_name, io};
    use interprocess::local_socket::prelude::*;

    fn endpoint() -> String {
        endpoint_for("mixercraft-gui-v1")
    }

    /// Claims the single-instance endpoint. Fails with
    /// [`io::ErrorKind::AddrInUse`] if another instance holds it.
    ///
    /// # Errors
    /// I/O failures, including `AddrInUse`.
    pub fn claim() -> io::Result<Listener> {
        bind_endpoint(&endpoint())
    }

    /// Asks the running instance to show its window.
    ///
    /// # Errors
    /// If no instance answers.
    pub fn activate_existing() -> io::Result<()> {
        let s = endpoint();
        let mut stream = Stream::connect(endpoint_name(&s)?)?;
        stream.write_all(b"show\n")?;
        stream.flush()
    }

    /// Calls `on_show` (on this thread) for every activation request. Runs
    /// until the listener fails.
    pub fn serve(listener: &Listener, on_show: impl Fn()) {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let mut line = String::new();
            let mut reader = BufReader::new(conn);
            if reader.read_line(&mut line).is_ok() && line.trim() == "show" {
                on_show();
            }
        }
    }
}

fn endpoint_name(s: &str) -> io::Result<Name<'_>> {
    #[cfg(windows)]
    {
        s.to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
    #[cfg(unix)]
    {
        s.to_fs_name::<interprocess::local_socket::GenericFilePath>()
    }
}

/// Binds the engine's endpoint.
///
/// Fails with [`io::ErrorKind::AddrInUse`] if another engine is already
/// listening — this doubles as the single-instance guard.
///
/// # Errors
/// I/O failures, including `AddrInUse`.
pub fn bind() -> io::Result<Listener> {
    bind_endpoint(&endpoint_string())
}

/// A connected, handshaken client.
#[derive(Debug)]
pub struct Client {
    conn: BufReader<Stream>,
    buf: String,
    engine_version: String,
}

impl Client {
    /// Connects to the running engine and performs the handshake.
    ///
    /// # Errors
    /// If no engine is running, or the handshake fails.
    pub fn connect(client_name: &str) -> Result<Self, IpcError> {
        let s = endpoint_string();
        let stream = Stream::connect(endpoint_name(&s)?)?;
        let mut c = Self { conn: BufReader::new(stream), buf: String::new(), engine_version: String::new() };
        match c.request(&Request::Hello { protocol: PROTOCOL_VERSION, client: client_name.into() })? {
            Response::Hello { protocol, engine_version } if protocol == PROTOCOL_VERSION => {
                c.engine_version = engine_version;
                Ok(c)
            }
            other => Err(IpcError::Protocol(format!("unexpected handshake answer: {other:?}"))),
        }
    }

    /// Version string of the connected engine.
    #[must_use]
    pub fn engine_version(&self) -> &str {
        &self.engine_version
    }

    /// Sends a request and waits for the answer. Engine-side errors are
    /// returned as [`IpcError::Remote`].
    ///
    /// # Errors
    /// Transport, protocol or remote errors.
    pub fn request(&mut self, req: &Request) -> Result<Response, IpcError> {
        write_message(self.conn.get_mut(), req)?;
        match read_message(&mut self.conn, &mut self.buf)? {
            Some(Response::Error { code, message }) => Err(IpcError::Remote { code, message }),
            Some(r) => Ok(r),
            None => Err(IpcError::Closed),
        }
    }
}

/// Server-side handling of one connection: enforces the handshake, then
/// calls `handle` for each request until the peer disconnects.
///
/// # Errors
/// Transport failures. Protocol violations are answered with an error
/// response and end the connection.
pub fn serve_connection<F>(stream: Stream, engine_version: &str, mut handle: F) -> Result<(), IpcError>
where
    F: FnMut(Request) -> Response,
{
    let mut conn = BufReader::new(stream);
    let mut buf = String::new();
    let mut greeted = false;
    loop {
        let req: Request = match read_message(&mut conn, &mut buf) {
            Ok(Some(r)) => r,
            Ok(None) | Err(IpcError::Closed) => return Ok(()),
            Err(IpcError::Protocol(m)) => {
                write_message(conn.get_mut(), &Response::Error { code: ErrorCode::BadRequest, message: m })?;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let resp = match (&req, greeted) {
            (Request::Hello { protocol, .. }, _) if *protocol != PROTOCOL_VERSION => Response::Error {
                code: ErrorCode::IncompatibleProtocol,
                message: format!("engine speaks protocol {PROTOCOL_VERSION}, client {protocol}"),
            },
            (Request::Hello { .. }, _) => {
                greeted = true;
                Response::Hello { protocol: PROTOCOL_VERSION, engine_version: engine_version.into() }
            }
            (_, false) => Response::Error { code: ErrorCode::HandshakeRequired, message: "send hello first".into() },
            (_, true) => handle(req),
        };
        let fatal = matches!(
            resp,
            Response::Error { code: ErrorCode::IncompatibleProtocol | ErrorCode::HandshakeRequired, .. }
        );
        write_message(conn.get_mut(), &resp)?;
        if fatal {
            return Ok(());
        }
    }
}

/// Accepts connections forever, serving each on its own thread with
/// [`serve_connection`]. At most `max_clients` connections are served
/// concurrently; further connections are closed immediately.
pub fn run_server<H>(listener: Listener, engine_version: String, max_clients: usize, handler: H)
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    let active = Arc::new(AtomicUsize::new(0));
    let version: Arc<str> = engine_version.into();
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        if active.fetch_add(1, Ordering::AcqRel) >= max_clients {
            active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let (handler, counter, version) = (Arc::clone(&handler), Arc::clone(&active), Arc::clone(&version));
        let spawned = std::thread::Builder::new().name("mixercraft-ipc-client".into()).spawn(move || {
            let _ = serve_connection(stream, &version, |r| handler(r));
            counter.fetch_sub(1, Ordering::AcqRel);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn requests_have_stable_wire_format() {
        let json = serde_json::to_string(&Request::SetRoute { strip: 1, bus: 2, enabled: true }).unwrap();
        assert_eq!(json, r#"{"type":"set_route","strip":1,"bus":2,"enabled":true}"#);
        let json = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(json, r#"{"type":"status"}"#);
        let settings = EngineSettings {
            sample_rate: 44_100,
            latency_profile: LatencyProfile::MaxStability,
            master_bus: Some("A2".into()),
        };
        let json = serde_json::to_string(&Request::SetEngineSettings { settings }).unwrap();
        assert_eq!(
            json,
            r#"{"type":"set_engine_settings","settings":{"sample_rate":44100,"latency_profile":"max-stability","master_bus":"A2"}}"#
        );
    }

    #[test]
    fn responses_tolerate_unknown_and_missing_fields() {
        // A newer engine adds a field; an older one omits most of them.
        let r: Response =
            serde_json::from_str(r#"{"type":"status","engine_version":"9.9","future_field":42,"sample_rate":48000}"#)
                .unwrap();
        let Response::Status(s) = r else { panic!() };
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.state, EngineState::Starting);
        // An engine from before noise suppression is recognisable as such.
        assert_eq!(s.noise_suppression_available, None);
        let r: Response =
            serde_json::from_str(r#"{"type":"status","sample_rate":48000,"noise_suppression_available":true}"#)
                .unwrap();
        let Response::Status(s) = r else { panic!() };
        assert_eq!(s.noise_suppression_available, Some(true));
        // Engines before the settings fields are recognisable.
        assert_eq!(s.settings, None);
        assert!(!s.restart_required);
    }

    #[test]
    fn framing_roundtrip_and_eof() {
        let mut wire = Vec::new();
        write_message(&mut wire, &Request::Meters).unwrap();
        write_message(&mut wire, &Request::SetBusMute { bus: 3, mute: true }).unwrap();
        let mut r = Cursor::new(wire);
        let mut buf = String::new();
        assert_eq!(read_message::<_, Request>(&mut r, &mut buf).unwrap(), Some(Request::Meters));
        assert_eq!(
            read_message::<_, Request>(&mut r, &mut buf).unwrap(),
            Some(Request::SetBusMute { bus: 3, mute: true })
        );
        assert_eq!(read_message::<_, Request>(&mut r, &mut buf).unwrap(), None);
    }

    #[test]
    fn oversized_and_garbage_messages_are_rejected() {
        let big = vec![b'x'; MAX_MESSAGE_BYTES + 10];
        let mut buf = String::new();
        assert!(matches!(read_message::<_, Request>(&mut Cursor::new(big), &mut buf), Err(IpcError::Protocol(_))));
        let mut r = Cursor::new(b"{\"type\":\"nope\"}\n".to_vec());
        assert!(matches!(read_message::<_, Request>(&mut r, &mut buf), Err(IpcError::Protocol(_))));
    }
}
