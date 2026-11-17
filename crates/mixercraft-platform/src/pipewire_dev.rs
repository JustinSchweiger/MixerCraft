//! Linux virtual devices: PipeWire nodes that belong to the engine.
//!
//! Each virtual device is one native PipeWire stream whose `media.class`
//! makes it look like a device to other applications, the way
//! `pw-loopback` creates virtual sinks and sources:
//!
//! * a strip with source `virtual:<name>` gets an **`Audio/Sink`** node
//!   "MixerCraft <name>" (`node.name = mixercraft.sink.<name>`) that
//!   applications play into; the engine reads what they play;
//! * a bus with sink `virtual:<name>` gets an **`Audio/Source`** node
//!   (`mixercraft.source.<name>`) that applications record from; the engine
//!   writes the bus into it.
//!
//! The node exists while its stream is open and disappears with it (also
//! when the engine crashes: PipeWire removes a client's nodes when its
//! connection closes), so nothing is left behind and no configuration of the
//! user or the distribution is touched. WirePlumber remembers which
//! application used which node by `node.name`, so routing set in a desktop
//! mixer (pavucontrol, GNOME/KDE settings) comes back when the engine
//! restarts.
//!
//! Format: 32-bit float stereo at 48 kHz; PipeWire converts for
//! applications. Each stream runs its own PipeWire main loop on its own
//! thread (like cpal's PipeWire host); the process callback runs there and
//! is real-time safe (no allocation, no locks, no I/O). The streams set
//! `node.always-process`, so PipeWire keeps scheduling them while no
//! application is connected and the engine never sees a stalled device.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::{ParamType, format_utils};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{StreamFlags, StreamRc, StreamState};

use crate::virtual_dev::{ID_PREFIX, PRODUCT_PREFIX};
use crate::{FaultKind, StreamFault, StreamInfo};

/// Sample rate of every virtual device.
pub(crate) const RATE: u32 = 48_000;
/// Channels of every virtual device (front left, front right).
pub(crate) const CHANNELS: usize = 2;
/// Bytes per frame (stereo `f32`).
const STRIDE: usize = 4 * CHANNELS;
/// Frames converted per block in the process callback.
const BLOCK_FRAMES: usize = 2048;
/// Frames produced when PipeWire does not say how many it wants.
const FALLBACK_FRAMES: usize = 1024;
/// Longest wait for PipeWire to create a node.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// The PipeWire socket of this session, if PipeWire runs.
#[must_use]
pub(crate) fn socket_path() -> Option<PathBuf> {
    let name = std::env::var("PIPEWIRE_REMOTE").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| "pipewire-0".into());
    let name = PathBuf::from(name);
    if name.is_absolute() {
        return name.exists().then_some(name);
    }
    ["PIPEWIRE_RUNTIME_DIR", "XDG_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|v| std::env::var_os(v).filter(|d| !d.is_empty()))
        .map(|dir| PathBuf::from(dir).join(&name))
        .find(|p| p.exists())
}

/// Why virtual devices are unavailable, or `None` if they can be created.
#[must_use]
pub(crate) fn unavailable_reason() -> Option<&'static str> {
    socket_path().is_none().then_some("PipeWire is not running in this session (virtual devices need PipeWire)")
}

/// `node.name` of a virtual device: stable, lower-case, no spaces
/// (`mixercraft.sink.game-chat` for "Game Chat"), so applications and
/// WirePlumber can refer to it.
#[must_use]
pub(crate) fn node_name(name: &str, speaker: bool) -> String {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "device" } else { slug };
    format!("mixercraft.{}.{slug}", if speaker { "sink" } else { "source" })
}

/// What desktop mixers show: "MixerCraft Game", or "Browser (MixerCraft
/// Input 2)" when the strip or bus has another name than the device id.
fn description(name: &str, label: Option<&str>) -> String {
    let product = format!("{PRODUCT_PREFIX}{name}");
    let text = match label.map(str::trim).filter(|l| !l.is_empty() && *l != name) {
        Some(label) => format!("{label} ({product})"),
        None => product,
    };
    text.replace('\0', "")
}

pub(crate) fn info(name: &str) -> StreamInfo {
    StreamInfo {
        device_id: format!("{ID_PREFIX}{name}"),
        device_name: format!("{PRODUCT_PREFIX}{name}"),
        sample_rate: RATE,
        channels: CHANNELS as u16,
        buffer_frames: None,
        sample_format: "F32 (PipeWire)".into(),
    }
}

type OnError = Box<dyn FnMut(StreamFault) + Send>;

