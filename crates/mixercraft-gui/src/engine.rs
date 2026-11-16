//! Background connection to the engine.
//!
//! One thread owns the IPC client. It polls meters at ~30 Hz and status at
//! 2 Hz, forwards user commands (coalescing fader moves so a fast drag sends
//! only the latest value), reconnects automatically, and can start the
//! engine. It never touches Slint types; the UI receives [`Update`]s.

use std::collections::HashSet;
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use mixercraft_ipc::{Client, DeviceChoice, DeviceSummary, EngineStatus, IpcError, Request, Response};

use crate::meters::Channel;

const METER_INTERVAL: Duration = Duration::from_millis(33);
const STATUS_INTERVAL: Duration = Duration::from_millis(500);
const RECONNECT_INTERVAL: Duration = Duration::from_millis(1000);
/// How often the device list is refreshed (plugging in a headset).
const DEVICES_INTERVAL: Duration = Duration::from_secs(5);
/// Longest wait for a stopping engine to exit before a restart.
const RESTART_WAIT: Duration = Duration::from_secs(10);

/// A user action to forward to the engine.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    StripGain(u16, f32),
    BusGain(u16, f32),
    StripMute(u16, bool),
    BusMute(u16, bool),
    StripSolo(u16, bool),
    Route {
        strip: u16,
        bus: u16,
        on: bool,
    },
    StripFx(u16, mixercraft_ipc::StripFx),
    BusKeepAwake(u16, mixercraft_ipc::KeepAwake),
    StripSource(u16, DeviceChoice),
    BusSink(u16, DeviceChoice),
    AddStrip,
    AddBus,
    RemoveStrip(u16),
    RemoveBus(u16),
    RenameStrip(u16, String),
    RenameBus(u16, String),
    /// Stores engine-wide settings (they apply at the next engine start).
    EngineSettings(mixercraft_ipc::EngineSettings),
    StartEngine,
    StopEngine,
    /// Stops the engine, waits until it has exited and starts it again.
    RestartEngine,
}

/// Something the UI should show.
#[derive(Debug)]
pub enum Update {
    /// No engine reachable; `hint` explains what to do.
    Disconnected { hint: String },
    /// Fresh engine status.
    Status(Box<EngineStatus>),
    /// Meter display values per strip and bus: `[left, right, left peak, right peak]`.
    Meters { strips: Vec<[f32; 4]>, buses: Vec<[f32; 4]> },
    /// Devices the engine can open.
    Devices(Vec<DeviceSummary>),
    /// The engine refused a request; show why.
    Notice(String),
}

/// Keeps only the last fader value per target; other commands keep order.
#[must_use]
pub fn coalesce(cmds: Vec<Cmd>) -> Vec<Cmd> {
    let mut seen = HashSet::new();
    let mut out: Vec<Cmd> = cmds
        .into_iter()
        .rev()
        .filter(|c| match c {
            Cmd::StripGain(i, _) => seen.insert((false, *i)),
            Cmd::BusGain(i, _) => seen.insert((true, *i)),
            // Knob drags send the whole processing state; the last one wins.
            Cmd::StripFx(i, _) => seen.insert((false, 1000 + *i)),
            _ => true,
        })
        .collect();
    out.reverse();
    out
}

