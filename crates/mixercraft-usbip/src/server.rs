//! USB/IP protocol handling (version 1.1.1, as spoken by Linux `usbipd`).
//!
//! All header fields are big-endian. After `OP_REQ_IMPORT` succeeds the
//! connection carries `USBIP_CMD_SUBMIT` / `USBIP_CMD_UNLINK` and their
//! replies. Isochronous data of IN transfers is sent packed (each packet's
//! `actual_length` bytes back to back), followed by the packet descriptors.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use crate::descriptors::{self, FRAME_BYTES, STREAM_EP};
use crate::{CHANNELS, Device, Direction, FRAMES_PER_MS, Inner, RATE};

const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_DEVLIST: u16 = 0x8005;
const OP_REP_DEVLIST: u16 = 0x0005;
const OP_REQ_IMPORT: u16 = 0x8003;
const OP_REP_IMPORT: u16 = 0x0003;
const CMD_SUBMIT: u32 = 1;
const CMD_UNLINK: u32 = 2;
const RET_SUBMIT: u32 = 3;
const RET_UNLINK: u32 = 4;
const SPEED_FULL: u32 = 2;
const BUSNUM: u32 = 1;
const EPIPE: i32 = -32;
const ECONNRESET: i32 = -104;
/// A host transfer arriving later than this after the previous one ended
/// starts a new timeline instead of continuing back to back.
const RESYNC: Duration = Duration::from_millis(4);
/// Largest transfer accepted from a client (sanity limit).
const MAX_TRANSFER: u32 = 1 << 20;

pub(crate) fn accept_loop(listener: &TcpListener, inner: &Arc<Inner>) {
    for conn in listener.incoming() {
        if inner.stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(sock) = conn else { continue };
        let inner = Arc::clone(inner);
        let _ = std::thread::Builder::new().name("mixercraft-usbip-conn".into()).spawn(move || {
            let _ = handle(sock, &inner);
        });
    }
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn devnum(busid: &str) -> u32 {
    busid.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(1)
}

fn device_record(d: &Device) -> Vec<u8> {
    let mut r = vec![0u8; 256];
    let path = format!("/sys/devices/mixercraft/{}", d.busid);
    r[..path.len().min(255)].copy_from_slice(&path.as_bytes()[..path.len().min(255)]);
    let mut busid = [0u8; 32];
    busid[..d.busid.len().min(31)].copy_from_slice(&d.busid.as_bytes()[..d.busid.len().min(31)]);
    r.extend_from_slice(&busid);
    r.extend_from_slice(&BUSNUM.to_be_bytes());
    r.extend_from_slice(&devnum(&d.busid).to_be_bytes());
    r.extend_from_slice(&SPEED_FULL.to_be_bytes());
    r.extend_from_slice(&descriptors::VENDOR_ID.to_be_bytes());
    r.extend_from_slice(&descriptors::PRODUCT_ID.to_be_bytes());
    r.extend_from_slice(&0x0100u16.to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 1, 1, 2]); // class, subclass, protocol, config value, configs, interfaces
    r
}

fn op_header(code: u16, status: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(8);
    h.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    h.extend_from_slice(&code.to_be_bytes());
    h.extend_from_slice(&status.to_be_bytes());
    h
}

fn handle(mut sock: TcpStream, inner: &Arc<Inner>) -> io::Result<()> {
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut op = [0u8; 8];
    sock.read_exact(&mut op)?;
    match u16::from_be_bytes([op[2], op[3]]) {
        OP_REQ_DEVLIST => {
            let devices: Vec<_> = inner.snapshot().into_iter().filter(|d| !d.removed.load(Ordering::Relaxed)).collect();
            let mut r = op_header(OP_REP_DEVLIST, 0);
            r.extend_from_slice(&(devices.len() as u32).to_be_bytes());
            for d in &devices {
                r.extend_from_slice(&device_record(d));
                r.extend_from_slice(&[1, 1, 0, 0, 1, 2, 0, 0]); // audio control, audio streaming
            }
            sock.write_all(&r)
        }
        OP_REQ_IMPORT => {
            let mut busid = [0u8; 32];
            sock.read_exact(&mut busid)?;
            let wanted = String::from_utf8_lossy(&busid).trim_end_matches('\0').to_owned();
            let device = inner.find(&wanted);
            let Some(device) = device else {
                return sock.write_all(&op_header(OP_REP_IMPORT, 1));
            };
            {
                let mut conn = device.conn.lock().unwrap_or_else(PoisonError::into_inner);
                if conn.is_some() {
                    drop(conn);
                    return sock.write_all(&op_header(OP_REP_IMPORT, 1)); // already attached
                }
                *conn = Some(sock.try_clone()?);
            }
            let mut r = op_header(OP_REP_IMPORT, 0);
            r.extend_from_slice(&device_record(&device));
            sock.write_all(&r)?;
            sock.set_read_timeout(None)?;
            let result = urb_loop(sock, inner, &device);
            device.host_active.store(false, Ordering::Relaxed);
            *device.conn.lock().unwrap_or_else(PoisonError::into_inner) = None;
            result
        }
        _ => Ok(()),
    }
}

