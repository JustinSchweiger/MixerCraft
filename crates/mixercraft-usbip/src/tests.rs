//! Protocol tests with a minimal in-process USB/IP client.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;

#[derive(Default)]
struct Log {
    played: Vec<f32>,
    recorded_calls: usize,
}

struct TestPort(Arc<Mutex<Log>>, f32);

impl AudioPort for TestPort {
    fn played(&mut self, frames: &[f32]) {
        self.0.lock().unwrap().played.extend_from_slice(frames);
    }
    fn record(&mut self, out: &mut [f32]) {
        self.0.lock().unwrap().recorded_calls += 1;
        out.fill(self.1);
    }
}

fn server() -> Server {
    Server::start(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap()
}

fn spec(direction: Direction) -> DeviceSpec {
    DeviceSpec { product: "MixerCraft Test".into(), serial: "T1".into(), direction }
}

struct Client {
    s: TcpStream,
    seq: u32,
}

impl Client {
    fn import(addr: SocketAddr, busid: &str) -> (Self, u32) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut req = vec![0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0];
        let mut b = [0u8; 32];
        b[..busid.len()].copy_from_slice(busid.as_bytes());
        req.extend_from_slice(&b);
        s.write_all(&req).unwrap();
        let mut h = [0u8; 8];
        s.read_exact(&mut h).unwrap();
        let status = u32::from_be_bytes([h[4], h[5], h[6], h[7]]);
        if status == 0 {
            let mut d = [0u8; 312];
            s.read_exact(&mut d).unwrap();
        }
        (Self { s, seq: 0 }, status)
    }

    fn submit(
        &mut self,
        ep: u32,
        dir_in: bool,
        len: u32,
        setup: [u8; 8],
        data: &[u8],
        packets: Option<&[(u32, u32)]>,
    ) -> u32 {
        self.seq += 1;
        let mut m = Vec::new();
        for v in [1, self.seq, (1 << 16) | 1, u32::from(dir_in), ep, 0, len, 0] {
            m.extend_from_slice(&v.to_be_bytes());
        }
        m.extend_from_slice(&(packets.map_or(u32::MAX, |p| p.len() as u32)).to_be_bytes());
        m.extend_from_slice(&1u32.to_be_bytes());
        m.extend_from_slice(&setup);
        m.extend_from_slice(data);
        for &(off, len) in packets.unwrap_or(&[]) {
            for v in [off, len, 0, 0] {
                m.extend_from_slice(&v.to_be_bytes());
            }
        }
        self.s.write_all(&m).unwrap();
        self.seq
    }

    /// Returns (command, seq, status, actual, packets).
    fn ret(&mut self) -> (u32, u32, i32, u32, u32) {
        let mut h = [0u8; 48];
        self.s.read_exact(&mut h).unwrap();
        let w = |i: usize| u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        (w(0), w(4), w(20) as i32, w(24), w(32))
    }

    fn control_in(&mut self, setup: [u8; 8]) -> (i32, Vec<u8>) {
        let len = u32::from(u16::from_le_bytes([setup[6], setup[7]]));
        self.submit(0, true, len, setup, &[], None);
        let (_, _, status, actual, _) = self.ret();
        let mut data = vec![0u8; actual as usize];
        self.s.read_exact(&mut data).unwrap();
        (status, data)
    }

    fn read(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.s.read_exact(&mut v).unwrap();
        v
    }
}

#[test]
fn devlist_and_import() {
    let srv = server();
    let h = srv.add(spec(Direction::Speaker), Box::new(TestPort(Arc::default(), 0.0)));
    let mut s = TcpStream::connect(srv.addr()).unwrap();
    s.write_all(&[0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0]).unwrap();
    let mut r = [0u8; 12];
    s.read_exact(&mut r).unwrap();
    assert_eq!(&r[2..4], &[0x00, 0x05]);
    assert_eq!(u32::from_be_bytes([r[8], r[9], r[10], r[11]]), 1);
    let mut d = [0u8; 312];
    s.read_exact(&mut d).unwrap();
    assert_eq!(&d[256..256 + h.busid().len()], h.busid().as_bytes());

    let (_c, status) = Client::import(srv.addr(), h.busid());
    assert_eq!(status, 0);
    // A second client cannot import the same device.
    let (_c2, status) = Client::import(srv.addr(), h.busid());
    assert_eq!(status, 1);
    let (_c3, status) = Client::import(srv.addr(), "9-9");
    assert_eq!(status, 1);
}