type OnSinkData = Box<dyn FnMut(&[f32]) + Send>;
type OnSourceData = Box<dyn FnMut(&mut [f32]) + Send>;

/// The engine side of a virtual device.
enum Port {
    /// Applications play into it; the engine receives their audio.
    Sink(OnSinkData),
    /// Applications record from it; the engine fills it.
    Source(OnSourceData),
}

/// Wanted names: (virtual name, is sink) -> strip or bus name.
type Labels = Arc<Mutex<HashMap<(String, bool), String>>>;

/// Creates virtual devices as PipeWire nodes.
pub(crate) struct Virtual {
    labels: Labels,
}

impl std::fmt::Debug for Virtual {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Virtual").field("backend", &"PipeWire").finish_non_exhaustive()
    }
}

/// An open virtual device; dropping it removes the node.
pub(crate) struct Handle {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Virtual {
    /// Checks that PipeWire runs. The memory file is a Windows feature
    /// (default-device roles); PipeWire/WirePlumber remember defaults by
    /// node name themselves.
    pub(crate) fn start(_memory_file: Option<PathBuf>) -> Result<Self, String> {
        if let Some(why) = unavailable_reason() {
            return Err(why.into());
        }
        pw::init();
        Ok(Self { labels: Arc::default() })
    }

    pub(crate) fn info(name: &str) -> StreamInfo {
        info(name)
    }

    /// Sets the strip or bus name shown with the device. PipeWire streams
    /// cannot change their description through the safe API, so it applies
    /// the next time the device opens (e.g. on the next engine start).
    pub(crate) fn set_label(&self, name: &str, speaker: bool, label: &str) {
        self.labels.lock().unwrap_or_else(PoisonError::into_inner).insert((name.to_owned(), speaker), label.to_owned());
    }

    fn label(&self, name: &str, speaker: bool) -> Option<String> {
        self.labels.lock().unwrap_or_else(PoisonError::into_inner).get(&(name.to_owned(), speaker)).cloned()
    }

