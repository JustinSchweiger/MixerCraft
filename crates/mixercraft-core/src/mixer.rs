//! Strips, buses and the routing matrix.
//!
//! The mixer model follows the input-strip / output-bus layout familiar from
//! hardware consoles and Voicemeeter:
//!
//! * a **strip** carries one source (physical input, virtual input, …) through
//!   a fader with mute and solo and can be routed to any number of buses;
//! * a **bus** sums every strip routed to it, applies its own fader and mute
//!   and feeds one destination (physical output, virtual output, …).
//!
//! Every gain change — fader moves, mute, solo, routing switches — is ramped
//! over [`Mixer::RAMP_SECONDS`] to avoid clicks.
//!
//! Solo semantics: as soon as at least one strip is soloed, every strip that
//! is not soloed is silenced on all buses.

use std::sync::Arc;

use crate::buffer::AudioBlock;
use crate::channel_map::ChannelMap;
use crate::denoise::Denoiser;
use crate::dsp::{self, GainShape, SmoothedGain};
use crate::fx::{FxParams, StripFx};
use crate::keepawake::{KeepAwake, KeepAwakeParams};
use crate::meter::MeterBank;
use crate::{MAX_BUSES, MAX_CHANNELS};

/// Initial settings of a strip.
#[derive(Debug, Clone, PartialEq)]
pub struct StripSpec {
    /// Channel count (1..=[`MAX_CHANNELS`]).
    pub channels: usize,
    /// Fader level in dB.
    pub gain_db: f32,
    /// Mute state.
    pub mute: bool,
    /// Solo state.
    pub solo: bool,
    /// Bitmask of buses this strip feeds (bit `n` = bus `n`).
    pub routes: u64,
    /// Gate, EQ and compressor settings.
    pub fx: FxParams,
}

impl StripSpec {
    /// A stereo strip at unity gain, routed nowhere, without processing.
    #[must_use]
    pub fn stereo() -> Self {
        Self { channels: 2, gain_db: 0.0, mute: false, solo: false, routes: 0, fx: FxParams::default() }
    }
}

/// Initial settings of a bus.
#[derive(Debug, Clone, PartialEq)]
pub struct BusSpec {
    /// Channel count (1..=[`MAX_CHANNELS`]).
    pub channels: usize,
    /// Fader level in dB.
    pub gain_db: f32,
    /// Mute state.
    pub mute: bool,
    /// Inaudible tone that keeps auto-standby speakers on.
    pub keep_awake: KeepAwakeParams,
}

impl BusSpec {
    /// A stereo bus at unity gain.
    #[must_use]
    pub fn stereo() -> Self {
        Self { channels: 2, gain_db: 0.0, mute: false, keep_awake: KeepAwakeParams::default() }
    }
}

/// The complete topology a [`Mixer`] is built from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MixerLayout {
    /// Input strips.
    pub strips: Vec<StripSpec>,
    /// Output buses.
    pub buses: Vec<BusSpec>,
}

/// Errors detected when building a mixer or applying a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixerError {
    /// A strip index does not exist.
    NoSuchStrip,
    /// A bus index does not exist.
    NoSuchBus,
    /// More than [`MAX_BUSES`] buses were requested.
    TooManyBuses,
    /// A channel count outside `1..=MAX_CHANNELS`.
    InvalidChannelCount,
    /// A gain value that is NaN.
    InvalidGain,
}

impl std::fmt::Display for MixerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NoSuchStrip => "no such strip",
            Self::NoSuchBus => "no such bus",
            Self::TooManyBuses => "too many buses",
            Self::InvalidChannelCount => "invalid channel count",
            Self::InvalidGain => "invalid gain",
        })
    }
}

impl std::error::Error for MixerError {}

/// A real-time parameter change. Small and `Copy` so it can travel through a
/// lock-free queue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MixerCommand {
    /// Sets a strip fader (dB).
    StripGain { strip: u16, db: f32 },
    /// Mutes or unmutes a strip.
    StripMute { strip: u16, mute: bool },
    /// Solos or unsolos a strip.
    StripSolo { strip: u16, solo: bool },
    /// Connects or disconnects a strip from a bus.
    Route { strip: u16, bus: u16, enabled: bool },
    /// Sets a strip's noise suppression, gate, EQ and compressor.
    StripFx { strip: u16, params: FxParams },
    /// Sets a bus's keep-awake signal.
    BusKeepAwake { bus: u16, params: KeepAwakeParams },
    /// Plays a bus's keep-awake signal once now.
    BusKeepAwakeTest { bus: u16 },
    /// Sets a bus fader (dB).
    BusGain { bus: u16, db: f32 },
    /// Mutes or unmutes a bus.
    BusMute { bus: u16, mute: bool },
}

