//! Update checks in the background (see the `mixercraft-update` crate).
//!
//! One thread owns the updater: it checks shortly after start and then once a
//! day (unless the user turned checks off), and downloads, verifies and
//! starts the installer only when the user clicks "Download and install". It
//! never touches Slint types; the UI receives [`Event`]s.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mixercraft_update::{Check, Release, Settings};

/// Delay before the check after start (keeps start-up light).
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
/// How often the thread wakes to see whether the daily check is due.
const WAKE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A user action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCmd {
    /// "Check now".
    Check,
    /// "Download and install".
    Install,
    /// The "check automatically" box.
    SetAuto(bool),
    /// "Release notes".
    OpenNotes,
}

/// What the About page shows (mirrors `UpdateView` in `ui/about.slint`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct View {
    pub state: i32,
    pub status: String,
    pub notes: String,
    pub progress: f32,
    pub can_install: bool,
    pub has_notes_url: bool,
}

/// UI states (`UpdateView.state`).
pub mod state {
    pub const CHECKING: i32 = 1;
    pub const AVAILABLE: i32 = 2;
    pub const DOWNLOADING: i32 = 3;
    pub const INSTALLING: i32 = 4;
    pub const ERROR: i32 = 5;
    pub const UP_TO_DATE: i32 = 6;
    pub const NOT_CONFIGURED: i32 = 7;
}

/// Something for the UI.
#[derive(Debug)]
pub enum Event {
    /// New page state.
    View(View),
    /// The installer runs; quit so it can replace the app.
    QuitForInstall,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Runs the updater until `rx` disconnects.
pub fn run(rx: &Receiver<UpdateCmd>, mut settings: Settings, exe_dir: PathBuf, emit: &dyn Fn(Event)) {
    let current = env!("CARGO_PKG_VERSION");
    let installed = mixercraft_update::installed_by_msi(&exe_dir);
    let mut available: Option<Release> = None;
    let mut view = View { status: "Updates have not been checked yet.".into(), ..View::default() };
    if settings.feed().is_none() {
        view = View {
            state: state::NOT_CONFIGURED,
            status: "Update checks are not configured for this build.".into(),
            ..View::default()
        };
    }
    emit(Event::View(view.clone()));

    let mut wait = FIRST_CHECK_DELAY;
    let mut first = true;
    loop {
        let cmd = match rx.recv_timeout(wait) {
            Ok(cmd) => Some(cmd),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        wait = WAKE_INTERVAL;
        let Some(feed) = settings.feed() else {
            if let Some(UpdateCmd::SetAuto(on)) = cmd {
                settings.check_automatically = on;
                let _ = settings.save();
            }
            continue;
        };
        match cmd {
            None => {
                // Automatic: once after start, then daily.
                let due = first || mixercraft_update::check_due(mixercraft_update::last_check(), now_secs());
                first = false;
                if settings.check_automatically && due {
                    view = check(&feed, current, installed, &mut available, emit);
                }
            }
            Some(UpdateCmd::Check) => view = check(&feed, current, installed, &mut available, emit),
            Some(UpdateCmd::SetAuto(on)) => {
                settings.check_automatically = on;
                if let Err(e) = settings.save() {
                    view = View { state: state::ERROR, status: format!("Cannot save the setting: {e}"), ..view };
                    emit(Event::View(view.clone()));
                }
            }
            Some(UpdateCmd::OpenNotes) => {
                if let Some(r) = &available {
                    open_url(&r.notes_url);
                }
            }
            Some(UpdateCmd::Install) => {
                if let Some(r) = available.clone()
                    && installed
                {
                    view = install(&r, emit).unwrap_or_else(|e| View {
                        state: state::ERROR,
                        status: format!("Update failed: {e}"),
                        ..View::default()
                    });
                    emit(Event::View(view.clone()));
                }
            }
        }
    }
}

fn check(feed: &str, current: &str, installed: bool, available: &mut Option<Release>, emit: &dyn Fn(Event)) -> View {
    emit(Event::View(View { state: state::CHECKING, status: "Checking for updates…".into(), ..View::default() }));
    let result = mixercraft_update::check(feed, current);
    let _ = mixercraft_update::record_check(now_secs());
    let view = match result {
        Ok(Check::UpToDate { .. }) => {
            *available = None;
            View { state: state::UP_TO_DATE, status: format!("MixerCraft {current} is up to date."), ..View::default() }
        }
        Ok(Check::Available(r)) => {
            let mut notes = r.notes.trim().to_owned();
            let extra = if r.asset.is_none() {
                Some("There is no installer for this platform in this release.")
            } else if !installed {
                Some("This copy was not installed with the MixerCraft installer, so it cannot update itself.")
            } else {
                None
            };
            if let Some(extra) = extra {
                if !notes.is_empty() {
                    notes.push_str("\n\n");
                }
                notes.push_str(extra);
            }
            let v = View {
                state: state::AVAILABLE,
                status: format!("Version {} is available (you have {current}).", r.version),
                notes,
                progress: 0.0,
                can_install: installed && r.asset.is_some(),
                has_notes_url: r.notes_url.starts_with("https://"),
            };
            *available = Some(r);
            v
        }
        Err(mixercraft_update::UpdateError::NoRelease) => {
            *available = None;
            View {
                state: state::UP_TO_DATE,
                status: format!("MixerCraft {current} is up to date (no release has been published yet)."),
                ..View::default()
            }
        }
        Err(e) => View { state: state::ERROR, status: format!("Update check failed: {e}"), ..View::default() },
    };
    emit(Event::View(view.clone()));
    view
}

/// Downloads, verifies and starts the installer; on success asks the app to
/// quit (the installer stops the engine and restarts both afterwards).
fn install(r: &Release, emit: &dyn Fn(Event)) -> Result<View, String> {
    let asset = r.asset.as_ref().ok_or("no installer for this platform")?;
    let dir = mixercraft_update::download_dir().ok_or("no data folder")?;
    let label = format!("Downloading version {}…", r.version);
    let mut shown = -1i64;
    let path = mixercraft_update::download(&r.version, asset, &dir, &mut |done, total| {
        let percent = (done * 100).checked_div(total).unwrap_or(0) as i64;
        if percent != shown {
            shown = percent;
            emit(Event::View(View {
                state: state::DOWNLOADING,
                status: format!("{label} {percent} %"),
                progress: percent as f32 / 100.0,
                ..View::default()
            }));
        }
    })
    .map_err(|e| e.to_string())?;
    emit(Event::View(View {
        state: state::INSTALLING,
        status: format!("Version {} downloaded and verified. Starting the installer…", r.version),
        ..View::default()
    }));
    let log = mixercraft_update::data_dir().ok_or("no data folder")?.join("install.log");
    mixercraft_update::msi_install_command(&path, &log)
        .spawn()
        .map_err(|e| format!("cannot start the installer: {e}"))?;
    emit(Event::QuitForInstall);
    Ok(View { state: state::INSTALLING, status: "The installer is running…".into(), ..View::default() })
}

/// Opens an https URL in the default browser.
fn open_url(url: &str) {
    if !url.starts_with("https://") {
        return;
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer.exe").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
