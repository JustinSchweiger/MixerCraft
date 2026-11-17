//! Platform audio integration for MixerCraft.
//!
//! * [`capabilities`] — what this build can do on this operating system,
//!   with explicit support states (never fake a control for something the
//!   platform cannot do);
//! * [`AudioSystem`] — device discovery and native capture/playback streams.
//!
//! Physical device I/O currently goes through [`cpal`], which wraps WASAPI
//! (Windows), Core Audio (macOS) and ALSA or native PipeWire (Linux).
//! Virtual devices (selector ids `virtual:<name>`) are emulated USB audio
//! devices on Windows (see [`virtual_dev`]) and PipeWire nodes on Linux.
//!
//! Streams are not `Send` on every backend. The engine therefore creates,
//! owns and drops all streams on a single supervisor thread; only the data
//! callbacks run on the backend's real-time threads.

pub mod autostart;
pub mod capabilities;
#[cfg(target_os = "linux")]
mod pipewire_dev;
pub mod virtual_dev;

/// The virtual-device backend of this platform: PipeWire nodes on Linux,
/// emulated USB devices (usbip-win2) elsewhere (only usable on Windows).
#[cfg(target_os = "linux")]
use pipewire_dev::Virtual as VirtualSystem;
#[cfg(not(target_os = "linux"))]
use virtual_dev::Virtual as VirtualSystem;

use std::sync::OnceLock;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample, StreamConfig, SupportedBufferSize};

/// Largest number of interleaved samples converted per callback chunk.
const CONVERT_CHUNK_SAMPLES: usize = 16_384;

/// Errors from device discovery or stream creation.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// No device matched the selector.
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),
    /// The backend refused the operation.
    #[error("{context}: {source}")]
    Backend {
        context: &'static str,
        #[source]
        source: cpal::Error,
    },
    /// The device only offers sample formats we cannot convert.
    #[error("unsupported sample format {0:?}")]
    UnsupportedFormat(SampleFormat),
}

fn backend(context: &'static str) -> impl FnOnce(cpal::Error) -> PlatformError {
    move |source| PlatformError::Backend { context, source }
}

/// Runtime faults reported by a running stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFault {
    /// Category.
    pub kind: FaultKind,
    /// Backend message.
    pub message: String,
}

/// Categories of stream faults, from the recovery policy's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// The device disappeared; reopen when it returns.
    DeviceLost,
    /// The stream must be rebuilt (format change, audio server restart…).
    StreamInvalidated,
    /// The backend rerouted a default-device stream itself; informational.
    Rerouted,
    /// A backend-level glitch (xrun); informational.
    Xrun,
    /// Anything else; treat as requiring a rebuild.
    Other,
}

impl From<cpal::Error> for StreamFault {
    fn from(e: cpal::Error) -> Self {
        let kind = match e.kind() {
            cpal::ErrorKind::DeviceNotAvailable => FaultKind::DeviceLost,
            cpal::ErrorKind::StreamInvalidated => FaultKind::StreamInvalidated,
            cpal::ErrorKind::DeviceChanged => FaultKind::Rerouted,
            cpal::ErrorKind::Xrun => FaultKind::Xrun,
            _ => FaultKind::Other,
        };
        Self { kind, message: e.to_string() }
    }
}

/// Which device to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    /// The system default capture device (followed by the backend where
    /// supported).
    DefaultInput,
    /// The system default playback device.
    DefaultOutput,
    /// A specific device: matched by id first, then by display name.
    Device { id: Option<String>, name: Option<String> },
}

impl std::fmt::Display for DeviceSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefaultInput => f.write_str("default input"),
            Self::DefaultOutput => f.write_str("default output"),
            Self::Device { id, name } => match (name, id) {
                (Some(n), _) => f.write_str(n),
                (None, Some(i)) => f.write_str(i),
                (None, None) => f.write_str("<unspecified device>"),
            },
        }
    }
}

/// A discovered device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable backend identifier (`host:id`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Can capture.
    pub is_input: bool,
    /// Can play back.
    pub is_output: bool,
    /// Is the default capture device.
    pub is_default_input: bool,
    /// Is the default playback device.
    pub is_default_output: bool,
    /// Default sample rate (capture config if input-only).
    pub default_sample_rate: Option<u32>,
    /// Default channel count.
    pub default_channels: Option<u16>,
}

/// Desired stream parameters; `None` means "device default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamRequest {
    /// Sample rate.
    pub sample_rate: Option<u32>,
    /// Device buffer size in frames.
    pub buffer_frames: Option<u32>,
}

