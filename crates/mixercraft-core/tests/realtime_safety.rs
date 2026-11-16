//! Verifies that the real-time paths never touch the allocator.
//!
//! `assert_no_alloc` installs a global allocator that aborts the process if
//! an allocation or deallocation happens inside `assert_no_alloc(|| ..)`.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use mixercraft_core::engine::{Command, engine};
use mixercraft_core::fifo::{FifoSpec, fifo};
use mixercraft_core::fx::FxParams;
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
                    eq_high_db: 4.0
                },
                ..StripSpec::stereo()
            };
            4
        ],
        buses: vec![BusSpec::stereo(); 2],
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
            let params = FxParams { eq_mid_db: (i % 7) as f32, comp_amount: (i % 11) as f32, ..FxParams::default() };
            handle.send(Command::Mixer(MixerCommand::StripFx { strip: 1, params })).unwrap();
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