/// A request read from the client.
enum Msg {
    Submit { seq: u32, ep: u32, dir_in: bool, setup: [u8; 8], len: u32, data: Vec<u8>, packets: Vec<(u32, u32)> },
    Unlink { seq: u32, target: u32 },
}

fn urb_loop(sock: TcpStream, inner: &Arc<Inner>, device: &Arc<Device>) -> io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let writer_sock = sock.try_clone()?;
    let writer_inner = Arc::clone(inner);
    let writer_device = Arc::clone(device);
    let writer = std::thread::Builder::new()
        .name("mixercraft-usbip-urb".into())
        .spawn(move || Writer::new(writer_sock, &writer_inner, &writer_device).run(&rx))?;
    let result = read_requests(sock, &tx, device);
    drop(tx);
    let _ = writer.join();
    result
}

fn read_requests(mut sock: TcpStream, tx: &mpsc::Sender<Msg>, device: &Device) -> io::Result<()> {
    let mut hdr = [0u8; 48];
    while !device.removed.load(Ordering::Relaxed) {
        sock.read_exact(&mut hdr)?;
        let seq = be32(&hdr, 4);
        let dir_in = be32(&hdr, 12) == 1;
        let ep = be32(&hdr, 16);
        let msg = match be32(&hdr, 0) {
            CMD_SUBMIT => {
                let len = be32(&hdr, 24);
                let np = be32(&hdr, 32) as i32;
                if len > MAX_TRANSFER || np > 1024 {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "transfer too large"));
                }
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
                    packets = iso.chunks(16).map(|p| (be32(p, 0), be32(p, 4))).collect();
                }
                Msg::Submit { seq, ep, dir_in, setup, len, data, packets }
            }
            CMD_UNLINK => Msg::Unlink { seq, target: be32(&hdr, 20) },
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown USB/IP command")),
        };
        if tx.send(msg).is_err() {
            break;
        }
    }
    Ok(())
}

struct Pending {
    data: Vec<u8>,
    packets: Vec<(u32, u32)>,
    due: Instant,
}

/// Answers control transfers at once and completes isochronous transfers
/// in real time (one 1 ms packet per frame, back to back).
struct Writer<'a> {
    sock: TcpStream,
    inner: &'a Inner,
    device: &'a Device,
    heap: BinaryHeap<Reverse<(Instant, u32)>>,
    pending: HashMap<u32, Pending>,
    next_due: Option<Instant>,
    samples: Vec<f32>,
    out: Vec<u8>,
}

impl<'a> Writer<'a> {
    fn new(sock: TcpStream, inner: &'a Inner, device: &'a Device) -> Self {
        Self {
            sock,
            inner,
            device,
            heap: BinaryHeap::new(),
            pending: HashMap::new(),
            next_due: None,
            samples: Vec::new(),
            out: Vec::new(),
        }
    }

