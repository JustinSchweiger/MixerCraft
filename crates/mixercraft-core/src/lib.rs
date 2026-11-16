//! Platform-independent real-time audio core of MixerCraft.
//!
//! This crate contains everything that runs on (or feeds) the real-time audio
//! thread and does not depend on any operating-system API:
//!
//! * [`buffer`] – fixed-capacity planar audio blocks,
//! * [`dsp`] – gain conversion and click-free gain smoothing,
//! * [`meter`] – lock-free peak/RMS metering,
//! * [`channel_map`] – channel up/down-mixing matrices,
//! * [`mixer`] – strips, buses and the routing matrix,
//! * [`resampler`] – band-limited variable-ratio sample-rate conversion,
//! * [`drift`] – clock-drift compensation control loop,
//! * [`fifo`] – lock-free device FIFOs combining the above,
//! * [`engine`] – the processing cycle that ties ports and mixer together.
//!
//! # Real-time rules
//!
//! Every function documented as *real-time safe* must not allocate, free,
//! lock (blocking), perform I/O or run unbounded loops. All allocation happens
//! at construction time on a control thread. Objects that have to be freed
//! after being used on the audio thread are sent back through a "garbage"
//! channel (see [`engine::Garbage`]). The test-suite verifies the hot paths
//! with an allocation-trapping global allocator.

pub mod buffer;
pub mod channel_map;
pub mod drift;
pub mod dsp;
pub mod engine;
pub mod fifo;
pub mod meter;
pub mod mixer;
pub mod resampler;
pub mod stats;

/// Maximum number of channels supported per strip, bus or device stream.
///
/// Fixed so that channel matrices can live in fixed-size arrays without
/// allocation. 8 covers stereo through 7.1.
pub const MAX_CHANNELS: usize = 8;

/// Maximum number of buses a mixer can have.
///
/// Routing is stored as a bitmask in a `u64`, see [`mixer::Mixer`].
pub const MAX_BUSES: usize = 64;
