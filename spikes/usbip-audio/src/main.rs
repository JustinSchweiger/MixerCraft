//! Spike: a virtual audio cable as an emulated USB Audio Class 1 device,
//! served over USB/IP to the (Microsoft-signed) usbip-win2 client driver.
//! Windows' inbox `usbaudio.sys` then creates ordinary playback and
//! recording endpoints. No kernel code of our own.
//!
//! ```text
//! usbip-audio-spike serve [--sine]   # USB/IP server on 127.0.0.1:3240
//! usbip.exe attach -r 127.0.0.1 -b 1-1
//! usbip-audio-spike latency --out "Virtual Cable" --in "Virtual Cable"
//! ```
//!
//! The device: full speed, 48 kHz, 16-bit stereo, one isochronous OUT
//! endpoint (speaker, adaptive) and one isochronous IN endpoint (microphone,
//! asynchronous). What Windows plays into the speaker comes back out of the
//! microphone (`--sine` sends a 440 Hz tone instead). The server paces URB
//! completions in real time, so it is the clock of both endpoints.
//!
//! Spike code: allocations, locks and logging on the "audio" path are fine
//! here and must not be copied into the engine.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod latency;

const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;
const BUSID: &str = "1-1";
const BUSNUM: u32 = 1;
const DEVNUM: u32 = 2;
const SPEED_FULL: u32 = 2;
const EPIPE: i32 = -32;
const ECONNRESET: i32 = -104;

const RATE: u32 = 48_000;
const FRAME_BYTES: usize = 4; // 16-bit stereo
const FRAMES_PER_MS: usize = (RATE / 1000) as usize;
/// Loopback buffer limit (frames): 30 ms. Older audio is dropped, so the
/// cable never builds up latency while nothing records.
const RING_MAX_FRAMES: usize = 30 * FRAMES_PER_MS;

// ---------------------------------------------------------------- descriptors

fn device_descriptor() -> Vec<u8> {
    vec![
        18, 1, // bLength, DEVICE
        0x00, 0x02, // bcdUSB 2.00 (full speed device)
        0, 0, 0,  // class in interfaces
        64, // bMaxPacketSize0
        0x09, 0x12, // idVendor 0x1209 (pid.codes)
        0x01, 0x00, // idProduct 0x0001 (pid.codes test PID; spike only)
        0x00, 0x01, // bcdDevice 1.00
        1, 2, 3, // strings
        1, // configurations
    ]
}

fn config_descriptor() -> Vec<u8> {
    // Audio control: playback IT(1, USB streaming) -> OT(2, speaker);
    // capture IT(3, microphone) -> OT(4, USB streaming).
    let it_play = [12, 0x24, 0x02, 1, 0x01, 0x01, 0, 2, 0x03, 0x00, 0, 0];
    let ot_spk = [9, 0x24, 0x03, 2, 0x01, 0x03, 0, 1, 0];
    let it_mic = [12, 0x24, 0x02, 3, 0x01, 0x02, 0, 2, 0x03, 0x00, 0, 0];
    let ot_usb = [9, 0x24, 0x03, 4, 0x01, 0x01, 0, 3, 0];
    let ac_total: u16 = 10 + 12 + 9 + 12 + 9;
    let [lo, hi] = ac_total.to_le_bytes();
    let ac_header = [10, 0x24, 0x01, 0x00, 0x01, lo, hi, 2, 1, 2];
    // Type I format: 2 channels, 2 bytes, 16 bits, one rate (48000 = 0x00BB80).
    let format = [11, 0x24, 0x02, 1, 2, 2, 16, 1, 0x80, 0xBB, 0x00];
    // Room for 49 frames per 1 ms packet.
    let max_packet = ((FRAMES_PER_MS + 1) * FRAME_BYTES) as u16;
    let [mp_lo, mp_hi] = max_packet.to_le_bytes();

    let mut d: Vec<u8> = vec![9, 2, 0, 0, 3, 1, 0, 0x80, 50];
    // Interface 0: audio control.
    d.extend_from_slice(&[9, 4, 0, 0, 0, 1, 1, 0, 0]);
    for part in [&ac_header[..], &it_play, &ot_spk, &it_mic, &ot_usb] {
        d.extend_from_slice(part);
    }
    // Interface 1: playback streaming (alt 0 = idle, alt 1 = streaming).
    d.extend_from_slice(&[9, 4, 1, 0, 0, 1, 2, 0, 0]);
    d.extend_from_slice(&[9, 4, 1, 1, 1, 1, 2, 0, 0]);
    d.extend_from_slice(&[7, 0x24, 0x01, 1, 1, 0x01, 0x00]);
    d.extend_from_slice(&format);
    d.extend_from_slice(&[9, 5, 0x01, 0x09, mp_lo, mp_hi, 1, 0, 0]); // iso OUT, adaptive
    d.extend_from_slice(&[7, 0x25, 0x01, 0x00, 0, 0, 0]);
    // Interface 2: capture streaming.
    d.extend_from_slice(&[9, 4, 2, 0, 0, 1, 2, 0, 0]);
    d.extend_from_slice(&[9, 4, 2, 1, 1, 1, 2, 0, 0]);
    d.extend_from_slice(&[7, 0x24, 0x01, 4, 1, 0x01, 0x00]);
    d.extend_from_slice(&format);
    d.extend_from_slice(&[9, 5, 0x82, 0x05, mp_lo, mp_hi, 1, 0, 0]); // iso IN, async
    d.extend_from_slice(&[7, 0x25, 0x01, 0x00, 0, 0, 0]);
    let [lo, hi] = (d.len() as u16).to_le_bytes();
    d[2] = lo;
    d[3] = hi;
    d
}