    pub(crate) fn open_speaker<D, E>(&self, name: &str, on_data: D, on_error: E) -> Result<Handle, String>
    where
        D: FnMut(&[f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        let desc = description(name, self.label(name, true).as_deref());
        open(node_name(name, true), desc, Port::Sink(Box::new(on_data)), Box::new(on_error))
    }

    pub(crate) fn open_microphone<D, E>(&self, name: &str, on_data: D, on_error: E) -> Result<Handle, String>
    where
        D: FnMut(&mut [f32]) + Send + 'static,
        E: FnMut(StreamFault) + Send + 'static,
    {
        let desc = description(name, self.label(name, false).as_deref());
        open(node_name(name, false), desc, Port::Source(Box::new(on_data)), Box::new(on_error))
    }
}

/// Starts the stream thread and waits until PipeWire created the node.
fn open(node: String, description: String, port: Port, on_error: OnError) -> Result<Handle, String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (stop, stop_rx) = pw::channel::channel::<()>();
    let thread = std::thread::Builder::new()
        .name(format!("mixercraft-pw {node}"))
        .spawn(move || {
            let failed = ready_tx.clone();
            if let Err(e) = run(node, description, port, on_error, stop_rx, ready_tx) {
                let _ = failed.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    match ready_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(Ok(())) => Ok(Handle { stop, thread: Some(thread) }),
        Ok(Err(e)) => {
            let _ = stop.send(());
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            // Leave the thread: it quits when PipeWire answers or the stop
            // message arrives; joining could block the supervisor.
            let _ = stop.send(());
            Err("PipeWire did not create the virtual device in time".into())
        }
    }
}

/// Engine port and scratch buffers, owned by the process callback.
struct Io {
    port: Port,
    scratch: Vec<f32>,
}

impl Io {
    fn process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else { return };
        let requested = usize::try_from(buffer.requested()).unwrap_or(0);
        let Some(data) = buffer.datas_mut().first_mut() else { return };
        match &mut self.port {
            Port::Sink(on_data) => {
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data() else { return };
                let end = offset.saturating_add(size).min(bytes.len());
                read_frames(&bytes[offset.min(end)..end], &mut self.scratch, on_data);
            }
            Port::Source(on_data) => {
                let Some(bytes) = data.data() else { return };
                let frames = write_frames(bytes, requested, &mut self.scratch, on_data);
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = STRIDE as i32;
                *chunk.size_mut() = (frames * STRIDE) as u32;
            }
        }
    }
}

/// Hands the F32LE frames in `bytes` to `on_data` as `f32`, in blocks of at
/// most `scratch.len()` samples. Real-time safe: no allocation.
fn read_frames(bytes: &[u8], scratch: &mut [f32], on_data: &mut dyn FnMut(&[f32])) {
    for block in bytes.chunks(scratch.len() * 4) {
        let frames = block.len() / STRIDE;
        let out = &mut scratch[..frames * CHANNELS];
        for (o, b) in out.iter_mut().zip(block.as_chunks::<4>().0) {
            *o = f32::from_le_bytes(*b);
        }
        if !out.is_empty() {
            on_data(out);
        }
    }
}

/// Fills `bytes` with `requested` frames (or [`FALLBACK_FRAMES`] when
/// PipeWire does not say, at most what fits) from `on_data`, as F32LE
/// clamped to ±1. Returns the number of frames written. Real-time safe.
fn write_frames(bytes: &mut [u8], requested: usize, scratch: &mut [f32], on_data: &mut dyn FnMut(&mut [f32])) -> usize {
    let wanted = if requested > 0 { requested } else { FALLBACK_FRAMES };
    let frames = wanted.min(bytes.len() / STRIDE);
    let block_bytes = (scratch.len() / CHANNELS) * STRIDE;
    for block in bytes[..frames * STRIDE].chunks_mut(block_bytes) {
        let buf = &mut scratch[..block.len() / 4];
        on_data(buf);
        for (o, s) in block.as_chunks_mut::<4>().0.iter_mut().zip(buf.iter()) {
            *o = s.clamp(-1.0, 1.0).to_le_bytes();
        }
    }
    frames
}

/// The `EnumFormat` parameter: F32LE stereo at [`RATE`].
fn format_param() -> Result<Vec<u8>, String> {
    let mut audio = AudioInfoRaw::new();
    audio.set_format(AudioFormat::F32LE);
    audio.set_rate(RATE);
    audio.set_channels(CHANNELS as u32);
    let mut position = [0; pw::spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    audio.set_position(position);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| format!("cannot build the PipeWire format: {e:?}"))
}

/// Runs one virtual device until `stop` receives a message.
fn run(
    node: String,
    description: String,
    port: Port,
    on_error: OnError,
    stop: pw::channel::Receiver<()>,
    ready: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let pw_err = |what: &str| {
        let what = what.to_owned();
        move |e: pw::Error| format!("PipeWire: cannot {what}: {e}")
    };
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_err("create a main loop"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_err("create a context"))?;
    let core = context.connect_rc(None).map_err(pw_err("connect"))?;

    // Until the node exists, problems go to `open` (as its error); after
    // that, once, to the engine (its recovery reopens the device). Nothing
    // is reported while stopping.
    let ready = Rc::new(RefCell::new(Some(ready)));
    let stopping = Rc::new(Cell::new(false));
    let report: Rc<dyn Fn(FaultKind, String)> = {
        let (ready, stopping, on_error) = (Rc::clone(&ready), Rc::clone(&stopping), RefCell::new(on_error));
        let reported = Cell::new(false);
        Rc::new(move |kind, message| {
            if stopping.get() {
                return;
            }
            if let Some(r) = ready.borrow_mut().take() {
                let _ = r.send(Err(message));
            } else if !reported.replace(true) {
                (on_error.borrow_mut())(StreamFault { kind, message });
            }
        })
    };

    let core_report = Rc::clone(&report);
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, _res, message| {
            if id == pw::core::PW_ID_CORE {
                core_report(FaultKind::StreamInvalidated, format!("PipeWire: {message}"));
            }
        })
        .register();

    let speaker = matches!(port, Port::Sink(_));
    let mut props = pw::properties::PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CLASS, if speaker { "Audio/Sink" } else { "Audio/Source" });
    props.insert(*pw::keys::NODE_NAME, node.as_str());
    props.insert(*pw::keys::NODE_DESCRIPTION, description.as_str());
    props.insert(*pw::keys::AUDIO_CHANNELS, CHANNELS.to_string());
    props.insert("audio.position", "FL,FR");
    // Keep being scheduled while no application is connected.
    props.insert(*pw::keys::NODE_ALWAYS_PROCESS, "true");
    props.insert("device.icon-name", "audio-card");