/// Per-cycle problems found while processing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessReport {
    /// NaN/inf samples that were replaced by silence.
    pub non_finite: u32,
    /// Bus output samples above full scale.
    pub over: u32,
}

#[derive(Debug)]
struct Strip {
    input: AudioBlock,
    gain_db: f32,
    mute: bool,
    solo: bool,
    fader: SmoothedGain,
    denoise: Denoiser,
    fx: StripFx,
    routes: u64,
    sends: Vec<SmoothedGain>,
    maps: Vec<ChannelMap>,
    meter: Arc<MeterBank>,
}

#[derive(Debug)]
struct Bus {
    output: AudioBlock,
    gain_db: f32,
    mute: bool,
    fader: SmoothedGain,
    meter: Arc<MeterBank>,
    keep_awake: KeepAwake,
}

/// Shared handles to every meter of a mixer, for control-side readers.
#[derive(Debug, Clone)]
pub struct MixerMeters {
    /// One bank per strip.
    pub strips: Vec<Arc<MeterBank>>,
    /// One bank per bus.
    pub buses: Vec<Arc<MeterBank>>,
}

/// The mixing matrix. Construction allocates; everything else is real-time
/// safe.
#[derive(Debug)]
pub struct Mixer {
    strips: Vec<Strip>,
    buses: Vec<Bus>,
    ramp_fader: Vec<f32>,
    ramp_send: Vec<f32>,
    max_frames: usize,
    solo_active: bool,
}

fn valid_channels(n: usize) -> bool {
    (1..=MAX_CHANNELS).contains(&n)
}

impl Mixer {
    /// Length of every gain ramp.
    pub const RAMP_SECONDS: f32 = 0.010;

    /// Builds a mixer for `layout` processing at most `max_frames` per cycle.
    ///
    /// # Errors
    /// If the layout has invalid channel counts or too many buses.
    pub fn new(layout: &MixerLayout, max_frames: usize, sample_rate: u32) -> Result<Self, MixerError> {
        if layout.buses.len() > MAX_BUSES {
            return Err(MixerError::TooManyBuses);
        }
        if layout.strips.iter().any(|s| !valid_channels(s.channels))
            || layout.buses.iter().any(|b| !valid_channels(b.channels))
        {
            return Err(MixerError::InvalidChannelCount);
        }
        let ramp = ((sample_rate as f32 * Self::RAMP_SECONDS) as u32).max(1);
        let solo_active = layout.strips.iter().any(|s| s.solo);
        let buses: Vec<Bus> = layout
            .buses
            .iter()
            .map(|b| Bus {
                output: AudioBlock::new(b.channels, max_frames),
                gain_db: b.gain_db,
                mute: b.mute,
                fader: SmoothedGain::new(bus_target(b.gain_db, b.mute), ramp),
                meter: Arc::new(MeterBank::new(b.channels)),
                keep_awake: {
                    let mut k = KeepAwake::new(sample_rate);
                    k.set_params(b.keep_awake);
                    k
                },
            })
            .collect();
        let strips = layout
            .strips
            .iter()
            .map(|s| {
                let routes = s.routes & bus_mask(buses.len());
                Strip {
                    input: AudioBlock::new(s.channels, max_frames),
                    gain_db: s.gain_db,
                    mute: s.mute,
                    solo: s.solo,
                    fader: SmoothedGain::new(strip_target(s.gain_db, s.mute, s.solo, solo_active), ramp),
                    denoise: {
                        let mut d = Denoiser::new(s.channels, sample_rate);
                        d.set_enabled(s.fx.noise_suppression);
                        d
                    },
                    fx: {
                        let mut fx = StripFx::new(sample_rate);
                        fx.set_params(s.fx);
                        fx
                    },
                    routes,
                    sends: (0..buses.len())
                        .map(|b| SmoothedGain::new(if routes & (1 << b) != 0 { 1.0 } else { 0.0 }, ramp))
                        .collect(),
                    maps: buses.iter().map(|b| ChannelMap::default_for(s.channels, b.output.channels())).collect(),
                    meter: Arc::new(MeterBank::new(s.channels)),
                }
            })
            .collect();
        Ok(Self {
            strips,
            buses,
            ramp_fader: vec![0.0; max_frames],
            ramp_send: vec![0.0; max_frames],
            max_frames,
            solo_active,
        })
    }