fn string_descriptor(index: u8) -> Option<Vec<u8>> {
    let text = match index {
        0 => return Some(vec![4, 3, 0x09, 0x04]), // English (US)
        1 => "MixerCraft",
        2 => "MixerCraft Virtual Cable",
        3 => "SPIKE0001",
        _ => return None,
    };
    let mut d = vec![0, 3];
    for u in text.encode_utf16() {
        d.extend_from_slice(&u.to_le_bytes());
    }
    d[0] = d.len() as u8;
    Some(d)
}

/// Answers a control transfer. `Err` is a USB/IP status (stall = -EPIPE).
fn control(setup: [u8; 8], state: &Shared) -> Result<Vec<u8>, i32> {
    let (bm, req) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let index = u16::from_le_bytes([setup[4], setup[5]]);
    let len = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
    let reply = |mut v: Vec<u8>| {
        v.truncate(len);
        Ok(v)
    };
    match (bm, req) {
        (0x80, 6) => {
            let d = match (value >> 8) as u8 {
                1 => device_descriptor(),
                2 => config_descriptor(),
                3 => string_descriptor(value as u8).ok_or(EPIPE)?,
                _ => return Err(EPIPE), // device qualifier, BOS, ...
            };
            reply(d)
        }
        (0x80 | 0x81 | 0x82, 0) => reply(vec![0, 0]),
        (0x80, 8) => reply(vec![1]),
        (0x81, 10) => reply(vec![state.alt(index as u8)]),
        (0x00, 9) => {
            log(&format!("SET_CONFIGURATION {value}"));
            Ok(Vec::new())
        }
        (0x01, 11) => {
            log(&format!("SET_INTERFACE interface {index} alt {value}"));
            state.set_alt(index as u8, value as u8);
            Ok(Vec::new())
        }
        (0x00 | 0x01 | 0x02, 1 | 3) => Ok(Vec::new()), // CLEAR/SET_FEATURE
        (b, r) if b & 0x60 == 0x20 => {
            // Audio class request (sampling frequency, etc.).
            log(&format!("class request bm={b:#04x} req={r:#04x} value={value:#06x} index={index:#06x} len={len}"));
            if b & 0x80 != 0 {
                let rate = RATE.to_le_bytes();
                reply(if value >> 8 == 1 { vec![rate[0], rate[1], rate[2]] } else { vec![0; len] })
            } else {
                Ok(Vec::new())
            }
        }
        _ => {
            log(&format!("unhandled control bm={bm:#04x} req={req:#04x} value={value:#06x} index={index:#06x}"));
            Err(EPIPE)
        }
    }
}

// ---------------------------------------------------------------- shared state

