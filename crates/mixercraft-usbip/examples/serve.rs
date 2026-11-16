//! Serves one virtual speaker for a while (development tool, Windows):
//! attaches it with usbip-win2, keeps the default devices unchanged, then
//! unplugs it.
//!
//! ```text
//! cargo run -p mixercraft-usbip --example serve -- <port> "<product>" <serial> [seconds]
//! ```

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mixercraft_usbip::{AudioPort, DeviceSpec, Direction, Server};

struct Discard;
impl AudioPort for Discard {}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let port: u16 = args.first().and_then(|p| p.parse().ok()).unwrap_or(3251);
    let product = args.get(1).cloned().unwrap_or_else(|| "MixerCraft Example".into());
    let serial = args.get(2).cloned().unwrap_or_else(|| "EXAMPLE1".into());
    let seconds: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(15);
    let defaults = mixercraft_winaudio::snapshot().ok();
    let server = Server::start(SocketAddr::from(([127, 0, 0, 1], port))).expect("bind");
    let handle = server.add(DeviceSpec { product, serial, direction: Direction::Speaker }, Box::new(Discard));
    println!("serving {} on {}", handle.busid(), server.addr());
    let status = std::process::Command::new(r"C:\Program Files\USBip\usbip.exe")
        .args(["-t", &port.to_string(), "attach", "--once", "-r", "127.0.0.1", "-b", handle.busid()])
        .status();
    println!("attach: {status:?}");
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        if let Some(d) = &defaults
            && let Ok(n) = mixercraft_winaudio::restore(d)
            && n > 0
        {
            println!("restored {n} default device(s)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