    /// Maximum frames per [`Self::process`] call.
    #[must_use]
    pub fn max_frames(&self) -> usize {
        self.max_frames
    }

    /// Number of strips.
    #[must_use]
    pub fn strip_count(&self) -> usize {
        self.strips.len()
    }

    /// Number of buses.
    #[must_use]
    pub fn bus_count(&self) -> usize {
        self.buses.len()
    }

    /// Meter handles for control-side readers. Allocates.
    #[must_use]
    pub fn meters(&self) -> MixerMeters {
        MixerMeters {
            strips: self.strips.iter().map(|s| Arc::clone(&s.meter)).collect(),
            buses: self.buses.iter().map(|b| Arc::clone(&b.meter)).collect(),
        }
    }

    /// The input buffer of a strip; fill it before [`Self::process`].
    ///
    /// # Panics
    /// If `strip` is out of range.
    pub fn strip_input_mut(&mut self, strip: usize) -> &mut AudioBlock {
        &mut self.strips[strip].input
    }

    /// The mixed output of a bus after [`Self::process`].
    ///
    /// # Panics
    /// If `bus` is out of range.
    #[must_use]
    pub fn bus_output(&self, bus: usize) -> &AudioBlock {
        &self.buses[bus].output
    }

    /// Replaces the channel map from a strip to a bus. Real-time safe.
    ///
    /// # Errors
    /// If indices are invalid or the map does not match the channel counts.
    pub fn set_channel_map(&mut self, strip: usize, bus: usize, map: ChannelMap) -> Result<(), MixerError> {
        let s = self.strips.get_mut(strip).ok_or(MixerError::NoSuchStrip)?;
        let b = self.buses.get(bus).ok_or(MixerError::NoSuchBus)?;
        if map.src_channels() != s.input.channels() || map.dst_channels() != b.output.channels() {
            return Err(MixerError::InvalidChannelCount);
        }
        s.maps[bus] = map;
        Ok(())
    }

    /// Applies a parameter change. Real-time safe.
    ///
    /// # Errors
    /// If an index is out of range or a gain is NaN.
    pub fn apply(&mut self, cmd: MixerCommand) -> Result<(), MixerError> {
        match cmd {
            MixerCommand::StripGain { strip, db } => {
                if db.is_nan() {
                    return Err(MixerError::InvalidGain);
                }
                self.strip_mut(strip)?.gain_db = db;
                self.retarget_strip(strip as usize);
            }
            MixerCommand::StripMute { strip, mute } => {
                self.strip_mut(strip)?.mute = mute;
                self.retarget_strip(strip as usize);
            }
            MixerCommand::StripSolo { strip, solo } => {
                self.strip_mut(strip)?.solo = solo;
                self.solo_active = self.strips.iter().any(|s| s.solo);
                for i in 0..self.strips.len() {
                    self.retarget_strip(i);
                }
            }
            MixerCommand::Route { strip, bus, enabled } => {
                if bus as usize >= self.buses.len() {
                    return Err(MixerError::NoSuchBus);
                }
                let s = self.strip_mut(strip)?;
                let bit = 1u64 << bus;
                if enabled {
                    s.routes |= bit;
                } else {
                    s.routes &= !bit;
                }
                s.sends[bus as usize].set_target(if enabled { 1.0 } else { 0.0 });
            }
            MixerCommand::StripFx { strip, params } => {
                let s = self.strip_mut(strip)?;
                s.denoise.set_enabled(params.noise_suppression);
                s.fx.set_params(params);
            }
            MixerCommand::BusKeepAwake { bus, params } => {
                self.bus_mut(bus)?.keep_awake.set_params(params);
            }
            MixerCommand::BusKeepAwakeTest { bus } => {
                self.bus_mut(bus)?.keep_awake.trigger_test();
            }
            MixerCommand::BusGain { bus, db } => {
                if db.is_nan() {
                    return Err(MixerError::InvalidGain);
                }
                let b = self.bus_mut(bus)?;
                b.gain_db = db;
                b.fader.set_target(bus_target(b.gain_db, b.mute));
            }
            MixerCommand::BusMute { bus, mute } => {
                let b = self.bus_mut(bus)?;
                b.mute = mute;
                b.fader.set_target(bus_target(b.gain_db, b.mute));
            }
        }
        Ok(())
    }