#[derive(Default)]
struct Stats {
    out_frames: AtomicU64,
    in_frames: AtomicU64,
    in_short: AtomicU64,
    out_urbs: AtomicU64,
    in_urbs: AtomicU64,
    out_packets: AtomicU64,
    in_packets: AtomicU64,
    late_max_us: AtomicU64,
    late_sum_us: AtomicU64,
    late_count: AtomicU64,
    out_peak: AtomicU64,
    unlinks: AtomicU64,
}

struct Shared {
    ring: Mutex<VecDeque<i16>>,
    alts: Mutex<[u8; 3]>,
    stats: Stats,
    sine: bool,
}

impl Shared {
    fn alt(&self, iface: u8) -> u8 {
        self.alts.lock().unwrap().get(iface as usize).copied().unwrap_or(0)
    }
    fn set_alt(&self, iface: u8, alt: u8) {
        if let Some(a) = self.alts.lock().unwrap().get_mut(iface as usize) {
            *a = alt;
        }
        // A recording that starts gets fresh audio, not a stale backlog.
        if alt == 1 || (iface == 1 && alt == 0) {
            self.ring.lock().unwrap().clear();
        }
    }
}

fn log(msg: &str) {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    println!("{t:9.3}  {msg}");
}

// ---------------------------------------------------------------- protocol

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn device_record() -> Vec<u8> {
    let mut d = vec![0u8; 256];
    let path = b"/sys/devices/mixercraft/virtual-cable";
    d[..path.len()].copy_from_slice(path);
    let mut busid = [0u8; 32];
    busid[..BUSID.len()].copy_from_slice(BUSID.as_bytes());
    d.extend_from_slice(&busid);
    d.extend_from_slice(&BUSNUM.to_be_bytes());
    d.extend_from_slice(&DEVNUM.to_be_bytes());
    d.extend_from_slice(&SPEED_FULL.to_be_bytes());
    d.extend_from_slice(&0x1209u16.to_be_bytes());
    d.extend_from_slice(&0x0001u16.to_be_bytes());
    d.extend_from_slice(&0x0100u16.to_be_bytes());
    d.extend_from_slice(&[0, 0, 0, 1, 1, 3]); // class, sub, proto, cfg value, num cfgs, num ifaces
    d
}

fn op_header(code: u16, status: u32) -> Vec<u8> {
    let mut h = Vec::new();
    h.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    h.extend_from_slice(&code.to_be_bytes());
    h.extend_from_slice(&status.to_be_bytes());
    h
}

fn handle_connection(mut sock: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    sock.set_nodelay(true)?;
    let mut op = [0u8; 8];
    sock.read_exact(&mut op)?;
    let code = u16::from_be_bytes([op[2], op[3]]);
    match code {
        OP_REQ_DEVLIST => {
            log("OP_REQ_DEVLIST");
            let mut r = op_header(OP_REP_DEVLIST, 0);
            r.extend_from_slice(&1u32.to_be_bytes());
            r.extend_from_slice(&device_record());
            for (class, sub) in [(1u8, 1u8), (1, 2), (1, 2)] {
                r.extend_from_slice(&[class, sub, 0, 0]);
            }
            sock.write_all(&r)?;
            Ok(())
        }
        OP_REQ_IMPORT => {
            let mut busid = [0u8; 32];
            sock.read_exact(&mut busid)?;
            let wanted = String::from_utf8_lossy(&busid).trim_end_matches('\0').to_string();
            log(&format!("OP_REQ_IMPORT {wanted}"));
            if wanted != BUSID {
                sock.write_all(&op_header(OP_REP_IMPORT, 1))?;
                return Ok(());
            }
            let mut r = op_header(OP_REP_IMPORT, 0);
            r.extend_from_slice(&device_record());
            sock.write_all(&r)?;
            urb_loop(sock, shared)
        }
        other => {
            log(&format!("unknown op {other:#06x}"));
            Ok(())
        }
    }
}

enum Msg {
    Submit { seq: u32, ep: u32, dir_in: bool, setup: [u8; 8], data: Vec<u8>, packets: Vec<(u32, u32)>, len: u32 },
    Unlink { seq: u32, target: u32 },
}