fn request_for(cmd: &Cmd) -> Option<Request> {
    Some(match *cmd {
        Cmd::StripGain(strip, db) => Request::SetStripGain { strip, db },
        Cmd::BusGain(bus, db) => Request::SetBusGain { bus, db },
        Cmd::StripMute(strip, mute) => Request::SetStripMute { strip, mute },
        Cmd::BusMute(bus, mute) => Request::SetBusMute { bus, mute },
        Cmd::StripSolo(strip, solo) => Request::SetStripSolo { strip, solo },
        Cmd::Route { strip, bus, on } => Request::SetRoute { strip, bus, enabled: on },
        Cmd::StripFx(strip, fx) => Request::SetStripFx { strip, fx },
        Cmd::BusKeepAwake(bus, keep_awake) => Request::SetBusKeepAwake { bus, keep_awake },
        Cmd::StripSource(strip, ref choice) => Request::SetStripSource { strip, choice: choice.clone() },
        Cmd::BusSink(bus, ref choice) => Request::SetBusSink { bus, choice: choice.clone() },
        Cmd::AddStrip => Request::AddStrip { name: String::new() },
        Cmd::AddBus => Request::AddBus { name: String::new() },
        Cmd::RemoveStrip(strip) => Request::RemoveStrip { strip },
        Cmd::RemoveBus(bus) => Request::RemoveBus { bus },
        Cmd::RenameStrip(strip, ref name) => Request::RenameStrip { strip, name: name.clone() },
        Cmd::RenameBus(bus, ref name) => Request::RenameBus { bus, name: name.clone() },
        Cmd::EngineSettings(ref settings) => Request::SetEngineSettings { settings: settings.clone() },
        Cmd::StopEngine => Request::Shutdown,
        Cmd::StartEngine | Cmd::RestartEngine => return None,
    })
}

/// Starts `mixercraftd` from the directory of this executable, with the
/// configuration file the last engine used (if known).
fn start_engine(config: Option<&str>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let engine = exe.with_file_name(format!("mixercraftd{}", std::env::consts::EXE_SUFFIX));
    if !engine.exists() {
        return Err(format!("mixercraftd not found next to the app ({})", engine.display()));
    }
    let mut cmd = Command::new(&engine);
    if let Some(config) = config {
        cmd.arg("--config").arg(config);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("cannot start {}: {e}", engine.display()))
}