    fn strip_mut(&mut self, strip: u16) -> Result<&mut Strip, MixerError> {
        self.strips.get_mut(strip as usize).ok_or(MixerError::NoSuchStrip)
    }

    fn bus_mut(&mut self, bus: u16) -> Result<&mut Bus, MixerError> {
        self.buses.get_mut(bus as usize).ok_or(MixerError::NoSuchBus)
    }

    fn retarget_strip(&mut self, i: usize) {
        let solo_active = self.solo_active;
        let s = &mut self.strips[i];
        s.fader.set_target(strip_target(s.gain_db, s.mute, s.solo, solo_active));
    }

    /// Mixes one cycle of `frames` frames. Strip inputs must have been filled
    /// with exactly `frames` frames. Real-time safe.
    ///
    /// # Panics
    /// If `frames` exceeds [`Self::max_frames`].
    pub fn process(&mut self, frames: usize) -> ProcessReport {
        assert!(frames <= self.max_frames, "mixer block too large");
        let mut report = ProcessReport::default();
        let Self { strips, buses, ramp_fader, ramp_send, .. } = self;
        let ramp_fader = &mut ramp_fader[..frames];
        let ramp_send = &mut ramp_send[..frames];

        for bus in buses.iter_mut() {
            bus.output.set_frames(frames);
            bus.output.clear();
        }

        for strip in strips.iter_mut() {
            strip.input.set_frames(frames);
            for ch in 0..strip.input.channels() {
                report.non_finite += dsp::sanitize(strip.input.channel_mut(ch));
            }
            strip.denoise.process(&mut strip.input);
            strip.fx.process(&mut strip.input);
            let shape = strip.fader.render(ramp_fader);
            for ch in 0..strip.input.channels() {
                dsp::apply_gain(strip.input.channel_mut(ch), shape, ramp_fader);
            }
            strip.meter.publish(&strip.input);
            let strip_silent = shape == GainShape::Constant(0.0);
            for (b, bus) in buses.iter_mut().enumerate() {
                // Always advance the send ramp so its timing is independent of
                // whether the strip is currently audible.
                let send = strip.sends[b].render(ramp_send);
                if strip_silent || send == GainShape::Constant(0.0) {
                    continue;
                }
                strip.maps[b].mix_into(&strip.input, &mut bus.output, send, ramp_send);
            }
        }

        for bus in buses.iter_mut() {
            let shape = bus.fader.render(ramp_fader);
            for ch in 0..bus.output.channels() {
                let samples = bus.output.channel_mut(ch);
                dsp::apply_gain(samples, shape, ramp_fader);
                report.non_finite += dsp::sanitize(samples);
                report.over += dsp::count_over(samples);
            }
            bus.meter.publish(&bus.output);
            // After the meter: the tone is not program audio.
            bus.keep_awake.process(&mut bus.output);
        }
        report
    }
}

fn bus_mask(buses: usize) -> u64 {
    if buses >= 64 { u64::MAX } else { (1u64 << buses) - 1 }
}

fn strip_target(gain_db: f32, mute: bool, solo: bool, solo_active: bool) -> f32 {
    if mute || (solo_active && !solo) { 0.0 } else { dsp::db_to_gain(gain_db) }
}

