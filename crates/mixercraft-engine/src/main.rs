//! `mixercraftd` — the MixerCraft background audio engine.
//!
//! Runs headless, independent of any GUI. Clients (GUI, `mixercraft` CLI)
//! connect over the versioned IPC protocol; closing or crashing a client
//! never affects audio.

mod clock;
mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Parser;
use mixercraft_config::{Config, ConfigError};
use mixercraft_ipc::{ErrorCode, Response};
use tracing::{error, info, warn};

use crate::supervisor::{IpcCall, Supervisor};

/// Maximum concurrent IPC clients.
const MAX_CLIENTS: usize = 8;

#[derive(Debug, Parser)]
#[command(name = "mixercraftd", version, about = "MixerCraft background audio engine")]
struct Args {
    /// Configuration file (default: the per-user configuration directory).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Print the default configuration and exit.
    #[arg(long)]
    print_default_config: bool,
    /// List audio devices and exit.
    #[arg(long)]
    list_devices: bool,
    /// Log filter, e.g. `info` or `mixercraft=debug`.
    #[arg(long, default_value = "info", env = "MIXERCRAFT_LOG")]
    log: String,
}

fn main() -> anyhow::Result<()> {
    let process_start = Instant::now();
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_new(&args.log).unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    if args.print_default_config {
        print!("{}", Config::default().to_toml()?);
        return Ok(());
    }
    if args.list_devices {
        let audio = mixercraft_platform::AudioSystem::new();
        println!("backend: {}", audio.backend_name());
        for d in audio.devices()? {
            let dir = match (d.is_input, d.is_output) {
                (true, true) => "in/out",
                (true, false) => "in",
                (false, true) => "out",
                _ => "-",
            };
            let def = if d.is_default_input || d.is_default_output { " (default)" } else { "" };
            println!("{dir:6} {}{def}\n       id: {}", d.name, d.id);
        }
        return Ok(());
    }

    // Single-instance guard: binding the IPC endpoint fails if another
    // engine already owns it.
    let listener = match mixercraft_ipc::bind() {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            bail!("another MixerCraft engine is already running ({})", mixercraft_ipc::endpoint_description())
        }
        Err(e) => return Err(e).context("cannot create IPC endpoint"),
    };

    let path = args.config.or_else(mixercraft_config::default_path);
    let config = load_config(path.as_deref())?;
    info!(config = ?path, "configuration loaded");

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&shutdown);
        ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed)).context("cannot install signal handler")?;
    }

    let (ipc_tx, ipc_rx) = mpsc::channel::<IpcCall>();
    std::thread::Builder::new().name("mixercraft-ipc".into()).spawn(move || {
        mixercraft_ipc::run_server(listener, env!("CARGO_PKG_VERSION").into(), MAX_CLIENTS, move |request| {
            let (reply, answer) = mpsc::sync_channel(1);
            if ipc_tx.send(IpcCall { request, reply }).is_err() {
                return Response::Error { code: ErrorCode::Internal, message: "engine is stopping".into() };
            }
            answer
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or(Response::Error { code: ErrorCode::Busy, message: "engine did not answer".into() })
        });
    })?;
    info!(endpoint = %mixercraft_ipc::endpoint_description(), "IPC listening");

    let supervisor = Supervisor::new(config, path, process_start)?;
    supervisor.run(&shutdown, &ipc_rx);
    info!("stopped");
    Ok(())
}

/// Loads the configuration, falling back to the backup copy if the main
/// file is unreadable. A missing file means first run: the default
/// configuration is written so the user can edit it.
fn load_config(path: Option<&std::path::Path>) -> anyhow::Result<Config> {
    let Some(path) = path else {
        warn!("no configuration directory available; using defaults without persistence");
        return Ok(Config::default());
    };
    match mixercraft_config::load_file(path) {
        Ok(Some(c)) => Ok(c),
        Ok(None) => {
            let c = Config::default();
            if let Err(e) = mixercraft_config::save_file(path, &c) {
                warn!("cannot write default configuration: {e}");
            }
            Ok(c)
        }
        Err(e @ ConfigError::TooNew { .. }) => {
            // Never downgrade a newer configuration silently.
            Err(e).context("configuration was written by a newer MixerCraft")
        }
        Err(e) => {
            error!("configuration is invalid: {e}");
            let backup = path.with_extension("toml.bak");
            match mixercraft_config::load_file(&backup) {
                Ok(Some(c)) => {
                    warn!(backup = %backup.display(), "using backup configuration; fix or delete the broken file");
                    Ok(c)
                }
                _ => Err(e).context(format!("cannot load {}", path.display())),
            }
        }
    }
}
