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
    /// Set a strip's gate, compressor and EQ (unspecified values keep their
    /// current setting).
    Fx {
        strip: u16,
        /// Gate threshold in dBFS (-80 = off).
        #[arg(long, allow_negative_numbers = true)]
        gate: Option<f32>,
        /// Compressor amount 0 (off) to 10.
        #[arg(long)]
        comp: Option<f32>,
        /// Low shelf gain (dB).
        #[arg(long, allow_negative_numbers = true)]
        low: Option<f32>,
        /// Mid gain (dB).
        #[arg(long, allow_negative_numbers = true)]
        mid: Option<f32>,
        /// High shelf gain (dB).
        #[arg(long, allow_negative_numbers = true)]
        high: Option<f32>,
        /// RNNoise speech noise suppression (needs a 48 kHz engine; adds 20 ms).
        #[arg(long)]
        denoise: Option<OnOff>,
    },
    /// Keep speakers with auto-standby awake: after 10 s of silence a bus
    /// plays an inaudible infrasonic tone (default 15 Hz at -45 dBFS).
    KeepAwake {
        bus: u16,
        state: OnOff,
        /// Tone level in dBFS (-70 to -20).
        #[arg(long, allow_negative_numbers = true)]
        level: Option<f32>,
        /// Tone frequency in Hz (5 to 40).
        #[arg(long)]
        frequency: Option<f32>,
    },
    /// Choose the capture device of a strip: "none", "default", a device
    /// name or id (see `mixercraft devices`), or `virtual:<name>` for a
    /// virtual playback device that applications play into.
    Source { strip: u16, device: String },
    /// Choose the playback device of a bus: "none", "default", a device name
    /// or id, or `virtual:<name>` for a virtual recording device that
    /// applications record from.
    Sink { bus: u16, device: String },
    /// Add an input strip (no device, not routed).
    AddStrip {
        #[arg(default_value = "")]
        name: String,
    },
    /// Add an output bus (no device).
    AddBus {
        #[arg(default_value = "")]
        name: String,
    },
    /// Remove a strip; later strips move down by one.
    RemoveStrip { strip: u16 },
    /// Remove a bus (not the master bus).
    RemoveBus { bus: u16 },
    /// Rename a strip.
    RenameStrip { strip: u16, name: String },
    /// Rename a bus.
    RenameBus { bus: u16, name: String },
    /// Platform feature support of this build (no engine needed).
    Capabilities,
    /// Audio devices on this machine, without a running engine.
    LocalDevices,
    /// Print the default engine configuration.
    DefaultConfig,
    /// Start the engine automatically when you sign in.
    Autostart {
        #[command(subcommand)]
        action: AutostartCmd,
    },
    /// Stop the engine.
    Shutdown,
}

#[derive(Debug, Subcommand)]
enum AutostartCmd {
    /// Register the engine to start at sign-in.
    Enable {
        /// Configuration file the engine should use (default: per-user config).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Path of mixercraftd (default: next to this program).
        #[arg(long)]
        engine: Option<std::path::PathBuf>,
        /// Also start the engine now through the same mechanism.
        #[arg(long)]
        now: bool,
    },
    /// Remove the sign-in registration.
    Disable,
    /// Show whether autostart is registered.
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
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
    match &args.command {
        Cmd::Capabilities => {
            for f in Feature::ALL {
                let (s, why) = capabilities::support(f);
                println!("{:28} {:12} {why}", f.name(), format!("{s:?}"));
            }
            return Ok(());
        }
        Cmd::LocalDevices => {
            let audio = mixercraft_platform::AudioSystem::new();
            println!("backend: {}", audio.backend_name());
            for d in audio.devices()? {
                print_device(&d.name, &d.id, d.is_input, d.is_output, d.is_default_input || d.is_default_output);
            }
            return Ok(());
        }
        Cmd::DefaultConfig => {
            print!("{}", mixercraft_config::Config::default().to_toml()?);
            return Ok(());
        }
        Cmd::Autostart { action } => return autostart(action),
        _ => {}
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
        Cmd::Source { strip, ref device } => Request::SetStripSource { strip, choice: device_choice(device) },
        Cmd::Sink { bus, ref device } => Request::SetBusSink { bus, choice: device_choice(device) },
        Cmd::AddStrip { ref name } => Request::AddStrip { name: name.clone() },
        Cmd::AddBus { ref name } => Request::AddBus { name: name.clone() },
        Cmd::RemoveStrip { strip } => Request::RemoveStrip { strip },
        Cmd::RemoveBus { bus } => Request::RemoveBus { bus },
        Cmd::RenameStrip { strip, ref name } => Request::RenameStrip { strip, name: name.clone() },
        Cmd::RenameBus { bus, ref name } => Request::RenameBus { bus, name: name.clone() },
        Cmd::Fx { strip, gate, comp, low, mid, high, denoise } => {
            let Response::Status(s) = c.request(&Request::Status)? else { anyhow::bail!("unexpected answer") };
            let mut fx = s.strips.get(usize::from(strip)).context("no such strip")?.fx;
            fx.gate_threshold_db = gate.unwrap_or(fx.gate_threshold_db);
            fx.comp_amount = comp.unwrap_or(fx.comp_amount);
            fx.eq_low_db = low.unwrap_or(fx.eq_low_db);
            fx.eq_mid_db = mid.unwrap_or(fx.eq_mid_db);
            fx.eq_high_db = high.unwrap_or(fx.eq_high_db);
            if let Some(d) = denoise {
                if d == OnOff::On && !s.noise_suppression_available {
                    anyhow::bail!(
                        "noise suppression needs the engine to run at 48 kHz (it runs at {} Hz)",
                        s.sample_rate
                    );
                }
                fx.noise_suppression = d.into();
            }
            Request::SetStripFx { strip, fx }
        }
        Cmd::KeepAwake { bus, state, level, frequency } => {
            let Response::Status(s) = c.request(&Request::Status)? else { anyhow::bail!("unexpected answer") };
            let mut keep_awake = s.buses.get(usize::from(bus)).context("no such bus")?.keep_awake;
            keep_awake.enabled = state.into();
            keep_awake.level_db = level.unwrap_or(keep_awake.level_db);
            keep_awake.frequency_hz = frequency.unwrap_or(keep_awake.frequency_hz);
            Request::SetBusKeepAwake { bus, keep_awake }
        }
        Cmd::Shutdown => Request::Shutdown,
        Cmd::Capabilities | Cmd::LocalDevices | Cmd::DefaultConfig | Cmd::Autostart { .. } => {
            unreachable!("handled above")
        }
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
                let f = st.fx;
                let mut fx = Vec::new();
                if f.gate_threshold_db > -80.0 {
                    fx.push(format!("gate {:.0} dB", f.gate_threshold_db));
                }
                if f.comp_amount > 0.0 {
                    fx.push(format!("comp {:.1}", f.comp_amount));
                }
                if f.noise_suppression {
                    fx.push("denoise".to_owned());
                }
                if f.eq_low_db != 0.0 || f.eq_mid_db != 0.0 || f.eq_high_db != 0.0 {
                    fx.push(format!("EQ {:+.1}/{:+.1}/{:+.1} dB", f.eq_low_db, f.eq_mid_db, f.eq_high_db));
                }
                let fx = if fx.is_empty() { String::new() } else { format!("  fx: {}", fx.join(", ")) };
                println!(
                    "  {i}: {:16} {:>6.1} dB{}{}  → {:?}  [{}]{fx}",
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
                    "  {i}: {:3} {:12} {:>6.1} dB{}  [{}]{}",
                    b.id,
                    b.name,
                    b.gain_db,
                    if b.mute { " M" } else { "" },
                    b.sink,
                    if b.keep_awake.enabled {
                        format!("  keep-awake {:.0} Hz at {:.0} dBFS", b.keep_awake.frequency_hz, b.keep_awake.level_db)
                    } else {
                        String::new()
                    }
                );
            }
            if !s.recovery_events.is_empty() {
                println!("recovery events:");
                for e in s.recovery_events.iter().rev().take(10) {
                    let times = if e.count > 1 { format!(" (×{})", e.count) } else { String::new() };
                    println!("  {:>8.1}s {} {}: {}{times}", e.at_secs, e.device, e.event, e.detail);
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
                print_device(&d.name, &d.id, d.is_input, d.is_output, d.is_default_input || d.is_default_output);
            }
        }
        Response::Ok => println!("ok"),
        other => println!("{other:?}"),
    }
    Ok(())
}