fn bus_target(gain_db: f32, mute: bool) -> f32 {
    if mute { 0.0 } else { dsp::db_to_gain(gain_db) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 48_000;

    fn layout(strips: usize, buses: usize) -> MixerLayout {
        MixerLayout { strips: vec![StripSpec::stereo(); strips], buses: vec![BusSpec::stereo(); buses] }
    }

    fn fill(m: &mut Mixer, strip: usize, value: f32, frames: usize) {
        let b = m.strip_input_mut(strip);
        b.set_frames(frames);
        for ch in 0..b.channels() {
            b.channel_mut(ch).fill(value);
        }
    }

    /// Runs enough cycles for all ramps to settle and returns the last bus
    /// sample of channel 0.
    fn settle(m: &mut Mixer, inputs: &[f32], bus: usize) -> f32 {
        let mut last = 0.0;
        for _ in 0..4 {
            for (i, v) in inputs.iter().enumerate() {
                fill(m, i, *v, 256);
            }
            m.process(256);
            last = *m.bus_output(bus).channel(0).last().unwrap();
        }
        last
    }

    #[test]
    fn routes_sum_into_buses() {
        let mut l = layout(2, 2);
        l.strips[0].routes = 0b01;
        l.strips[1].routes = 0b11;
        let mut m = Mixer::new(&l, 256, SR).unwrap();
        fill(&mut m, 0, 0.25, 64);
        fill(&mut m, 1, 0.5, 64);
        m.process(64);
        assert!(m.bus_output(0).channel(0).iter().all(|&s| s == 0.75));
        assert!(m.bus_output(1).channel(1).iter().all(|&s| s == 0.5));
    }

    #[test]
    fn unrouted_strip_is_silent() {
        let mut m = Mixer::new(&layout(1, 1), 64, SR).unwrap();
        fill(&mut m, 0, 1.0, 64);
        m.process(64);
        assert!(m.bus_output(0).channel(0).iter().all(|&s| s == 0.0));
    }

    #[test]
    fn fader_mute_and_bus_gain() {
        let mut l = layout(1, 1);
        l.strips[0].routes = 1;
        let mut m = Mixer::new(&l, 256, SR).unwrap();
        m.apply(MixerCommand::StripGain { strip: 0, db: -6.0206 }).unwrap();
        assert!((settle(&mut m, &[1.0], 0) - 0.5).abs() < 1e-4);
        m.apply(MixerCommand::BusGain { bus: 0, db: -6.0206 }).unwrap();
        assert!((settle(&mut m, &[1.0], 0) - 0.25).abs() < 1e-4);
        m.apply(MixerCommand::StripMute { strip: 0, mute: true }).unwrap();
        assert_eq!(settle(&mut m, &[1.0], 0), 0.0);
        m.apply(MixerCommand::StripMute { strip: 0, mute: false }).unwrap();
        m.apply(MixerCommand::BusMute { bus: 0, mute: true }).unwrap();
        assert_eq!(settle(&mut m, &[1.0], 0), 0.0);
    }

    #[test]
    fn solo_silences_other_strips() {
        let mut l = layout(3, 1);
        for s in &mut l.strips {
            s.routes = 1;
        }
        let mut m = Mixer::new(&l, 256, SR).unwrap();
        assert!((settle(&mut m, &[0.1, 0.2, 0.4], 0) - 0.7).abs() < 1e-5);
        m.apply(MixerCommand::StripSolo { strip: 1, solo: true }).unwrap();
        assert!((settle(&mut m, &[0.1, 0.2, 0.4], 0) - 0.2).abs() < 1e-5);
        m.apply(MixerCommand::StripSolo { strip: 2, solo: true }).unwrap();
        assert!((settle(&mut m, &[0.1, 0.2, 0.4], 0) - 0.6).abs() < 1e-5);
        m.apply(MixerCommand::StripSolo { strip: 1, solo: false }).unwrap();
        m.apply(MixerCommand::StripSolo { strip: 2, solo: false }).unwrap();
        assert!((settle(&mut m, &[0.1, 0.2, 0.4], 0) - 0.7).abs() < 1e-5);
    }

    #[test]
    fn routing_change_is_click_free() {
        let mut l = layout(1, 1);
        l.strips[0].routes = 0;
        let mut m = Mixer::new(&l, 1024, SR).unwrap();
        m.apply(MixerCommand::Route { strip: 0, bus: 0, enabled: true }).unwrap();
        fill(&mut m, 0, 1.0, 1024);
        m.process(1024);
        let out = m.bus_output(0).channel(0);
        let ramp_len = (SR as f32 * Mixer::RAMP_SECONDS) as usize;
        let max_step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step <= 1.0 / ramp_len as f32 + 1e-6, "step {max_step}");
        assert_eq!(out[1023], 1.0);
    }

    #[test]
    fn strip_fx_apply_before_the_fader_and_can_be_changed_live() {
        let mut l = layout(1, 1);
        l.strips[0].routes = 1;
        let mut m = Mixer::new(&l, 256, SR).unwrap();
        m.apply(MixerCommand::StripFx {
            strip: 0,
            params: FxParams { gate_threshold_db: -20.0, ..FxParams::default() },
        })
        .unwrap();
        // 0.01 (-40 dBFS) is below the -20 dB gate: after the hold (50 ms)
        // and release (120 ms time constant) it is closed.
        for _ in 0..200 {
            fill(&mut m, 0, 0.01, 256);
            m.process(256);
        }
        assert!(m.bus_output(0).channel(0).iter().all(|s| s.abs() < 1e-4));
        assert_eq!(
            m.apply(MixerCommand::StripFx { strip: 3, params: FxParams::default() }),
            Err(MixerError::NoSuchStrip)
        );
        m.apply(MixerCommand::StripFx { strip: 0, params: FxParams::default() }).unwrap();
        assert!((settle(&mut m, &[0.01], 0) - 0.01).abs() < 1e-6, "bypassed again");
    }

    #[test]
    fn mono_strip_feeds_both_bus_channels() {
        let l = MixerLayout {
            strips: vec![StripSpec { channels: 1, routes: 1, ..StripSpec::stereo() }],
            buses: vec![BusSpec::stereo()],
        };
        let mut m = Mixer::new(&l, 16, SR).unwrap();
        fill(&mut m, 0, 0.3, 16);
        m.process(16);
        assert_eq!(m.bus_output(0).channel(0)[0], 0.3);
        assert_eq!(m.bus_output(0).channel(1)[0], 0.3);
    }

    #[test]
    fn non_finite_input_is_contained_and_reported() {
        let mut l = layout(1, 1);
        l.strips[0].routes = 1;
        let mut m = Mixer::new(&l, 8, SR).unwrap();
        fill(&mut m, 0, 0.5, 8);
        m.strip_input_mut(0).channel_mut(0)[3] = f32::NAN;
        let r = m.process(8);
        assert_eq!(r.non_finite, 1);
        assert!(m.bus_output(0).channel(0).iter().all(|s| s.is_finite()));
    }

    #[test]
    fn overs_are_counted_not_clipped() {
        let mut l = layout(2, 1);
        l.strips[0].routes = 1;
        l.strips[1].routes = 1;
        let mut m = Mixer::new(&l, 4, SR).unwrap();
        fill(&mut m, 0, 0.75, 4);
        fill(&mut m, 1, 0.75, 4);
        let r = m.process(4);
        assert_eq!(r.over, 8);
        assert_eq!(m.bus_output(0).channel(0)[0], 1.5);
    }

    #[test]
    fn invalid_commands_are_rejected() {
        let mut m = Mixer::new(&layout(1, 1), 4, SR).unwrap();
        assert_eq!(m.apply(MixerCommand::StripGain { strip: 5, db: 0.0 }), Err(MixerError::NoSuchStrip));
        assert_eq!(m.apply(MixerCommand::Route { strip: 0, bus: 9, enabled: true }), Err(MixerError::NoSuchBus));
        assert_eq!(m.apply(MixerCommand::BusGain { bus: 0, db: f32::NAN }), Err(MixerError::InvalidGain));
        let bad = MixerLayout { strips: vec![StripSpec { channels: 0, ..StripSpec::stereo() }], buses: vec![] };
        assert_eq!(Mixer::new(&bad, 4, SR).err(), Some(MixerError::InvalidChannelCount));
    }

    #[test]
    fn meters_report_post_fader_levels() {
        let mut l = layout(1, 1);
        l.strips[0].routes = 1;
        l.strips[0].gain_db = -6.0206;
        let mut m = Mixer::new(&l, 64, SR).unwrap();
        let meters = m.meters();
        fill(&mut m, 0, 1.0, 64);
        m.process(64);
        assert!((meters.strips[0].take_peaks()[0].peak - 0.5).abs() < 1e-4);
        assert!((meters.buses[0].take_peaks()[1].peak - 0.5).abs() < 1e-4);
    }
}
