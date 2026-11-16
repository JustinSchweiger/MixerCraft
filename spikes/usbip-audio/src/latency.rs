//! Round-trip test through the virtual cable: clicks are played into the
//! virtual speaker and detected on the virtual microphone. Latency is
//! app-to-app wall-clock time: when the click is handed to Windows versus
//! when the recording app receives it (cpal's stream timestamps were not
//! usable on this device).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use std::time::Instant;

fn name(d: &cpal::Device) -> String {
    d.description().map(|desc| desc.name().to_string()).unwrap_or_else(|_| d.to_string())
}

pub fn list() {
    let host = cpal::default_host();
    for d in host.devices().expect("enumerate devices") {
        let dir = match (d.supports_input(), d.supports_output()) {
            (true, true) => "in/out",
            (true, false) => "in",
            (false, true) => "out",
            _ => "?",
        };
        let id = d.id().map(|i| i.to_string()).unwrap_or_default();
        println!("{dir:6}  {}  [{id}]", name(&d));
    }
}

/// Prints the default output and input device names (unfiltered).
pub fn defaults() {
    let host = cpal::default_host();
    let out = host.default_output_device().map_or_else(|| "-".to_owned(), |d| name(&d));
    let inp = host.default_input_device().map_or_else(|| "-".to_owned(), |d| name(&d));
    println!("default out: {out} | default in: {inp}");
}

fn find(host: &cpal::Host, part: &str, output: bool) -> cpal::Device {
    host.devices()
        .expect("enumerate devices")
        .find(|d| (if output { d.supports_output() } else { d.supports_input() }) && name(d).contains(part))
        .unwrap_or_else(|| panic!("no {} device containing {part:?}", if output { "output" } else { "input" }))
}

pub fn run(out_part: &str, in_part: &str, secs: u64) {
    let host = cpal::default_host();
    let out_dev = find(&host, out_part, true);
    let in_dev = find(&host, in_part, false);
    let out_cfg = out_dev.default_output_config().expect("output config");
    let in_cfg = in_dev.default_input_config().expect("input config");
    println!("play into   {} ({:?})", name(&out_dev), out_cfg);
    println!("record from {} ({:?})", name(&in_dev), in_cfg);
    assert_eq!(out_cfg.sample_format(), SampleFormat::F32, "spike handles f32 shared-mode streams only");
    assert_eq!(in_cfg.sample_format(), SampleFormat::F32, "spike handles f32 shared-mode streams only");

    let out_rate = f64::from(out_cfg.sample_rate());
    let in_rate = f64::from(in_cfg.sample_rate());
    let out_ch = usize::from(out_cfg.channels());
    let in_ch = usize::from(in_cfg.channels());
    let period = (out_rate * 0.5) as u64; // a click every 500 ms

    let sent: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let results: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let callbacks: Arc<Mutex<(Vec<usize>, Vec<usize>)>> = Arc::new(Mutex::new((Vec::new(), Vec::new())));

    let sent_out = Arc::clone(&sent);
    let cb_out = Arc::clone(&callbacks);
    let mut n: u64 = 0;
    let out_stream = out_dev
        .build_output_stream::<f32, _, _>(
            out_cfg.config(),
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                cb_out.lock().unwrap().0.push(data.len() / out_ch);
                // Map the stream clock onto Instant: playback starts this
                // much after the callback.
                // Wall clock: when this buffer is handed to Windows.
                let _ = info;
                let base = Instant::now();
                for (i, frame) in data.chunks_mut(out_ch).enumerate() {
                    let pos = n % period;
                    let v = if pos < 24 { 0.8 } else { 0.0 };
                    if pos == 0 {
                        let at = base + Duration::from_secs_f64(i as f64 / out_rate);
                        sent_out.lock().unwrap().push_back(at);
                    }
                    frame.fill(v);
                    n += 1;
                }
            },
            |e| eprintln!("output error: {e}"),
            None,
        )
        .expect("open output");

    let level: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
    let level_in = Arc::clone(&level);
    let sent_in = Arc::clone(&sent);
    let res_in = Arc::clone(&results);
    let cb_in = Arc::clone(&callbacks);
    let mut quiet = 0usize;
    let in_stream = in_dev
        .build_input_stream::<f32, _, _>(
            in_cfg.config(),
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                cb_in.lock().unwrap().1.push(data.len() / in_ch);
                // Wall clock: when this buffer reaches the recording app (the
                // last frame "now", earlier frames before it).
                let _ = info;
                let frames = data.len() / in_ch;
                let base = Instant::now()
                    .checked_sub(Duration::from_secs_f64(frames as f64 / in_rate))
                    .unwrap_or_else(Instant::now);
                let peak = data.iter().fold(0.0f32, |m, s| m.max(s.abs()));
                {
                    let mut lv = level_in.lock().unwrap();
                    *lv = lv.max(peak);
                }
                for (j, frame) in data.chunks(in_ch).enumerate() {
                    let loud = frame.iter().any(|s| s.abs() > 0.3);
                    if loud && quiet > (in_rate * 0.1) as usize {
                        let at = base + Duration::from_secs_f64(j as f64 / in_rate);
                        // Pair with the latest click played before this one.
                        // Signed offset to the nearest click, in ms.
                        let q = sent_in.lock().unwrap();
                        let signed = |t: &Instant| {
                            if at >= *t {
                                at.duration_since(*t).as_secs_f64() * 1000.0
                            } else {
                                -(t.duration_since(at).as_secs_f64() * 1000.0)
                            }
                        };
                        let nearest = q.iter().map(signed).min_by(|a, b| a.abs().total_cmp(&b.abs()));
                        drop(q);
                        if let Some(ms) = nearest {
                            println!("click round trip {ms:7.1} ms");
                            res_in.lock().unwrap().push(ms);
                        }
                    }
                    quiet = if loud { 0 } else { quiet + 1 };
                }
            },
            |e| eprintln!("input error: {e}"),
            None,
        )
        .expect("open input");

    in_stream.play().expect("start input");
    out_stream.play().expect("start output");
    for _ in 0..secs {
        std::thread::sleep(Duration::from_secs(1));
        let mut lv = level.lock().unwrap();
        println!("input peak {:.3}", *lv);
        *lv = 0.0;
    }
    drop(out_stream);
    drop(in_stream);

    let r = results.lock().unwrap();
    let cb = callbacks.lock().unwrap();
    let avg = |v: &[usize]| if v.is_empty() { 0.0 } else { v.iter().sum::<usize>() as f64 / v.len() as f64 };
    println!(
        "clicks sent {}, detected {}; output callback {:.0} frames, input callback {:.0} frames",
        secs * 2,
        r.len(),
        avg(&cb.0),
        avg(&cb.1)
    );
    if !r.is_empty() {
        let min = r.iter().copied().fold(f64::MAX, f64::min);
        let max = r.iter().copied().fold(f64::MIN, f64::max);
        let mean = r.iter().sum::<f64>() / r.len() as f64;
        println!("round trip: min {min:.1} ms, mean {mean:.1} ms, max {max:.1} ms");
    }
}
