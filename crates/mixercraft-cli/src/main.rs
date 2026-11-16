//! `mixercraft` — command-line control of a running MixerCraft engine.

use anyhow::Context;
use clap::{Parser, Subcommand};
use mixercraft_ipc::{Client, Request, Response};
use mixercraft_platform::capabilities::{self, Feature};

#[derive(Debug, Parser)]
#[command(name = "mixercraft", version, about = "Control a running MixerCraft engine")]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Engine health and diagnostics.
    Status {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Current peak levels (dBFS) of all strips and buses.
    Meters,
    /// Audio devices visible to the engine.
    Devices,
    /// Set a strip fader (dB).
    Gain {
        strip: u16,
        #[arg(allow_negative_numbers = true)]
        db: f32,
    },
    /// Set a bus fader (dB).
    BusGain {
        bus: u16,
        #[arg(allow_negative_numbers = true)]
        db: f32,
    },
    /// Mute or unmute a strip.
    Mute { strip: u16, state: OnOff },
    /// Mute or unmute a bus.
    BusMute { bus: u16, state: OnOff },
    /// Solo or unsolo a strip.
    Solo { strip: u16, state: OnOff },
    /// Connect or disconnect a strip from a bus.
    Route { strip: u16, bus: u16, state: OnOff },
    /// Platform feature support of this build (no engine needed).
    Capabilities,
    /// Stop the engine.
    Shutdown,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl From<OnOff> for bool {
    fn from(v: OnOff) -> bool {
        matches!(v, OnOff::On)
    }
}

fn db(x: f32) -> String {
    if x <= 0.0 { "  -inf".into() } else { format!("{:6.1}", 20.0 * x.log10()) }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if let Cmd::Capabilities = args.command {
        for f in Feature::ALL {
            let (s, why) = capabilities::support(f);
            println!("{:28} {:12} {why}", f.name(), format!("{s:?}"));
        }
        return Ok(());
    }
    let mut c =
        Client::connect("mixercraft-cli").context("cannot reach the MixerCraft engine (is mixercraftd running?)")?;
    let req = match args.command {
        Cmd::Status { .. } => Request::Status,
        Cmd::Meters => Request::Meters,
        Cmd::Devices => Request::Devices,
        Cmd::Gain { strip, db } => Request::SetStripGain { strip, db },
        Cmd::BusGain { bus, db } => Request::SetBusGain { bus, db },
        Cmd::Mute { strip, state } => Request::SetStripMute { strip, mute: state.into() },
        Cmd::BusMute { bus, state } => Request::SetBusMute { bus, mute: state.into() },
        Cmd::Solo { strip, state } => Request::SetStripSolo { strip, solo: state.into() },
        Cmd::Route { strip, bus, state } => Request::SetRoute { strip, bus, enabled: state.into() },
        Cmd::Shutdown => Request::Shutdown,
        Cmd::Capabilities => unreachable!("handled above"),
    };
    let json = matches!(args.command, Cmd::Status { json: true });
    match c.request(&req)? {
        Response::Status(s) if json => println!("{}", serde_json::to_string_pretty(&s)?),
        Response::Status(s) => {
            println!("engine {}  {:?}  up {:.0}s  backend {}", s.engine_version, s.state, s.uptime_secs, s.backend);
            println!(
                "clock {}  {} Hz  load {:.1}% (peak {:.1}%)  overloads {}  skipped {}",
                s.clock_source,
                s.sample_rate,
                f64::from(s.load_permille) / 10.0,
                f64::from(s.peak_load_permille) / 10.0,
                s.overloads,
                s.skipped_cycles
            );
            println!("devices:");
            for d in &s.devices {
                print!("  {:7} {:10} {} ({} Hz, {} ch)", d.role, d.state, d.name, d.sample_rate, d.channels);
                if let Some(f) = d.fifo {
                    print!(
                        "  fifo {}/{}  xruns {}u/{}o  gaps {}  drift {:+.1} ppm",
                        f.fill_frames, f.target_frames, f.underruns, f.overruns, f.gap_reads, f.drift_ppm
                    );
                }
                println!();
                if let Some(e) = &d.error {
                    println!("          last error: {e}");
                }
            }
            println!("strips:");
            for (i, st) in s.strips.iter().enumerate() {
                println!(
                    "  {i}: {:16} {:>6.1} dB{}{}  → {:?}  [{}]",
                    st.name,
                    st.gain_db,
                    if st.mute { " M" } else { "" },
                    if st.solo { " S" } else { "" },
                    st.routes,
                    st.source
                );
            }
            println!("buses:");
            for (i, b) in s.buses.iter().enumerate() {
                println!(
                    "  {i}: {:3} {:12} {:>6.1} dB{}  [{}]",
                    b.id,
                    b.name,
                    b.gain_db,
                    if b.mute { " M" } else { "" },
                    b.sink
                );
            }
            if !s.recovery_events.is_empty() {
                println!("recovery events:");
                for e in s.recovery_events.iter().rev().take(10) {
                    println!("  {:>8.1}s {} {}: {}", e.at_secs, e.device, e.event, e.detail);
                }
            }
        }
        Response::Meters(m) => {
            for (i, ch) in m.strips.iter().enumerate() {
                let peaks: Vec<String> = ch.iter().map(|l| db(l.peak)).collect();
                println!("strip {i}: {}", peaks.join(" "));
            }
            for (i, ch) in m.buses.iter().enumerate() {
                let peaks: Vec<String> = ch.iter().map(|l| db(l.peak)).collect();
                println!("bus   {i}: {}", peaks.join(" "));
            }
        }
        Response::Devices { devices } => {
            for d in devices {
                let dir = match (d.is_input, d.is_output) {
                    (true, true) => "in/out",
                    (true, false) => "in",
                    _ => "out",
                };
                let def = if d.is_default_input || d.is_default_output { " (default)" } else { "" };
                println!("{dir:6} {}{def}\n       id: {}", d.name, d.id);
            }
        }
        Response::Ok => println!("ok"),
        other => println!("{other:?}"),
    }
    Ok(())
}