    let stream = StreamRc::new(core.clone(), &description, props).map_err(pw_err("create a stream"))?;
    let io = Io { port, scratch: vec![0.0; BLOCK_FRAMES * CHANNELS] };
    let (param_report, state_report) = (Rc::clone(&report), Rc::clone(&report));
    let state_ready = Rc::clone(&ready);
    let _listener = stream
        .add_local_listener_with_user_data(io)
        .param_changed(move |_, _, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((MediaType::Audio, MediaSubtype::Raw)) = format_utils::parse_format(param) else { return };
            let mut got = AudioInfoRaw::new();
            if got.parse(param).is_ok()
                && (got.format() != AudioFormat::F32LE || got.rate() != RATE || got.channels() != CHANNELS as u32)
            {
                param_report(
                    FaultKind::StreamInvalidated,
                    format!(
                        "PipeWire negotiated {:?} {} Hz {} channels instead of F32LE {RATE} Hz stereo",
                        got.format(),
                        got.rate(),
                        got.channels()
                    ),
                );
            }
        })
        .state_changed(move |_, _, _old, new| match new {
            StreamState::Paused | StreamState::Streaming => {
                if let Some(r) = state_ready.borrow_mut().take() {
                    let _ = r.send(Ok(()));
                }
            }
            StreamState::Error(message) => state_report(FaultKind::StreamInvalidated, format!("PipeWire: {message}")),
            StreamState::Unconnected => {
                state_report(FaultKind::DeviceLost, "PipeWire removed the virtual device".into());
            }
            StreamState::Connecting => {}
        })
        .process(|stream, io| io.process(stream))
        .register()
        .map_err(pw_err("listen to the stream"))?;

    let format = format_param()?;
    let mut params = [Pod::from_bytes(&format).ok_or("invalid PipeWire format")?];
    let direction = if speaker { Direction::Input } else { Direction::Output };
    // No AUTOCONNECT: the node is a device; applications connect to it.
    stream.connect(direction, None, StreamFlags::MAP_BUFFERS, &mut params).map_err(pw_err("connect the stream"))?;

    // The process callback runs on this thread (no RT_PROCESS: the listener
    // is thread-local), so this thread does the audio.
    let _ = audio_thread_priority::promote_current_thread_to_real_time(FALLBACK_FRAMES as u32, RATE);

    let weak = mainloop.downgrade();
    let stop_flag = Rc::clone(&stopping);
    let _stop = stop.attach(mainloop.loop_(), move |()| {
        stop_flag.set(true);
        if let Some(l) = weak.upgrade() {
            l.quit();
        }
    });
    mainloop.run();
    stopping.set(true);
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_names_are_stable_slugs() {
        assert_eq!(node_name("Game", true), "mixercraft.sink.game");
        assert_eq!(node_name("Game Chat", false), "mixercraft.source.game-chat");
        assert_eq!(node_name("  Voice / Chat!! ", true), "mixercraft.sink.voice-chat");
        assert_eq!(node_name("Ära", true), "mixercraft.sink.ra");
        assert_eq!(node_name("???", true), "mixercraft.sink.device");
    }

    #[test]
    fn frames_convert_both_ways_in_blocks() {
        // 5 stereo frames through a 2-frame scratch buffer: blocks of 2, 2, 1.
        let samples: Vec<f32> = (0..10).map(|i| i as f32 / 10.0).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut scratch = [0.0f32; 4];
        let (mut got, mut calls) = (Vec::new(), 0);
        read_frames(&bytes, &mut scratch, &mut |b: &[f32]| {
            calls += 1;
            got.extend_from_slice(b);
        });
        assert_eq!((got, calls), (samples.clone(), 3));

        let mut out = vec![0u8; 8 * STRIDE];
        let mut next = 0;
        let frames = write_frames(&mut out, 5, &mut scratch, &mut |b: &mut [f32]| {
            for s in b {
                *s = if next == 0 { 3.0 } else { next as f32 / 10.0 }; // over full scale clamps
                next += 1;
            }
        });
        assert_eq!((frames, next), (5, 10));
        let back: Vec<f32> = out[..frames * STRIDE].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(back[0], 1.0);
        assert_eq!(back[1..], samples[1..]);
        // Unknown request: the fallback, limited by the buffer.
        assert_eq!(write_frames(&mut out, 0, &mut scratch, &mut |_| {}), 8);
    }

    #[test]
    fn descriptions_follow_the_strip_name() {
        assert_eq!(description("Game", None), "MixerCraft Game");
        assert_eq!(description("Game", Some("Game")), "MixerCraft Game");
        assert_eq!(description("Input 2", Some("Browser")), "Browser (MixerCraft Input 2)");
        assert_eq!(description("A\0B", None), "MixerCraft AB");
        assert_eq!(info("Game").device_id, "virtual:Game");
    }
}