/// Parameters a stream was actually opened with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Device id.
    pub device_id: String,
    /// Device display name.
    pub device_name: String,
    /// Actual sample rate.
    pub sample_rate: u32,
    /// Interleaved channel count of the callback buffers.
    pub channels: u16,
    /// Device buffer size if reported.
    pub buffer_frames: Option<u32>,
    /// Native sample format (converted to/from `f32`).
    pub sample_format: String,
}

/// A running stream. Dropping it stops the stream (a virtual device is
/// unplugged).
pub struct OpenStream {
    stream: StreamInner,
    /// What was negotiated.
    pub info: StreamInfo,
}

enum StreamInner {
    Cpal(cpal::Stream),
    /// Held for its `Drop`, which unplugs (Windows) or removes (Linux) the
    /// device.
    Virtual {
        _device: Box<dyn std::any::Any>,
    },
}

impl std::fmt::Debug for OpenStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenStream").field("info", &self.info).finish_non_exhaustive()
    }
}

impl OpenStream {
    /// Pauses the stream where the backend supports it.
    ///
    /// # Errors
    /// Backend failures.
    pub fn pause(&self) -> Result<(), PlatformError> {
        match &self.stream {
            StreamInner::Cpal(s) => s.pause().map_err(backend("pause stream")),
            StreamInner::Virtual { .. } => Ok(()),
        }
    }
}

/// Entry point for device discovery and stream creation.
pub struct AudioSystem {
    host: cpal::Host,
    /// Started on first use of a virtual device.
    virtual_devices: OnceLock<VirtualSystem>,
    /// Why starting virtual devices failed last time (retried on next use).
    virtual_error: std::sync::Mutex<Option<String>>,
    /// Where the default-device roles of virtual devices are remembered.
    virtual_memory_file: std::sync::Mutex<Option<std::path::PathBuf>>,
}

impl std::fmt::Debug for AudioSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSystem").field("backend", &self.backend_name()).finish()
    }
}

impl AudioSystem {
    /// Selects the preferred backend for this platform.
    #[must_use]
    pub fn new() -> Self {
        #[cfg(all(target_os = "linux", feature = "pipewire"))]
        if let Ok(host) = cpal::host_from_id(cpal::HostId::PipeWire) {
            return Self {
                host,
                virtual_devices: OnceLock::new(),
                virtual_error: std::sync::Mutex::default(),
                virtual_memory_file: std::sync::Mutex::default(),
            };
        }
        Self {
            host: cpal::default_host(),
            virtual_devices: OnceLock::new(),
            virtual_error: std::sync::Mutex::default(),
            virtual_memory_file: std::sync::Mutex::default(),
        }
    }

    /// Where to remember which Windows default-device roles the user gave to
    /// MixerCraft's virtual devices (restored after an engine restart). Set
    /// it before the first virtual device opens.
    pub fn set_virtual_memory_file(&self, file: std::path::PathBuf) {
        *self.virtual_memory_file.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
    }