#[test]
fn descriptors_over_the_wire() {
    let srv = server();
    let h = srv.add(spec(Direction::Microphone), Box::new(TestPort(Arc::default(), 0.0)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    let (st, d) = c.control_in([0x80, 6, 0, 1, 0, 0, 18, 0]);
    assert_eq!((st, d), (0, descriptors::device()));
    let (st, d) = c.control_in([0x80, 6, 0, 2, 0, 0, 255, 0]);
    assert_eq!((st, d), (0, descriptors::configuration(Direction::Microphone)));
    let (st, d) = c.control_in([0x80, 6, 2, 3, 0x09, 0x04, 255, 0]);
    assert_eq!(st, 0);
    let text: Vec<u16> = d[2..].chunks(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
    assert_eq!(String::from_utf16(&text).unwrap(), "MixerCraft Test");
    let (st, _) = c.control_in([0x80, 6, 0, 6, 0, 0, 10, 0]);
    assert_eq!(st, -32, "device qualifier stalls (full-speed only)");
}

#[test]
fn speaker_transfers_are_paced_and_delivered() {
    let srv = server();
    let log = Arc::new(Mutex::new(Log::default()));
    let h = srv.add(spec(Direction::Speaker), Box::new(TestPort(Arc::clone(&log), 0.0)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    // Two transfers of 10 x 1 ms, 48 frames of 0x4000 (0.5) each.
    let pk = 48 * 4;
    let data: Vec<u8> = std::iter::repeat_n(0x4000i16.to_le_bytes(), 480 * 2).flatten().collect();
    let packets: Vec<(u32, u32)> = (0..10).map(|i| (i * pk, pk)).collect();
    let t0 = Instant::now();
    c.submit(1, false, data.len() as u32, [0; 8], &data, Some(&packets));
    c.submit(1, false, data.len() as u32, [0; 8], &data, Some(&packets));
    for i in 1..=2u32 {
        let (cmd, _, status, actual, np) = c.ret();
        assert_eq!((cmd, status, actual, np), (3, 0, 1920, 10));
        c.read(10 * 16);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(ms >= f64::from(i * 10) - 1.0, "transfer {i} completed too early: {ms:.1} ms");
    }
    assert!(h.attached());
    let played = log.lock().unwrap().played.clone();
    let host: Vec<f32> = played.iter().copied().filter(|&v| v != 0.0).collect();
    assert_eq!(host.len(), 960 * 2);
    assert!(host.iter().all(|&v| (v - 0.5).abs() < 1e-6));
}

#[test]
fn microphone_transfers_carry_recorded_audio_packed() {
    let srv = server();
    let h = srv.add(spec(Direction::Microphone), Box::new(TestPort(Arc::default(), 0.25)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    let packets: Vec<(u32, u32)> = (0..4).map(|i| (i * 196, 196)).collect();
    c.submit(1, true, 4 * 196, [0; 8], &[], Some(&packets));
    let (cmd, _, status, actual, np) = c.ret();
    assert_eq!((cmd, status, actual, np), (3, 0, 4 * 192, 4));
    let payload = c.read(actual as usize);
    let iso = c.read(4 * 16);
    let first = i16::from_le_bytes([payload[0], payload[1]]);
    assert_eq!(first, (0.25f32 * 32_767.0).round() as i16);
    // Packet 1 keeps its offset; actual length is 48 frames.
    assert_eq!(u32::from_be_bytes([iso[16], iso[17], iso[18], iso[19]]), 196);
    assert_eq!(u32::from_be_bytes([iso[24], iso[25], iso[26], iso[27]]), 192);
}

#[test]
fn unlinked_transfers_are_not_completed() {
    let srv = server();
    let h = srv.add(spec(Direction::Microphone), Box::new(TestPort(Arc::default(), 0.0)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    let packets: Vec<(u32, u32)> = (0..50).map(|i| (i * 196, 196)).collect();
    let target = c.submit(1, true, 50 * 196, [0; 8], &[], Some(&packets));
    c.seq += 1;
    let mut m = Vec::new();
    for v in [2, c.seq, (1 << 16) | 1, 0, 1, target] {
        m.extend_from_slice(&v.to_be_bytes());
    }
    m.extend_from_slice(&[0u8; 24]);
    c.s.write_all(&m).unwrap();
    let (cmd, seq, status, _, _) = c.ret();
    assert_eq!((cmd, seq, status), (4, c.seq, -104));
    c.s.set_read_timeout(Some(Duration::from_millis(120))).unwrap();
    let mut b = [0u8; 1];
    assert!(c.s.read(&mut b).is_err(), "no completion after unlink");
}

#[test]
fn idle_ticker_keeps_the_clock_running_without_a_host() {
    let srv = server();
    let log = Arc::new(Mutex::new(Log::default()));
    let start = Instant::now();
    let _speaker = srv.add(spec(Direction::Speaker), Box::new(TestPort(Arc::clone(&log), 0.0)));
    let mic_log = Arc::new(Mutex::new(Log::default()));
    let _mic = srv.add(spec(Direction::Microphone), Box::new(TestPort(Arc::clone(&mic_log), 0.0)));
    std::thread::sleep(Duration::from_millis(300));
    let frames = log.lock().unwrap().played.len() / CHANNELS;
    // Real time at 48 kHz, delivered in 2 ms chunks. Measured rather than
    // assumed: a loaded CI runner can oversleep the 300 ms considerably.
    let elapsed_ms = start.elapsed().as_millis() as usize;
    let expected = elapsed_ms * FRAMES_PER_MS;
    assert!(
        frames + 30 * FRAMES_PER_MS >= expected && frames <= expected + 2 * FRAMES_PER_MS,
        "{frames} frames of silence in {elapsed_ms} ms"
    );
    assert!(mic_log.lock().unwrap().recorded_calls >= 25);
}

#[test]
fn dropping_the_handle_unplugs_the_device() {
    let srv = server();
    let h = srv.add(spec(Direction::Speaker), Box::new(TestPort(Arc::default(), 0.0)));
    let busid = h.busid().to_owned();
    let (mut c, _) = Client::import(srv.addr(), &busid);
    drop(h);
    let mut b = [0u8; 1];
    assert!(matches!(c.s.read(&mut b), Ok(0) | Err(_)), "connection closed");
    let (_c, status) = Client::import(srv.addr(), &busid);
    assert_eq!(status, 1, "removed device is no longer offered");
}

#[test]
fn streaming_decision_follows_the_explicit_stop_and_the_timeouts() {
    let ms = |n: u64| n * 1_000_000;
    let t = ms(1_000);
    // Speaker: streaming right after a packet, not after IDLE_AFTER without one.
    assert!(host_moves_audio(Direction::Speaker, true, t, 0, t + ms(1)));
    assert!(!host_moves_audio(Direction::Speaker, true, t, 0, t + IDLE_AFTER.as_nanos() as u64));
    // An explicit stop hands over at once, even right after a packet.
    assert!(!host_moves_audio(Direction::Speaker, false, t, 0, t));
    assert!(!host_moves_audio(Direction::Microphone, false, t, t, t));
    // Microphone: the host owns the clock from its first submission, before
    // any packet completed, until MIC_IDLE_AFTER without transfers.
    assert!(host_moves_audio(Direction::Microphone, true, 0, t, t + ms(5)));
    assert!(!host_moves_audio(Direction::Microphone, true, 0, t, t + MIC_IDLE_AFTER.as_nanos() as u64));
}

#[test]
fn explicit_stop_hands_over_to_the_idle_clock_at_once() {
    let srv = server();
    let log = Arc::new(Mutex::new(Log::default()));
    let h = srv.add(spec(Direction::Speaker), Box::new(TestPort(Arc::clone(&log), 0.0)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    let pk = 48 * 4;
    let data = vec![0u8; 10 * pk as usize];
    let packets: Vec<(u32, u32)> = (0..10).map(|i| (i * pk, pk)).collect();
    c.submit(1, false, data.len() as u32, [0; 8], &data, Some(&packets));
    c.ret();
    c.read(10 * 16);
    assert!(h.device.host_active.load(Ordering::Relaxed), "host streams after a transfer");
    // SET_INTERFACE 1, alternate setting 0: the host stops streaming. The
    // server applies it before answering, so this needs no timing: the
    // decision itself is covered by the test above.
    c.submit(0, false, 0, [0x01, 11, 0, 0, 1, 0, 0, 0], &[], None);
    c.ret();
    assert!(!h.device.host_active.load(Ordering::Relaxed), "explicit stop recorded before the reply");
    // And the idle clock does take over (generous bound for slow CI runners).
    let limit = Instant::now() + Duration::from_millis(200);
    while h.host_streaming() && Instant::now() < limit {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!h.host_streaming(), "idle clock takes over");
}

#[test]
fn microphone_idle_clock_stops_as_soon_as_the_host_reads() {
    let srv = server();
    let log = Arc::new(Mutex::new(Log::default()));
    let h = srv.add(spec(Direction::Microphone), Box::new(TestPort(Arc::clone(&log), 0.0)));
    let (mut c, _) = Client::import(srv.addr(), h.busid());
    std::thread::sleep(Duration::from_millis(30));
    let packets: Vec<(u32, u32)> = (0..10).map(|i| (i * 196, 196)).collect();
    c.submit(1, true, 10 * 196, [0; 8], &[], Some(&packets));
    std::thread::sleep(Duration::from_millis(3));
    let before = log.lock().unwrap().recorded_calls;
    let (_, _, _, actual, _) = c.ret();
    c.read(actual as usize + 10 * 16);
    // Only the host's packets read audio while the transfer was pending
    // (one call per 1 ms packet, at most the 10 of this transfer).
    let calls = log.lock().unwrap().recorded_calls - before;
    assert!(calls <= 10, "{calls} reads during one 10-packet transfer");
}
