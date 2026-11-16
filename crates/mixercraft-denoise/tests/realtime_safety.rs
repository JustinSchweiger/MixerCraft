//! `process_frame` must not allocate, also on a thread that never ran it
//! before (upstream allocated FFT plans per thread on first use).

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use mixercraft_denoise::{DenoiseState, FRAME_SIZE};

#[global_allocator]
static ALLOC: AllocDisabler = AllocDisabler;

#[test]
fn process_frame_does_not_allocate_on_a_fresh_thread() {
    let mut state = DenoiseState::new();
    let noisy: Vec<f32> = (0..FRAME_SIZE).map(|i| ((i * 7919) % 2000) as f32 - 1000.0).collect();
    std::thread::spawn(move || {
        let mut out = [0.0f32; FRAME_SIZE];
        for _ in 0..100 {
            assert_no_alloc(|| {
                state.process_frame(&mut out, &noisy);
            });
        }
    })
    .join()
    .unwrap();
}