fn print_device(name: &str, id: &str, is_input: bool, is_output: bool, is_default: bool) {
    let dir = match (is_input, is_output) {
        (true, true) => "in/out",
        (true, false) => "in",
        _ => "out",
    };
    let def = if is_default { " (default)" } else { "" };
    println!("{dir:6} {name}{def}\n       id: {id}");
}

fn autostart(action: &AutostartCmd) -> anyhow::Result<()> {
    use mixercraft_platform::autostart::{self, AutostartSpec, AutostartState};
    match action {
        AutostartCmd::Enable { config, engine, now } => {
            let engine = match engine {
                Some(p) => p.clone(),
                None => {
                    let exe = std::env::current_exe().context("cannot locate this program")?;
                    exe.with_file_name(format!("mixercraftd{}", std::env::consts::EXE_SUFFIX))
                }
            };
            let engine = std::fs::canonicalize(&engine)
                .with_context(|| format!("engine not found at {} (use --engine)", engine.display()))?;
            let mut args = Vec::new();
            if let Some(config) = config {
                let config = std::fs::canonicalize(config)
                    .with_context(|| format!("configuration not found: {}", config.display()))?;
                args.push("--config".to_owned());
                args.push(strip_verbatim(&config));
            }
            let spec = AutostartSpec { engine: strip_verbatim(&engine).into(), args };
            autostart::enable(&spec, *now)?;
            println!("autostart enabled: {}", spec.engine.display());
            if *now {
                println!("engine started");
            }
        }
        AutostartCmd::Disable => {
            autostart::disable()?;
            println!("autostart disabled");
        }
        AutostartCmd::Status => match autostart::status()? {
            AutostartState::Enabled { detail } => println!("enabled ({detail})"),
            AutostartState::Disabled => println!("disabled"),
        },
    }
    Ok(())
}

/// `canonicalize` on Windows returns `\\?\C:\...` paths, which some tools
/// (Task Scheduler among them) do not accept. Plain drive paths are kept.
fn strip_verbatim(p: &std::path::Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix(r"\\?\").filter(|rest| !rest.starts_with("UNC")).unwrap_or(&s).to_owned()
}

/// "none", "default", an id (contains ':' like `wasapi:{...}`), a virtual
/// device (`virtual:<name>`) or a name.
fn device_choice(arg: &str) -> mixercraft_ipc::DeviceChoice {
    use mixercraft_ipc::DeviceChoice;
    match arg {
        "none" => DeviceChoice::None,
        "default" => DeviceChoice::Default,
        id if id.contains(':') && id.contains('{') => DeviceChoice::Device { id: Some(id.into()), name: None },
        // A MixerCraft virtual device, e.g. `virtual:Game`.
        id if id.starts_with("virtual:") => {
            DeviceChoice::Device { id: Some(id.into()), name: Some(format!("MixerCraft {}", &id["virtual:".len()..])) }
        }
        name => DeviceChoice::Device { id: None, name: Some(name.into()) },
    }
}