    /// Whether virtual devices can be created; `Err` explains why not.
    ///
    /// # Errors
    /// The reason virtual devices are unavailable.
    pub fn virtual_devices_available(&self) -> Result<(), String> {
        if self.virtual_devices.get().is_some() {
            return Ok(());
        }
        if let Some(why) = virtual_dev::unavailable_reason() {
            return Err(why.into());
        }
        match self.virtual_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Starts virtual devices on first use. A failure (e.g. the USB/IP port
    /// is taken) is retried on the next use instead of being remembered.
    fn virtual_system(&self, name: &str) -> Result<&VirtualSystem, PlatformError> {
        if let Some(v) = self.virtual_devices.get() {
            return Ok(v);
        }
        let mut error = self.virtual_error.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let memory = self.virtual_memory_file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        match VirtualSystem::start(memory) {
            Ok(v) => {
                *error = None;
                Ok(self.virtual_devices.get_or_init(|| v))
            }
            Err(why) => {
                let e = virtual_dev::unavailable(name, &why);
                *error = Some(why);
                Err(e)
            }
        }
    }

    /// Sets the name Windows shows for the virtual device of a strip
    /// (`speaker`) or bus. Does nothing if virtual devices are not running.
    pub fn set_virtual_label(&self, sel: &DeviceSelector, speaker: bool, label: &str) {
        if let (Some(name), Some(v)) = (virtual_dev::virtual_name(sel), self.virtual_devices.get()) {
            v.set_label(name, speaker, label);
        }
    }

    /// Name of the active backend (e.g. "WASAPI").
    #[must_use]
    pub fn backend_name(&self) -> String {
        self.host.id().name().to_string()
    }

    /// Lists all devices.
    ///
    /// # Errors
    /// Backend failures.
    pub fn devices(&self) -> Result<Vec<DeviceInfo>, PlatformError> {
        let default_in = self.host.default_input_device().and_then(|d| d.id().ok());
        let default_out = self.host.default_output_device().and_then(|d| d.id().ok());
        let mut out = Vec::new();
        for d in self.host.devices().map_err(backend("enumerate devices"))? {
            let Ok(id) = d.id() else { continue };
            // MixerCraft's own virtual endpoints: routing into them from here
            // would loop MixerCraft into itself.
            if virtual_dev::is_virtual_endpoint_name(&device_name(&d)) {
                continue;
            }
            let is_input = d.supports_input();
            let is_output = d.supports_output();
            let cfg = if is_output { d.default_output_config() } else { d.default_input_config() }.ok();
            out.push(DeviceInfo {
                name: device_name(&d),
                is_default_input: default_in.as_ref() == Some(&id),
                is_default_output: default_out.as_ref() == Some(&id),
                id: id.to_string(),
                is_input,
                is_output,
                default_sample_rate: cfg.as_ref().map(cpal::SupportedStreamConfig::sample_rate),
                default_channels: cfg.as_ref().map(cpal::SupportedStreamConfig::channels),
            });
        }
        Ok(out)
    }

    fn resolve(&self, sel: &DeviceSelector, input: bool) -> Result<cpal::Device, PlatformError> {
        let not_found = || PlatformError::DeviceNotFound(sel.to_string());
        match sel {
            DeviceSelector::DefaultInput => self.host.default_input_device().ok_or_else(not_found),
            DeviceSelector::DefaultOutput => self.host.default_output_device().ok_or_else(not_found),
            DeviceSelector::Device { id, name } => {
                let candidates: Vec<cpal::Device> = self
                    .host
                    .devices()
                    .map_err(backend("enumerate devices"))?
                    .filter(|d| if input { d.supports_input() } else { d.supports_output() })
                    .collect();
                if let Some(id) = id
                    && let Some(d) = candidates.iter().find(|d| d.id().is_ok_and(|i| i.to_string() == *id))
                {
                    return Ok(d.clone());
                }
                if let Some(name) = name
                    && let Some(d) = candidates.iter().find(|d| device_name(d) == *name)
                {
                    return Ok(d.clone());
                }
                Err(not_found())
            }
        }
    }

    fn prepare_input(
        &self,
        sel: &DeviceSelector,
        req: StreamRequest,
    ) -> Result<(cpal::Device, StreamConfig, SampleFormat), PlatformError> {
        let device = self.resolve(sel, true)?;
        let default = device.default_input_config().map_err(backend("query input config"))?;
        let ranges: Vec<_> = device.supported_input_configs().map_err(backend("query input configs"))?.collect();
        let (config, format) = negotiate(&default, &ranges, req);
        Ok((device, config, format))
    }

    /// Resolves a capture device and reports the parameters
    /// [`Self::open_input`] would negotiate, without opening a stream.
    ///
    /// # Errors
    /// Device lookup failures.
    pub fn probe_input(&self, sel: &DeviceSelector, req: StreamRequest) -> Result<StreamInfo, PlatformError> {
        if let Some(name) = virtual_dev::virtual_name(sel) {
            self.virtual_system(name)?;
            return Ok(VirtualSystem::info(name));
        }
        let (device, config, format) = self.prepare_input(sel, req)?;
        Ok(stream_info(&device, &config, format))
    }

    /// Opens and starts a capture stream. `on_data` receives interleaved
    /// `f32` frames on the backend's real-time thread and must be real-time
    /// safe.
    ///
    /// # Errors
    /// Device lookup or stream creation failures.
    pub fn open_input<D, E>(
        &self,
        sel: &DeviceSelector,
        req: StreamRequest,
        mut on_data: D,
        on_error: E,
    ) -> Result<OpenStream, PlatformError>
    where
        D: FnMut(&[f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        if let Some(name) = virtual_dev::virtual_name(sel) {
            let handle = self.virtual_system(name)?.open_speaker(name, on_data, on_error);
            return Ok(OpenStream {
                stream: StreamInner::Virtual { _device: virtual_handle(name, handle)? },
                info: VirtualSystem::info(name),
            });
        }
        let (device, config, format) = self.prepare_input(sel, req)?;
        let info = stream_info(&device, &config, format);
        let mut on_error = on_error;
        let err = move |e: cpal::Error| on_error(StreamFault::from(e));
        let mut scratch = vec![0.0f32; CONVERT_CHUNK_SAMPLES];
        let stream = match format {
            SampleFormat::F32 => device.build_input_stream::<f32, _, _>(config, move |d, _| on_data(d), err, None),
            SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
                config,
                move |d, _| convert_in(d, &mut scratch, &mut on_data),
                err,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream::<i32, _, _>(
                config,
                move |d, _| convert_in(d, &mut scratch, &mut on_data),
                err,
                None,
            ),
            other => return Err(PlatformError::UnsupportedFormat(other)),
        }
        .map_err(backend("build input stream"))?;
        stream.play().map_err(backend("start input stream"))?;
        Ok(OpenStream { stream: StreamInner::Cpal(stream), info })
    }

    fn prepare_output(
        &self,
        sel: &DeviceSelector,
        req: StreamRequest,
    ) -> Result<(cpal::Device, StreamConfig, SampleFormat), PlatformError> {
        let device = self.resolve(sel, false)?;
        let default = device.default_output_config().map_err(backend("query output config"))?;
        let ranges: Vec<_> = device.supported_output_configs().map_err(backend("query output configs"))?.collect();
        let (config, format) = negotiate(&default, &ranges, req);
        Ok((device, config, format))
    }

    /// Resolves a playback device and reports the parameters
    /// [`Self::open_output`] would negotiate, without opening a stream.
    ///
    /// # Errors
    /// Device lookup failures.
    pub fn probe_output(&self, sel: &DeviceSelector, req: StreamRequest) -> Result<StreamInfo, PlatformError> {
        if let Some(name) = virtual_dev::virtual_name(sel) {
            self.virtual_system(name)?;
            return Ok(VirtualSystem::info(name));
        }
        let (device, config, format) = self.prepare_output(sel, req)?;
        Ok(stream_info(&device, &config, format))
    }

    /// Opens and starts a playback stream. `on_data` must fill the
    /// interleaved `f32` buffer on the backend's real-time thread and must be
    /// real-time safe.
    ///
    /// # Errors
    /// Device lookup or stream creation failures.
    pub fn open_output<D, E>(
        &self,
        sel: &DeviceSelector,
        req: StreamRequest,
        mut on_data: D,
        on_error: E,
    ) -> Result<OpenStream, PlatformError>
    where
        D: FnMut(&mut [f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        if let Some(name) = virtual_dev::virtual_name(sel) {
            let handle = self.virtual_system(name)?.open_microphone(name, on_data, on_error);
            return Ok(OpenStream {
                stream: StreamInner::Virtual { _device: virtual_handle(name, handle)? },
                info: VirtualSystem::info(name),
            });
        }
        let (device, config, format) = self.prepare_output(sel, req)?;
        let info = stream_info(&device, &config, format);
        let mut on_error = on_error;
        let err = move |e: cpal::Error| on_error(StreamFault::from(e));
        let mut scratch = vec![0.0f32; CONVERT_CHUNK_SAMPLES];
        let stream = match format {
            SampleFormat::F32 => device.build_output_stream::<f32, _, _>(config, move |d, _| on_data(d), err, None),
            SampleFormat::I16 => device.build_output_stream::<i16, _, _>(
                config,
                move |d, _| convert_out(d, &mut scratch, &mut on_data),
                err,
                None,
            ),
            SampleFormat::I32 => device.build_output_stream::<i32, _, _>(
                config,
                move |d, _| convert_out(d, &mut scratch, &mut on_data),
                err,
                None,
            ),
            other => return Err(PlatformError::UnsupportedFormat(other)),
        }
        .map_err(backend("build output stream"))?;
        stream.play().map_err(backend("start output stream"))?;
        Ok(OpenStream { stream: StreamInner::Cpal(stream), info })
    }
}

impl Default for AudioSystem {
    fn default() -> Self {
        Self::new()
    }
}

/// Boxes the handle of an opened virtual device (PipeWire can fail to
/// create the node; a USB/IP device always opens and reports later).
#[cfg(target_os = "linux")]
fn virtual_handle(name: &str, handle: Result<pipewire_dev::Handle, String>) -> Result<Box<dyn std::any::Any>, PlatformError> {
    match handle {
        Ok(h) => Ok(Box::new(h)),
        Err(why) => Err(virtual_dev::unavailable(name, &why)),
    }
}
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)] // same signature as on Linux
fn virtual_handle(_name: &str, handle: mixercraft_usbip::DeviceHandle) -> Result<Box<dyn std::any::Any>, PlatformError> {
    Ok(Box::new(handle))
}

fn device_name(d: &cpal::Device) -> String {
    d.description().map(|desc| desc.name().to_string()).unwrap_or_else(|_| d.to_string())
}

fn stream_info(device: &cpal::Device, config: &StreamConfig, format: SampleFormat) -> StreamInfo {
    StreamInfo {
        device_id: device.id().map(|i| i.to_string()).unwrap_or_default(),
        device_name: device_name(device),
        sample_rate: config.sample_rate,
        channels: config.channels,
        buffer_frames: match config.buffer_size {
            BufferSize::Fixed(n) => Some(n),
            BufferSize::Default => None,
        },
        sample_format: format!("{format:?}"),
    }
}

/// Picks rate, format and buffer size: the device default channel layout,
/// the requested rate if the device supports it (else its default), `f32`
/// when available, and the requested buffer size if within range.
fn negotiate(
    default: &cpal::SupportedStreamConfig,
    ranges: &[cpal::SupportedStreamConfigRange],
    req: StreamRequest,
) -> (StreamConfig, SampleFormat) {
    let channels = default.channels();
    let usable = |r: &&cpal::SupportedStreamConfigRange| {
        r.channels() == channels
            && matches!(r.sample_format(), SampleFormat::F32 | SampleFormat::I16 | SampleFormat::I32)
    };
    let rate = req
        .sample_rate
        .filter(|&want| {
            ranges.iter().filter(usable).any(|r| (r.min_sample_rate()..=r.max_sample_rate()).contains(&want))
        })
        .unwrap_or_else(|| default.sample_rate());
    let rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        _ => 2,
    };
    let chosen = ranges
        .iter()
        .filter(usable)
        .filter(|r| (r.min_sample_rate()..=r.max_sample_rate()).contains(&rate))
        .min_by_key(|r| rank(r.sample_format()));
    let format = chosen.map_or(default.sample_format(), cpal::SupportedStreamConfigRange::sample_format);
    let buffer_range = chosen.map_or(*default.buffer_size(), |r| *r.buffer_size());
    let buffer_size = match (req.buffer_frames, buffer_range) {
        (Some(n), SupportedBufferSize::Range { min, max }) if (min..=max).contains(&n) => BufferSize::Fixed(n),
        _ => BufferSize::Default,
    };
    (StreamConfig { channels, sample_rate: rate, buffer_size }, format)
}

fn convert_in<T, D>(data: &[T], scratch: &mut [f32], on_data: &mut D)
where
    T: SizedSample,
    f32: FromSample<T>,
    D: FnMut(&[f32]),
{
    for chunk in data.chunks(scratch.len()) {
        let buf = &mut scratch[..chunk.len()];
        for (o, s) in buf.iter_mut().zip(chunk) {
            *o = s.to_sample::<f32>();
        }
        on_data(buf);
    }
}

fn convert_out<T, D>(data: &mut [T], scratch: &mut [f32], on_data: &mut D)
where
    T: SizedSample + FromSample<f32>,
    D: FnMut(&mut [f32]),
{
    let n = scratch.len();
    for chunk in data.chunks_mut(n) {
        let buf = &mut scratch[..chunk.len()];
        on_data(buf);
        for (o, s) in chunk.iter_mut().zip(buf.iter()) {
            *o = s.clamp(-1.0, 1.0).to_sample::<T>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_conversion_roundtrip() {
        let ints: Vec<i16> = vec![i16::MIN, -16384, 0, 16384, i16::MAX];
        let mut got = Vec::new();
        let mut scratch = vec![0.0; 2];
        convert_in(&ints, &mut scratch, &mut |b: &[f32]| got.extend_from_slice(b));
        assert_eq!(got.len(), 5);
        assert_eq!(got[0], -1.0);
        assert_eq!(got[2], 0.0);
        let mut out = vec![0i16; 5];
        let mut i = 0;
        convert_out(&mut out, &mut scratch, &mut |b: &mut [f32]| {
            for s in b {
                *s = got[i] * 4.0; // over full scale must clamp, not wrap
                i += 1;
            }
        });
        assert_eq!(out[0], i16::MIN);
        assert_eq!(out[4], i16::MAX);
    }

    /// Enumerating devices must never panic, even on headless CI machines
    /// without audio hardware (it may legitimately return an error or an
    /// empty list there).
    #[test]
    fn enumeration_does_not_panic() {
        let sys = AudioSystem::new();
        assert!(!sys.backend_name().is_empty());
        let _ = sys.devices();
    }
}