fn urb_loop(sock: TcpStream, shared: Arc<Shared>) -> std::io::Result<()> {
    log("device attached; serving URBs");
    let (tx, rx) = mpsc::channel::<Msg>();
    let writer_sock = sock.try_clone()?;
    let writer_shared = Arc::clone(&shared);
    let done = Arc::new(AtomicBool::new(false));
    let writer_done = Arc::clone(&done);
    let writer = std::thread::spawn(move || {
        let r = writer_loop(writer_sock, &rx, &writer_shared);
        writer_done.store(true, Ordering::Relaxed);
        r
    });
    let mut sock = sock;
    let mut hdr = [0u8; 48];
    while !done.load(Ordering::Relaxed) {
        if sock.read_exact(&mut hdr).is_err() {
            break;
        }
        let command = be32(&hdr, 0);
        let seq = be32(&hdr, 4);
        let dir_in = be32(&hdr, 12) == 1;
        let ep = be32(&hdr, 16);
        match command {
            CMD_SUBMIT => {
                let len = be32(&hdr, 24);
                let np = be32(&hdr, 32) as i32;
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&hdr[40..48]);
                let mut data = Vec::new();
                if !dir_in && len > 0 {
                    data = vec![0u8; len as usize];
                    sock.read_exact(&mut data)?;
                }
                let mut packets = Vec::new();
                if np > 0 {
                    let mut iso = vec![0u8; np as usize * 16];
                    sock.read_exact(&mut iso)?;
                    for p in iso.chunks(16) {
                        packets.push((be32(p, 0), be32(p, 4)));
                    }
                }
                if tx.send(Msg::Submit { seq, ep, dir_in, setup, data, packets, len }).is_err() {
                    break;
                }
            }
            CMD_UNLINK => {
                let target = be32(&hdr, 20);
                if tx.send(Msg::Unlink { seq, target }).is_err() {
                    break;
                }
            }
            other => {
                log(&format!("unknown command {other}"));
                break;
            }
        }
    }
    drop(tx);
    let _ = writer.join();
    log("device detached");
    Ok(())
}

struct Pending {
    seq: u32,
    ep: u32,
    data: Vec<u8>,
    packets: Vec<(u32, u32)>,
    due: Instant,
}

fn ret_submit(seq: u32, status: i32, actual: u32, start_frame: u32, packets: i32, payload: &[u8], iso: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(48 + payload.len() + iso.len());
    for v in [RET_SUBMIT, seq, 0, 0, 0] {
        r.extend_from_slice(&v.to_be_bytes());
    }
    r.extend_from_slice(&status.to_be_bytes());
    r.extend_from_slice(&actual.to_be_bytes());
    r.extend_from_slice(&start_frame.to_be_bytes());
    r.extend_from_slice(&packets.to_be_bytes());
    r.extend_from_slice(&0i32.to_be_bytes()); // error_count
    r.extend_from_slice(&[0u8; 8]);
    r.extend_from_slice(payload);
    r.extend_from_slice(iso);
    r
}

fn ret_unlink(seq: u32, status: i32) -> Vec<u8> {
    let mut r = Vec::with_capacity(48);
    for v in [RET_UNLINK, seq, 0, 0, 0] {
        r.extend_from_slice(&v.to_be_bytes());
    }
    r.extend_from_slice(&status.to_be_bytes());
    r.extend_from_slice(&[0u8; 24]);
    r
}