    fn run(mut self, rx: &Receiver<Msg>) -> io::Result<()> {
        loop {
            loop {
                match rx.try_recv() {
                    Ok(msg) => self.on_message(msg)?,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            let now = Instant::now();
            while let Some(&Reverse((due, seq))) = self.heap.peek() {
                if due > now {
                    break;
                }
                self.heap.pop();
                if let Some(p) = self.pending.remove(&seq) {
                    self.complete(seq, &p)?;
                }
            }
            let wait = self
                .heap
                .peek()
                .map_or(Duration::from_millis(1), |Reverse((d, _))| d.saturating_duration_since(Instant::now()))
                .min(Duration::from_millis(1));
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
    }

    fn on_message(&mut self, msg: Msg) -> io::Result<()> {
        match msg {
            Msg::Submit { seq, ep: 0, dir_in, setup, len, .. } => {
                let reply = match control(setup, self.device) {
                    Ok(v) if dir_in => ret_submit(seq, 0, v.len() as u32, 0, 0, &v, &[]),
                    Ok(_) => ret_submit(seq, 0, len, 0, 0, &[], &[]),
                    Err(status) => ret_submit(seq, status, 0, 0, 0, &[], &[]),
                };
                self.sock.write_all(&reply)
            }
            Msg::Submit { seq, ep, dir_in, data, packets, .. } => {
                let expected_in = self.device.spec.direction == Direction::Microphone;
                if ep != STREAM_EP || dir_in != expected_in || packets.is_empty() {
                    return self.sock.write_all(&ret_submit(seq, EPIPE, 0, 0, 0, &[], &[]));
                }
                if !self.device.host_active.swap(true, Ordering::Relaxed) {
                    self.device.host_since.store(self.inner.nanos(Instant::now()), Ordering::Relaxed);
                }
                let now = Instant::now();
                let start = match self.next_due {
                    Some(t) if t + RESYNC >= now => t,
                    _ => now,
                };
                let due = start + Duration::from_millis(packets.len() as u64);
                self.next_due = Some(due);
                self.heap.push(Reverse((due, seq)));
                self.pending.insert(seq, Pending { data, packets, due });
                Ok(())
            }
            Msg::Unlink { seq, target } => {
                let status = if self.pending.remove(&target).is_some() { ECONNRESET } else { 0 };
                self.sock.write_all(&ret_unlink(seq, status))
            }
        }
    }

    fn complete(&mut self, seq: u32, p: &Pending) -> io::Result<()> {
        let frame = (self.inner.nanos(p.due) / 1_000_000) as u32;
        let mut iso = Vec::with_capacity(p.packets.len() * 16);
        let reply = match self.device.spec.direction {
            Direction::Speaker => {
                self.samples.clear();
                let mut actual = 0u32;
                for &(offset, length) in &p.packets {
                    let bytes = p.data.get(offset as usize..(offset as usize + length as usize)).unwrap_or(&[]);
                    self.samples
                        .extend(bytes.as_chunks::<2>().0.iter().map(|b| f32::from(i16::from_le_bytes(*b)) / 32_768.0));
                    actual += length;
                    for v in [offset, length, length, 0] {
                        iso.extend_from_slice(&v.to_be_bytes());
                    }
                }
                let samples = &self.samples;
                self.device.with_port(|port| port.played(samples));
                ret_submit(seq, 0, actual, frame, p.packets.len() as i32, &[], &iso)
            }
            Direction::Microphone => {
                let frames: Vec<usize> =
                    p.packets.iter().map(|&(_, len)| FRAMES_PER_MS.min(len as usize / FRAME_BYTES)).collect();
                let total: usize = frames.iter().sum();
                self.samples.clear();
                self.samples.resize(total * CHANNELS, 0.0);
                let samples = &mut self.samples;
                self.device.with_port(|port| port.record(samples));
                self.out.clear();
                for s in &self.samples {
                    let v = (s.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
                    self.out.extend_from_slice(&v.to_le_bytes());
                }
                for (&(offset, length), n) in p.packets.iter().zip(&frames) {
                    for v in [offset, length, (n * FRAME_BYTES) as u32, 0] {
                        iso.extend_from_slice(&v.to_be_bytes());
                    }
                }
                ret_submit(seq, 0, self.out.len() as u32, frame, p.packets.len() as i32, &self.out, &iso)
            }
        };
        self.device.host_until.store(self.inner.nanos(p.due), Ordering::Relaxed);
        self.sock.write_all(&reply)
    }
}

/// Answers a control transfer; `Err` is a USB/IP status (stall = -EPIPE).
fn control(setup: [u8; 8], device: &Device) -> Result<Vec<u8>, i32> {
    let (bm, req) = (setup[0], setup[1]);
    let value = u16::from_le_bytes([setup[2], setup[3]]);
    let len = usize::from(u16::from_le_bytes([setup[6], setup[7]]));
    let reply = |mut v: Vec<u8>| {
        v.truncate(len);
        Ok(v)
    };
    match (bm, req) {
        (0x80, 6) => {
            let d = match (value >> 8) as u8 {
                1 => descriptors::device(),
                2 => descriptors::configuration(device.spec.direction),
                3 => descriptors::string(value as u8, &device.spec.product, &device.spec.serial).ok_or(EPIPE)?,
                _ => return Err(EPIPE), // device qualifier, BOS, ...
            };
            reply(d)
        }
        (0x80..=0x82, 0) => reply(vec![0, 0]), // GET_STATUS
        (0x80, 8) => reply(vec![1]),           // GET_CONFIGURATION
        (0x81, 10) => reply(vec![0]),          // GET_INTERFACE
        (0x01, 11) => {
            // SET_INTERFACE: alternate setting 0 = the host stopped streaming.
            if value == 0 {
                device.host_active.store(false, Ordering::Relaxed);
            }
            Ok(Vec::new())
        }
        (0x00, 9) | (0x00..=0x02, 1 | 3) => Ok(Vec::new()), // SET_CONFIGURATION, features
        (b, _) if b & 0x60 == 0x20 && b & 0x80 != 0 && value >> 8 == 1 => {
            let r = RATE.to_le_bytes();
            reply(vec![r[0], r[1], r[2]]) // sampling frequency
        }
        (b, _) if b & 0x60 == 0x20 => reply(if b & 0x80 != 0 { vec![0; len] } else { Vec::new() }),
        _ => Err(EPIPE),
    }
}

fn ret_submit(
    seq: u32,
    status: i32,
    actual: u32,
    start_frame: u32,
    packets: i32,
    payload: &[u8],
    iso: &[u8],
) -> Vec<u8> {
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
