//! Verifies that the real-time paths never touch the allocator.
//!
//! `assert_no_alloc` installs a global allocator that aborts the process if
//! an allocation or deallocation happens inside `assert_no_alloc(|| ..)`.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use mixercraft_core::engine::{Command, engine};
use mixercraft_core::fifo::{FifoSpec, fifo};
use mixercraft_core::fx::FxParams;
use mixercraft_core::keepawake::{KeepAwakeMode, KeepAwakeParams};
use mixercraft_core::mixer::{BusSpec, Mixer, MixerCommand, MixerLayout, StripSpec};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn engine_cycle_with_fifos_and_commands_does_not_allocate() {
    let layout = MixerLayout {
        strips: vec![
            StripSpec {
                routes: 0b11,
                fx: FxParams {
                    gate_threshold_db: -50.0,
                    comp_amount: 6.0,
                    eq_low_db: 3.0,
                    eq_mid_db: -2.0,
                    eq_high_db: 4.0,
                    noise_suppression: true,
                },
                ..StripSpec::stereo()
            };
            4
        ],
        buses: vec![
            BusSpec {
                keep_awake: KeepAwakeParams { enabled: true, ..KeepAwakeParams::default() },
                ..BusSpec::stereo()
            },
            BusSpec::stereo(),
        ],
    };
    let mixer = Mixer::new(&layout, 1024, 48_000).unwrap();
    let (mut core, mut handle) = engine(mixer, 48_000, Some(0));

    // Input at 44.1 kHz (resampled), output at 48 kHz with drift loop active.
    let (mut in_w, in_r) = fifo(FifoSpec::new(2, 44_100, 48_000, 10.0));
    let (out_w, mut out_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 10.0));
    handle.send(Command::AttachInput { strip: 0, reader: Box::new(in_r) }).unwrap();
    handle.send(Command::AttachOutput { bus: 1, writer: Box::new(out_w) }).unwrap();

    let input = vec![0.1f32; 441 * 2];
    let mut master = vec![0.0f32; 480 * 2];
    let mut second = vec![0.0f32; 480 * 2];
    let detach_target = {
        let (_w, r) = fifo(FifoSpec::new(2, 48_000, 48_000, 10.0));
        Box::new(r)
    };
    handle.send(Command::AttachInput { strip: 1, reader: detach_target }).unwrap();
    handle.send(Command::DetachInput { strip: 1 }).unwrap();

    for i in 0..2_000u32 {
        // Commands are queued outside the guarded region (pushing a small
        // Copy enum into a pre-allocated ring does not allocate either, but
        // keep the guard focused on the audio-thread work).
        if i % 50 == 0 {
            let db = -(((i / 50) % 24) as f32);
            handle.send(Command::Mixer(MixerCommand::StripGain { strip: 0, db })).unwrap();
            handle.send(Command::Mixer(MixerCommand::Route { strip: 2, bus: 0, enabled: i % 100 == 0 })).unwrap();
            let params = FxParams {
                eq_mid_db: (i % 7) as f32,
                comp_amount: (i % 11) as f32,
                noise_suppression: i % 100 == 0,
                ..FxParams::default()
            };
            handle.send(Command::Mixer(MixerCommand::StripFx { strip: 1, params })).unwrap();
            let mode = if i % 400 < 200 { KeepAwakeMode::Pulse } else { KeepAwakeMode::Infrasound };
            let keep_awake =
                KeepAwakeParams { enabled: i % 200 != 0, mode, level_db: -40.0, frequency_hz: 12.0, interval_s: 30.0 };
            handle.send(Command::Mixer(MixerCommand::BusKeepAwake { bus: 1, params: keep_awake })).unwrap();
            handle.send(Command::Mixer(MixerCommand::BusKeepAwakeTest { bus: 1 })).unwrap();
        }
        assert_no_alloc(|| {
            in_w.write(&input);
            core.render_master(&mut master, 2);
            out_r.read(&mut second);
        });
        handle.collect_garbage();
    }
    let snap = handle.stats().snapshot();
    assert_eq!(snap.rt_deallocations, 0);
    assert!(snap.cycles >= 2_000);
}

#[test]
fn reshaping_the_mixer_does_not_allocate_on_the_audio_thread() {
    let layout = |strips: usize, buses: usize| MixerLayout {
        strips: vec![StripSpec { routes: 0b1, ..StripSpec::stereo() }; strips],
        buses: vec![BusSpec::stereo(); buses],
    };
    let (mut core, mut handle) = engine(Mixer::new(&layout(3, 3), 1024, 48_000).unwrap(), 48_000, Some(0));
    let mut readers = Vec::new();
    for strip in 0..3 {
        let (w, r) = fifo(FifoSpec::new(2, 48_000, 48_000, 10.0));
        readers.push(w);
        handle.send(Command::AttachInput { strip, reader: Box::new(r) }).unwrap();
    }
    let (out_w, _out_r) = fifo(FifoSpec::new(2, 48_000, 48_000, 10.0));
    handle.send(Command::AttachOutput { bus: 2, writer: Box::new(out_w) }).unwrap();
    let mut master = vec![0.0f32; 480 * 2];
    assert_no_alloc(|| core.render_master(&mut master, 2));

    // Remove strip 1 and bus 1, then add a strip back.
    let smaller = Mixer::new(&layout(2, 2), 1024, 48_000).unwrap();
    handle.send(Command::Reshape { mixer: Box::new(smaller), remove_input: Some(1), remove_output: Some(1) }).unwrap();
    assert_no_alloc(|| core.render_master(&mut master, 2));
    let bigger = Mixer::new(&layout(3, 2), 1024, 48_000).unwrap();
    handle.send(Command::Reshape { mixer: Box::new(bigger), remove_input: None, remove_output: None }).unwrap();
    assert_no_alloc(|| core.render_master(&mut master, 2));
    handle.collect_garbage();
    assert_eq!(handle.stats().snapshot().rt_deallocations, 0);
    drop(readers);
}

/// The mixer is built on one thread and run on another (as the engine does):
/// noise suppression and the keep-awake tone must not allocate there either,
/// including on their very first cycle.
#[test]
fn denoise_and_keep_awake_do_not_allocate_on_the_audio_thread() {
    let layout = MixerLayout {
        strips: vec![
            StripSpec {
                routes: 0b1,
                fx: FxParams { noise_suppression: true, ..FxParams::default() },
                ..StripSpec::stereo()
            };
            2
        ],
        buses: vec![BusSpec {
            keep_awake: KeepAwakeParams { enabled: true, ..KeepAwakeParams::default() },
            ..BusSpec::stereo()
        }],
    };
    let mut mixer = Mixer::new(&layout, 1024, 48_000).unwrap();
    std::thread::spawn(move || {
        // 12 s of near-silence: the keep-awake tone starts after 10 s.
        for k in 0..1200u32 {
            for s in 0..2 {
                let b = mixer.strip_input_mut(s);
                b.set_frames(480);
                for ch in 0..2 {
                    b.channel_mut(ch).fill(if k % 50 == 0 { 1e-4 } else { 0.0 });
                }
            }
            if k == 600 {
                // Toggle mid-stream.
                let off = FxParams::default();
                mixer.apply(MixerCommand::StripFx { strip: 0, params: off }).unwrap();
            }
            assert_no_alloc(|| {
                mixer.process(480);
            });
        }
        assert!(mixer.bus_output(0).channel(0).iter().any(|s| *s != 0.0), "keep-awake tone is playing");
    })
    .join()
    .unwrap();
}