fn writer_loop(mut sock: TcpStream, rx: &mpsc::Receiver<Msg>, shared: &Shared) -> std::io::Result<()> {
    let t0 = Instant::now();
    let mut heap: BinaryHeap<Reverse<(Instant, u32)>> = BinaryHeap::new();
    let mut pending: HashMap<u32, Pending> = HashMap::new();
    let mut next_due: [Option<Instant>; 3] = [None; 3];
    let mut phase = 0.0f64;
    loop {
        loop {
            match rx.try_recv() {
                Ok(Msg::Submit { seq, ep, dir_in, setup, data, packets, len }) => {
                    if ep == 0 {
                        let r = match control(setup, shared) {
                            Ok(v) if dir_in => ret_submit(seq, 0, v.len() as u32, 0, 0, &v, &[]),
                            Ok(_) => ret_submit(seq, 0, len, 0, 0, &[], &[]),
                            Err(status) => ret_submit(seq, status, 0, 0, 0, &[], &[]),
                        };
                        sock.write_all(&r)?;
                        continue;
                    }
                    if packets.is_empty() || ep > 2 {
                        log(&format!("unexpected transfer on ep {ep} (dir_in {dir_in}, {len} bytes)"));
                        sock.write_all(&ret_submit(seq, EPIPE, 0, 0, 0, &[], &[]))?;
                        continue;
                    }
                    // Real-time pacing: one packet per 1 ms frame, back to
                    // back while the host keeps the queue filled.
                    let now = Instant::now();
                    let slot = &mut next_due[ep as usize];
                    let start = match *slot {
                        Some(t) if t + Duration::from_millis(4) >= now => t,
                        _ => now,
                    };
                    let due = start + Duration::from_millis(packets.len() as u64);
                    *slot = Some(due);
                    heap.push(Reverse((due, seq)));
                    pending.insert(seq, Pending { seq, ep, data, packets, due });
                }
                Ok(Msg::Unlink { seq, target }) => {
                    shared.stats.unlinks.fetch_add(1, Ordering::Relaxed);
                    let status = if pending.remove(&target).is_some() { ECONNRESET } else { 0 };
                    sock.write_all(&ret_unlink(seq, status))?;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        let now = Instant::now();
        while let Some(&Reverse((due, seq))) = heap.peek() {
            if due > now {
                break;
            }
            heap.pop();
            let Some(p) = pending.remove(&seq) else { continue };
            let late = now.saturating_duration_since(p.due).as_micros() as u64;
            let s = &shared.stats;
            s.late_max_us.fetch_max(late, Ordering::Relaxed);
            s.late_sum_us.fetch_add(late, Ordering::Relaxed);
            s.late_count.fetch_add(1, Ordering::Relaxed);
            let frame = (p.due.duration_since(t0).as_millis() as u32).wrapping_sub(p.packets.len() as u32);
            let r = if p.ep == 1 { complete_out(&p, shared, frame) } else { complete_in(&p, shared, frame, &mut phase) };
            sock.write_all(&r)?;
        }

        let wait = heap
            .peek()
            .map(|Reverse((d, _))| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(1))
            .min(Duration::from_millis(1));
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

fn complete_out(p: &Pending, shared: &Shared, frame: u32) -> Vec<u8> {
    let s = &shared.stats;
    s.out_urbs.fetch_add(1, Ordering::Relaxed);
    s.out_packets.fetch_add(p.packets.len() as u64, Ordering::Relaxed);
    let mut iso = Vec::with_capacity(p.packets.len() * 16);
    let mut actual = 0u32;
    let mut ring = shared.ring.lock().unwrap();
    let mut peak = 0u64;
    for &(offset, length) in &p.packets {
        let bytes = p.data.get(offset as usize..(offset + length) as usize).unwrap_or(&[]);
        for pair in bytes.chunks_exact(2) {
            let v = i16::from_le_bytes([pair[0], pair[1]]);
            peak = peak.max(u64::from(v.unsigned_abs()));
            ring.push_back(v);
        }
        s.out_frames.fetch_add((bytes.len() / FRAME_BYTES) as u64, Ordering::Relaxed);
        actual += length;
        for v in [offset, length, length, 0] {
            iso.extend_from_slice(&v.to_be_bytes());
        }
    }
    let excess = ring.len().saturating_sub(RING_MAX_FRAMES * 2);
    ring.drain(..excess);
    drop(ring);
    s.out_peak.fetch_max(peak, Ordering::Relaxed);
    ret_submit(p.seq, 0, actual, frame, p.packets.len() as i32, &[], &iso)
}

fn complete_in(p: &Pending, shared: &Shared, frame: u32, phase: &mut f64) -> Vec<u8> {
    let s = &shared.stats;
    s.in_urbs.fetch_add(1, Ordering::Relaxed);
    s.in_packets.fetch_add(p.packets.len() as u64, Ordering::Relaxed);
    let mut payload = Vec::new();
    let mut iso = Vec::with_capacity(p.packets.len() * 16);
    let mut ring = shared.ring.lock().unwrap();
    for &(offset, length) in &p.packets {
        let frames = FRAMES_PER_MS.min(length as usize / FRAME_BYTES);
        for _ in 0..frames {
            let (l, r) = if shared.sine {
                let v = (phase.sin() * 8192.0) as i16;
                *phase += 2.0 * std::f64::consts::PI * 440.0 / f64::from(RATE);
                (v, v)
            } else if ring.len() >= 2 {
                (ring.pop_front().unwrap_or(0), ring.pop_front().unwrap_or(0))
            } else {
                s.in_short.fetch_add(1, Ordering::Relaxed);
                (0, 0)
            };
            payload.extend_from_slice(&l.to_le_bytes());
            payload.extend_from_slice(&r.to_le_bytes());
        }
        s.in_frames.fetch_add(frames as u64, Ordering::Relaxed);
        let actual = (frames * FRAME_BYTES) as u32;
        for v in [offset, length, actual, 0] {
            iso.extend_from_slice(&v.to_be_bytes());
        }
    }
    drop(ring);
    ret_submit(p.seq, 0, payload.len() as u32, frame, p.packets.len() as i32, &payload, &iso)
}

fn report(shared: &Shared) {
    let s = &shared.stats;
    let mut last = Instant::now();
    let mut prev = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();
        let now = (
            s.out_frames.load(Ordering::Relaxed),
            s.in_frames.load(Ordering::Relaxed),
            s.out_urbs.load(Ordering::Relaxed),
            s.out_packets.load(Ordering::Relaxed),
            s.in_urbs.load(Ordering::Relaxed),
            s.in_packets.load(Ordering::Relaxed),
        );
        if now == prev {
            continue;
        }
        let per_urb = |p: u64, u: u64| if u == 0 { 0.0 } else { p as f64 / u as f64 };
        let count = s.late_count.swap(0, Ordering::Relaxed).max(1);
        log(&format!(
            "out {:7.0} fr/s ({:.1} pkt/urb)  in {:7.0} fr/s ({:.1} pkt/urb)  ring {:5} fr  in-short {}  late avg {} us max {} us  out peak {}  unlinks {}  alts {:?}",
            (now.0 - prev.0) as f64 / dt,
            per_urb(now.3 - prev.3, now.2 - prev.2),
            (now.1 - prev.1) as f64 / dt,
            per_urb(now.5 - prev.5, now.4 - prev.4),
            shared.ring.lock().unwrap().len() / 2,
            s.in_short.swap(0, Ordering::Relaxed),
            s.late_sum_us.swap(0, Ordering::Relaxed) / count,
            s.late_max_us.swap(0, Ordering::Relaxed),
            s.out_peak.swap(0, Ordering::Relaxed),
            s.unlinks.load(Ordering::Relaxed),
            *shared.alts.lock().unwrap(),
        ));
        prev = now;
    }
}

fn serve(sine: bool) -> std::io::Result<()> {
    let shared = Arc::new(Shared {
        ring: Mutex::new(VecDeque::new()),
        alts: Mutex::new([0; 3]),
        stats: Stats::default(),
        sine,
    });
    let listener = TcpListener::bind("127.0.0.1:3240")?;
    log(&format!("USB/IP server on 127.0.0.1:3240, busid {BUSID} ({})", if sine { "440 Hz tone" } else { "loopback" }));
    let reporter = Arc::clone(&shared);
    std::thread::spawn(move || report(&reporter));
    for conn in listener.incoming() {
        let sock = conn?;
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(sock, shared) {
                log(&format!("connection ended: {e}"));
            }
        });
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    match args.first().map(String::as_str) {
        Some("serve") => {
            if let Err(e) = serve(args.iter().any(|a| a == "--sine")) {
                eprintln!("server failed: {e}");
                std::process::exit(1);
            }
        }
        Some("latency") => latency::run(
            &arg("--out").unwrap_or_else(|| "Virtual Cable".into()),
            &arg("--in").unwrap_or_else(|| "Virtual Cable".into()),
            arg("--secs").and_then(|s| s.parse().ok()).unwrap_or(20),
        ),
        Some("devices") => latency::list(),
        _ => eprintln!("usage: usbip-audio-spike serve [--sine] | latency [--out NAME] [--in NAME] [--secs N] | devices"),
    }
}