/// Runs the connection loop until the command channel closes.
pub fn run(rx: &Receiver<Cmd>, deliver: &dyn Fn(Update)) {
    let mut client: Option<Client> = None;
    let mut hint = String::from("Start it here, or run `mixercraft autostart enable` to start it at sign-in.");
    let mut announced_disconnect = false;
    let mut strips: Vec<[Channel; 2]> = Vec::new();
    let mut buses: Vec<[Channel; 2]> = Vec::new();
    let mut last_meter = Instant::now();
    let mut next_meter = Instant::now();
    let mut next_status = Instant::now();
    let mut next_devices = Instant::now();
    let mut tried_auto_start = false;
    // The configuration file of the engine last connected to, so a restart
    // keeps using it.
    let mut config_path: Option<String> = None;

    loop {
        let Some(c) = client.as_mut() else {
            match Client::connect("mixercraft-gui") {
                Ok(c) => {
                    client = Some(c);
                    announced_disconnect = false;
                    tried_auto_start = true;
                    next_status = Instant::now();
                    next_devices = Instant::now();
                    continue;
                }
                // No engine when the app opens: start it, once. An engine
                // that stops later (e.g. stopped from the tray) is not
                // restarted behind the user's back.
                Err(_) if !tried_auto_start => {
                    tried_auto_start = true;
                    hint = match start_engine(config_path.as_deref()) {
                        Ok(()) => "Starting the engine…".into(),
                        Err(e) => e,
                    };
                    deliver(Update::Disconnected { hint: hint.clone() });
                    announced_disconnect = true;
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                Err(_) if !announced_disconnect => {
                    deliver(Update::Disconnected { hint: hint.clone() });
                    announced_disconnect = true;
                }
                Err(_) => {}
            }
            match rx.recv_timeout(RECONNECT_INTERVAL) {
                Ok(Cmd::StartEngine | Cmd::RestartEngine) => {
                    hint = match start_engine(config_path.as_deref()) {
                        Ok(()) => "Starting the engine…".into(),
                        Err(e) => e,
                    };
                    deliver(Update::Disconnected { hint: hint.clone() });
                    std::thread::sleep(Duration::from_millis(300));
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        };

        let deadline = next_meter.min(next_status);
        let mut pending = Vec::new();
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(cmd) => {
                pending.push(cmd);
                while let Ok(more) = rx.try_recv() {
                    pending.push(more);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        let mut failed = false;
        let mut restart = false;
        for cmd in coalesce(pending) {
            if cmd == Cmd::RestartEngine {
                restart = true;
                continue;
            }
            let Some(req) = request_for(&cmd) else { continue };
            let reshapes = matches!(cmd, Cmd::AddStrip | Cmd::AddBus | Cmd::RemoveStrip(_) | Cmd::RemoveBus(_));
            match c.request(&req) {
                Ok(_) => {
                    if reshapes {
                        next_status = Instant::now(); // show the new layout at once
                    }
                }
                Err(IpcError::Remote { message, .. }) => deliver(Update::Notice(message)),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }

        if restart && !failed {
            let _ = c.request(&Request::Shutdown);
            client = None;
            deliver(Update::Disconnected { hint: "Restarting the engine…".into() });
            // The endpoint stays reachable until the old process has exited
            // (and released its devices); only then start the new one.
            let deadline = Instant::now() + RESTART_WAIT;
            while Instant::now() < deadline && Client::connect("mixercraft-gui").is_ok() {
                std::thread::sleep(Duration::from_millis(200));
            }
            hint = match start_engine(config_path.as_deref()) {
                Ok(()) => "Starting the engine…".into(),
                Err(e) => e,
            };
            deliver(Update::Disconnected { hint: hint.clone() });
            announced_disconnect = true;
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }

        let now = Instant::now();
        if !failed && now >= next_meter {
            match c.request(&Request::Meters) {
                Ok(Response::Meters(m)) => {
                    let dt = now - last_meter;
                    last_meter = now;
                    let s = advance(&mut strips, &m.strips, dt);
                    let b = advance(&mut buses, &m.buses, dt);
                    deliver(Update::Meters { strips: s, buses: b });
                }
                Ok(_) | Err(IpcError::Remote { .. }) => {}
                Err(_) => failed = true,
            }
            next_meter = (next_meter + METER_INTERVAL).max(now);
        }
        if !failed && now >= next_status {
            match c.request(&Request::Status) {
                Ok(Response::Status(s)) => {
                    if !s.config_path.is_empty() {
                        config_path = Some(s.config_path.clone());
                    }
                    deliver(Update::Status(s));
                }
                Ok(_) | Err(IpcError::Remote { .. }) => {}
                Err(_) => failed = true,
            }
            next_status = now + STATUS_INTERVAL;
        }
        if !failed && now >= next_devices {
            match c.request(&Request::Devices) {
                Ok(Response::Devices { devices }) => deliver(Update::Devices(devices)),
                Ok(_) | Err(IpcError::Remote { .. }) => {}
                Err(_) => failed = true,
            }
            next_devices = now + DEVICES_INTERVAL;
        }
        if failed {
            client = None;
            hint = "The connection to the engine was lost. Reconnecting…".into();
        }
    }
}

fn advance(state: &mut Vec<[Channel; 2]>, levels: &[Vec<mixercraft_ipc::Level>], dt: Duration) -> Vec<[f32; 4]> {
    state.resize(levels.len(), [Channel::default(); 2]);
    state
        .iter_mut()
        .zip(levels)
        .map(|(chs, lv)| {
            let l = lv.first().map_or(0.0, |x| x.peak);
            // Mono sources show the same level on both meters.
            let r = lv.get(1).map_or(l, |x| x.peak);
            chs[0].update(l, dt);
            chs[1].update(r, dt);
            [chs[0].level, chs[1].level, chs[0].peak, chs[1].peak]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_keeps_last_gain_per_target_and_order_of_others() {
        let cmds = vec![
            Cmd::StripGain(0, -10.0),
            Cmd::StripMute(0, true),
            Cmd::StripGain(1, -3.0),
            Cmd::StripGain(0, -6.0),
            Cmd::BusGain(0, 1.0),
            Cmd::BusGain(0, 2.0),
        ];
        assert_eq!(
            coalesce(cmds),
            vec![Cmd::StripMute(0, true), Cmd::StripGain(1, -3.0), Cmd::StripGain(0, -6.0), Cmd::BusGain(0, 2.0)]
        );
    }
}
