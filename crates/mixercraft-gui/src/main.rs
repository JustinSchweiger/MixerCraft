//! `mixercraft-gui` — the MixerCraft desktop mixer.
//!
//! A thin client: all audio runs in `mixercraftd`. The window can be closed,
//! crash or restart at any time without affecting audio.
//!
//! On Windows and macOS the app lives in the system tray: closing the window
//! hides it (unless the Settings tab says to quit), and a second launch shows
//! the running instance. `--tray` starts hidden (for sign-in).

// No console window for the GUI on Windows (release builds).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
mod fader_law;
mod fx_law;
mod meters;
mod settings;
#[cfg(any(windows, target_os = "macos"))]
mod tray;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mixercraft_ipc::{DeviceChoice, DeviceSummary, EngineSettings, EngineState, EngineStatus, LatencyProfile};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::engine::{Cmd, Update};

/// Code generated from `ui/*.slint`.
#[allow(
    missing_debug_implementations,
    clippy::all,
    clippy::pedantic,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro
)]
mod ui {
    slint::include_modules!();
}
use ui::{DeviceRow, MainWindow, MeterView, SettingKind, SettingsState, StripView};

/// After a local edit, status polls do not overwrite that strip for this long
/// (the engine may not have applied the change yet).
const LOCAL_EDIT_GRACE: Duration = Duration::from_millis(1500);
/// After an engine setting changes, a status answer already on its way does
/// not overwrite it for this long.
const SETTINGS_EDIT_GRACE: Duration = Duration::from_millis(400);
/// Where usbip-win2 (needed for virtual devices on Windows) is published.
const USBIP_WIN2_URL: &str = "https://github.com/vadimgrn/usbip-win2/releases";

/// The engine's configured settings and its buses (id, name, clocks the
/// engine) from the last status.
#[derive(Debug, Default)]
struct EngineSettingsView {
    settings: EngineSettings,
    buses: Vec<(String, String, bool)>,
    edited: Option<Instant>,
}

thread_local! {
    static LOCAL_EDITS: RefCell<HashMap<(bool, usize), Instant>> = RefCell::new(HashMap::new());
    /// What each entry of the input / output device pickers selects.
    static DEVICE_CHOICES: RefCell<(Vec<DeviceChoice>, Vec<DeviceChoice>)> = const { RefCell::new((Vec::new(), Vec::new())) };
    /// Keep-awake settings of every bus from the last status.
    static BUS_KEEP_AWAKE: RefCell<Vec<mixercraft_ipc::KeepAwake>> = const { RefCell::new(Vec::new()) };
    /// The last device list and whether the engine can create virtual
    /// devices (the pickers are rebuilt when either changes).
    static DEVICE_LIST: RefCell<(Vec<DeviceSummary>, bool)> = const { RefCell::new((Vec::new(), false)) };
    /// When the title bar was last pressed (a second press soon after
    /// toggles maximise).
    static LAST_TITLE_PRESS: RefCell<Option<Instant>> = const { RefCell::new(None) };
    #[cfg(any(windows, target_os = "macos"))]
    static TRAY: RefCell<Option<tray::Tray>> = const { RefCell::new(None) };
    /// App preferences and the file they are saved to.
    static GUI_SETTINGS: RefCell<(Option<PathBuf>, settings::GuiSettings)> = RefCell::new((None, settings::GuiSettings::default()));
    static ENGINE_SETTINGS: RefCell<EngineSettingsView> = RefCell::new(EngineSettingsView::default());
}

fn gui_settings() -> settings::GuiSettings {
    GUI_SETTINGS.with(|g| g.borrow().1)
}

/// Whether the close button hides the window instead of quitting.
fn hides_to_tray() -> bool {
    has_tray() && gui_settings().close_to_tray
}

/// Whether a tray icon exists (closing the window then only hides it).
fn has_tray() -> bool {
    #[cfg(any(windows, target_os = "macos"))]
    {
        TRAY.with(|t| t.borrow().is_some())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// Mirrors strip mutes, connection and status into the tray.
fn update_tray(ui: &MainWindow) {
    #[cfg(any(windows, target_os = "macos"))]
    TRAY.with(|t| {
        if let Some(tray) = t.borrow_mut().as_mut() {
            let strips: Vec<(String, bool)> = ui.get_strips().iter().map(|s| (s.name.to_string(), s.mute)).collect();
            tray.update(&strips, ui.get_connected(), &format!("MixerCraft — {}", ui.get_status_text()));
        }
    });
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = ui;
}

fn show_window(ui: &MainWindow) {
    let _ = ui.show();
    ui.window().set_minimized(false);
}

fn mark_edit(bus: bool, i: usize) {
    LOCAL_EDITS.with(|e| e.borrow_mut().insert((bus, i), Instant::now()));
}

fn recently_edited(bus: bool, i: usize) -> bool {
    LOCAL_EDITS.with(|e| e.borrow().get(&(bus, i)).is_some_and(|t| t.elapsed() < LOCAL_EDIT_GRACE))
}

fn vec_model<T: Clone + 'static>(m: &ModelRc<T>) -> Option<&VecModel<T>> {
    m.as_any().downcast_ref::<VecModel<T>>()
}

/// Text shown in a device picker for a configured device.
fn choice_text(choice: &DeviceChoice, input: bool) -> String {
    match choice {
        DeviceChoice::None => "None".into(),
        DeviceChoice::Default if input => "Default input".into(),
        DeviceChoice::Default => "Default output".into(),
        DeviceChoice::Device { name: Some(name), .. } => name.clone(),
        DeviceChoice::Device { id: Some(id), .. } => id.clone(),
        DeviceChoice::Device { .. } => "Unknown device".into(),
    }
}

/// Id prefix of MixerCraft virtual devices; alone it marks the picker entry
/// that creates one named after the strip or bus.
const VIRTUAL_PREFIX: &str = "virtual:";

/// Picker entries ("None", the default, then each device, then a virtual
/// device if available) and what each selects.
fn device_options(devices: &[DeviceSummary], input: bool, virtual_ok: bool) -> (Vec<SharedString>, Vec<DeviceChoice>) {
    let mut names = vec![SharedString::from("None"), SharedString::from(choice_text(&DeviceChoice::Default, input))];
    let mut choices = vec![DeviceChoice::None, DeviceChoice::Default];
    for d in devices.iter().filter(|d| if input { d.is_input } else { d.is_output }) {
        names.push(d.name.as_str().into());
        choices.push(DeviceChoice::Device { id: Some(d.id.clone()), name: Some(d.name.clone()) });
    }
    if virtual_ok {
        // A strip's virtual device is a playback endpoint applications play
        // into; a bus's is a recording endpoint applications record from.
        names.push(if input { "New virtual playback device" } else { "New virtual recording device" }.into());
        choices.push(DeviceChoice::Device { id: Some(VIRTUAL_PREFIX.into()), name: None });
    }
    (names, choices)
}

/// Resolves the "new virtual device" entry to a device named after the
/// strip or bus.
fn resolve_choice(choice: DeviceChoice, owner: &str) -> DeviceChoice {
    match choice {
        DeviceChoice::Device { id: Some(id), name: None } if id == VIRTUAL_PREFIX => DeviceChoice::Device {
            id: Some(format!("{VIRTUAL_PREFIX}{owner}")),
            name: Some(format!("MixerCraft {owner}")),
        },
        other => other,
    }
}

fn apply_devices(ui: &MainWindow, devices: Option<Vec<DeviceSummary>>, virtual_ok: Option<bool>) {
    let changed = DEVICE_LIST.with(|l| {
        let mut l = l.borrow_mut();
        let before = (l.0.clone(), l.1);
        if let Some(d) = devices {
            l.0 = d;
        }
        if let Some(v) = virtual_ok {
            l.1 = v;
        }
        before != (l.0.clone(), l.1)
    });
    if !changed {
        return;
    }
    let (devices, virtual_ok) = DEVICE_LIST.with(|l| l.borrow().clone());
    let (in_names, in_choices) = device_options(&devices, true, virtual_ok);
    let (out_names, out_choices) = device_options(&devices, false, virtual_ok);
    if ui.get_input_devices().iter().ne(in_names.iter().cloned()) {
        ui.set_input_devices(ModelRc::new(VecModel::from(in_names)));
    }
    if ui.get_output_devices().iter().ne(out_names.iter().cloned()) {
        ui.set_output_devices(ModelRc::new(VecModel::from(out_names)));
    }
    DEVICE_CHOICES.with(|c| *c.borrow_mut() = (in_choices, out_choices));
}

fn strip_view(name: &str, device: &str, db: f32, mute: bool, solo: bool, routes: Vec<bool>) -> StripView {
    with_fx(
        StripView {
            name: name.into(),
            device: device.into(),
            pos: fader_law::db_to_pos(db),
            db,
            db_text: fader_law::knob_text(db).into(),
            mute,
            solo,
            routes: ModelRc::new(VecModel::from(routes)),
            ..StripView::default()
        },
        &mixercraft_ipc::StripFx::default(),
    )
}

/// Sets the knob positions of a strip from engine settings.
fn with_fx(mut row: StripView, fx: &mixercraft_ipc::StripFx) -> StripView {
    let k = fx_law::Knobs::from_fx(fx);
    row.gate = k.gate;
    row.comp = k.comp;
    row.low = k.low;
    row.mid = k.mid;
    row.high = k.high;
    row.denoise = k.denoise;
    row
}

fn knobs_of(row: &StripView) -> fx_law::Knobs {
    fx_law::Knobs { gate: row.gate, comp: row.comp, low: row.low, mid: row.mid, high: row.high, denoise: row.denoise }
}

/// Replaces rows that changed, or the whole model if the shape changed.
/// Rows recently edited locally keep their control values.
fn sync_rows(current: ModelRc<StripView>, fresh: Vec<StripView>, bus: bool) -> Option<ModelRc<StripView>> {
    let Some(model) = vec_model(&current) else { return Some(ModelRc::new(VecModel::from(fresh))) };
    if model.row_count() != fresh.len() {
        return Some(ModelRc::new(VecModel::from(fresh)));
    }
    for (i, mut row) in fresh.into_iter().enumerate() {
        let old = model.row_data(i).unwrap_or_default();
        if recently_edited(bus, i) {
            row.pos = old.pos;
            row.db = old.db;
            row.db_text = old.db_text.clone();
            row.mute = old.mute;
            row.solo = old.solo;
            row.routes = old.routes.clone();
            row.gate = old.gate;
            row.comp = old.comp;
            row.low = old.low;
            row.mid = old.mid;
            row.high = old.high;
            row.denoise = old.denoise;
            row.keep_awake = old.keep_awake;
            row.device = old.device.clone();
            row.name = old.name.clone();
        }
        let same_routes = old.routes.iter().eq(row.routes.iter());
        let changed = old.name != row.name
            || old.master != row.master
            || old.device != row.device
            || old.pos != row.pos
            || old.db_text != row.db_text
            || old.mute != row.mute
            || old.solo != row.solo
            || knobs_of(&old) != knobs_of(&row)
            || old.keep_awake != row.keep_awake
            || !same_routes;
        if changed {
            model.set_row_data(i, row);
        }
    }
    None
}

fn apply_status(ui: &MainWindow, s: &EngineStatus) {
    ui.set_connected(true);
    apply_engine_settings(ui, s);
    apply_devices(ui, None, Some(s.virtual_devices));
    if ui.get_denoise_available() != s.noise_suppression_available {
        ui.set_denoise_available(s.noise_suppression_available);
    }
    BUS_KEEP_AWAKE.with(|k| *k.borrow_mut() = s.buses.iter().map(|b| b.keep_awake).collect());

    let bus_ids: Vec<SharedString> = s.buses.iter().map(|b| SharedString::from(b.id.as_str())).collect();
    let ids_changed = ui.get_bus_ids().iter().ne(bus_ids.iter().cloned());
    if ids_changed {
        ui.set_bus_ids(ModelRc::new(VecModel::from(bus_ids)));
    }

    let strips: Vec<StripView> = s
        .strips
        .iter()
        .map(|st| {
            let routes = (0..s.buses.len()).map(|b| st.routes.contains(&(b as u16))).collect();
            let device = choice_text(&st.source_choice, true);
            with_fx(strip_view(&st.name, &device, st.gain_db, st.mute, st.solo, routes), &st.fx)
        })
        .collect();
    let n_strips = strips.len();
    if let Some(m) = sync_rows(ui.get_strips(), strips, false) {
        ui.set_strips(m);
        ui.set_strip_meters(ModelRc::new(VecModel::from(vec![MeterView::default(); n_strips])));
    }
    let buses: Vec<StripView> = s
        .buses
        .iter()
        .map(|b| StripView {
            master: b.master,
            keep_awake: b.keep_awake.enabled,
            ..strip_view(&b.name, &choice_text(&b.sink_choice, false), b.gain_db, b.mute, false, Vec::new())
        })
        .collect();
    let n_buses = buses.len();
    if let Some(m) = sync_rows(ui.get_buses(), buses, true) {
        ui.set_buses(m);
        ui.set_bus_meters(ModelRc::new(VecModel::from(vec![MeterView::default(); n_buses])));
    }

    let latency_ms = s
        .devices
        .iter()
        .filter_map(|d| d.fifo.map(|f| f64::from(f.target_frames) * 1000.0 / f64::from(d.sample_rate.max(1))))
        .fold(0.0f64, f64::max);
    let gaps: u64 = s.devices.iter().filter_map(|d| d.fifo.map(|f| f.gap_reads)).sum();
    let (state, color) = match s.state {
        EngineState::Running => ("Running", Color::from_rgb_u8(0x46, 0xc4, 0x7a)),
        EngineState::Starting => ("Starting", Color::from_rgb_u8(0xf2, 0xc1, 0x4e)),
        EngineState::Degraded => ("Degraded", Color::from_rgb_u8(0xf2, 0xc1, 0x4e)),
        EngineState::Stopping => ("Stopping", Color::from_rgb_u8(0xff, 0x6b, 0x5f)),
    };
    ui.set_status_color(color);
    ui.set_status_text(
        format!(
            "{state} · {} kHz · {latency_ms:.0} ms · {gaps} gap{}",
            f64::from(s.sample_rate) / 1000.0,
            if gaps == 1 { "" } else { "s" }
        )
        .into(),
    );

    let up = s.uptime_secs as u64;
    ui.set_engine_line(
        format!(
            "Version {} · {} · clock {} · load {:.1} % (peak {:.1} %) · {} overloads · up {}h {:02}m {:02}s",
            s.engine_version,
            s.backend,
            s.clock_source,
            f64::from(s.load_permille) / 10.0,
            f64::from(s.peak_load_permille) / 10.0,
            s.overloads,
            up / 3600,
            up / 60 % 60,
            up % 60
        )
        .into(),
    );
    let rows: Vec<DeviceRow> = s
        .devices
        .iter()
        .map(|d| {
            let f = d.fifo;
            DeviceRow {
                name: d.name.as_str().into(),
                role: d.role.as_str().into(),
                state: d.state.as_str().into(),
                healthy: d.state == "running",
                format: format!("{} kHz · {} ch", f64::from(d.sample_rate) / 1000.0, d.channels).into(),
                buffer: f
                    .map(|f| format!("{:.0} ms", f64::from(f.target_frames) * 1000.0 / f64::from(d.sample_rate.max(1))))
                    .unwrap_or_else(|| "direct".into())
                    .into(),
                gaps: f.map(|f| f.gap_reads.to_string()).unwrap_or_else(|| "–".into()).into(),
                drift: f.map(|f| format!("{:+.1} ppm", f.drift_ppm)).unwrap_or_else(|| "–".into()).into(),
                error: d.error.clone().unwrap_or_default().into(),
            }
        })
        .collect();
    ui.set_devices(ModelRc::new(VecModel::from(rows)));
    let events: Vec<SharedString> = s
        .recovery_events
        .iter()
        .rev()
        .map(|e| {
            let times = if e.count > 1 { format!("  (×{})", e.count) } else { String::new() };
            format!("{:>9.1} s   {}   {}: {}{times}", e.at_secs, e.device, e.event, e.detail).into()
        })
        .collect();
    ui.set_events(ModelRc::new(VecModel::from(events)));
}

fn apply_meters(model: &ModelRc<MeterView>, values: &[[f32; 4]]) {
    let Some(m) = vec_model(model) else { return };
    if m.row_count() != values.len() {
        return; // the next status update resizes the model
    }
    let hold = gui_settings().peak_hold;
    for (i, v) in values.iter().enumerate() {
        let (lp, rp) = if hold { (v[2], v[3]) } else { (0.0, 0.0) };
        m.set_row_data(i, MeterView { l: v[0], r: v[1], lp, rp });
    }
}

/// "44.1 kHz", "48 kHz".
fn khz(rate: u32) -> String {
    format!("{} kHz", f64::from(rate) / 1000.0)
}

/// Picker label of a bus.
fn bus_label(id: &str, name: &str) -> String {
    if id == name { id.to_owned() } else { format!("{id} · {name}") }
}

/// Explains the virtual device state in the user's terms.
fn virtual_note(available: bool, note: &str) -> String {
    if available {
        String::new()
    } else if note.contains("usbip-win2 is not installed") {
        "Needs the free usbip-win2 driver. Installing it needs administrator rights and briefly restarts the \
         USB hubs."
            .into()
    } else if note.is_empty() {
        "Unavailable.".into()
    } else {
        let mut c = note.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    }
}

/// Virtual device state from this machine (while no engine is connected).
fn set_local_virtual_state(st: &mut SettingsState) {
    let reason = mixercraft_platform::virtual_dev::unavailable_reason();
    st.virtual_available = reason.is_none();
    st.virtual_note = virtual_note(reason.is_none(), reason.unwrap_or_default()).into();
    st.usbip_link = cfg!(windows) && mixercraft_platform::virtual_dev::usbip_exe().is_none();
}

/// Fills the engine part of the Settings tab from `view`.
fn engine_fields(st: &mut SettingsState, view: &EngineSettingsView) {
    let e = &view.settings;
    st.sample_rate_index = match e.sample_rate {
        44_100 => 0,
        48_000 => 1,
        _ => -1,
    };
    st.latency_index = match e.latency_profile {
        LatencyProfile::LowLatency => 0,
        LatencyProfile::Balanced => 1,
        LatencyProfile::MaxStability => 2,
    };
    let clock = view.buses.iter().find(|b| b.2).map(|b| bus_label(&b.0, &b.1));
    let automatic = clock.as_ref().map_or_else(|| "Automatic".to_owned(), |c| format!("Automatic ({c})"));
    let mut options = vec![SharedString::from("Automatic: first bus with a device")];
    options.extend(view.buses.iter().map(|b| SharedString::from(bus_label(&b.0, &b.1))));
    if st.master_options.iter().ne(options.iter().cloned()) {
        st.master_options = ModelRc::new(VecModel::from(options));
    }
    st.master_text = match &e.master_bus {
        None => automatic.into(),
        Some(id) => view.buses.iter().find(|b| &b.0 == id).map_or_else(|| id.clone(), |b| bus_label(&b.0, &b.1)).into(),
    };
}

fn apply_engine_settings(ui: &MainWindow, s: &EngineStatus) {
    let recent = ENGINE_SETTINGS.with(|v| {
        let mut v = v.borrow_mut();
        v.buses = s.buses.iter().map(|b| (b.id.clone(), b.name.clone(), b.master)).collect();
        let recent = v.edited.is_some_and(|t| t.elapsed() < SETTINGS_EDIT_GRACE);
        if !recent {
            v.settings = s.settings.clone();
        }
        recent
    });
    if recent {
        return;
    }
    let mut st = ui.get_settings();
    let before = st.clone();
    ENGINE_SETTINGS.with(|v| engine_fields(&mut st, &v.borrow()));
    let mut detail = format!("Running at {}.", khz(s.sample_rate));
    if st.sample_rate_index < 0 {
        detail.push_str(&format!(" The configuration file asks for {}.", khz(s.settings.sample_rate)));
    } else if !s.restart_required && s.settings.sample_rate != s.sample_rate {
        detail.push_str(" The clock device does not offer the chosen rate, so the engine uses the device's rate.");
    }
    detail.push_str(" Noise suppression needs 48 kHz.");
    st.rate_detail = detail.into();
    st.restart_required = s.restart_required;
    st.config_path = s.config_path.as_str().into();
    st.virtual_available = s.virtual_devices;
    st.virtual_note = virtual_note(s.virtual_devices, &s.virtual_devices_note).into();
    st.usbip_link = cfg!(windows) && mixercraft_platform::virtual_dev::usbip_exe().is_none();
    if st != before {
        ui.set_settings(st);
    }
}

/// Changes an engine setting: tells the engine and shows it at once.
fn change_engine_settings(ui: &MainWindow, tx: &mpsc::Sender<Cmd>, f: impl FnOnce(&mut EngineSettings, &[String])) {
    let changed = ENGINE_SETTINGS.with(|v| {
        let mut v = v.borrow_mut();
        let ids: Vec<String> = v.buses.iter().map(|b| b.0.clone()).collect();
        let mut next = v.settings.clone();
        f(&mut next, &ids);
        if next == v.settings {
            return None;
        }
        v.settings = next.clone();
        v.edited = Some(Instant::now());
        Some(next)
    });
    let Some(next) = changed else { return };
    let _ = tx.send(Cmd::EngineSettings(next));
    let mut st = ui.get_settings();
    ENGINE_SETTINGS.with(|v| engine_fields(&mut st, &v.borrow()));
    ui.set_settings(st);
}

/// Changes and saves an app preference.
fn change_gui_settings(ui: &MainWindow, f: impl FnOnce(&mut settings::GuiSettings)) {
    let (path, gs) = GUI_SETTINGS.with(|g| {
        let mut g = g.borrow_mut();
        f(&mut g.1);
        (g.0.clone(), g.1)
    });
    let mut st = ui.get_settings();
    st.close_to_tray = gs.close_to_tray;
    st.peak_hold = gs.peak_hold;
    ui.set_settings(st);
    if let Some(path) = path
        && let Err(e) = settings::save(&path, &gs)
    {
        show_notice(ui, format!("Cannot save the settings to {}: {e}", path.display()));
    }
}

/// Opens a web address or folder with the system's default handler.
fn open_external(target: &std::ffi::OsStr) {
    let tool = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(tool).arg(target).spawn();
}

/// What the sign-in registration of the app starts.
fn app_autostart_spec() -> Result<mixercraft_platform::autostart::AutostartSpec, String> {
    let program = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    Ok(mixercraft_platform::autostart::AutostartSpec { program, args: vec!["--tray".into()] })
}

/// What the sign-in registration of the engine starts: `mixercraftd` next
/// to this program, with the configuration file this app was told to use.
fn engine_autostart_spec() -> Result<mixercraft_platform::autostart::AutostartSpec, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    let program = exe.with_file_name(format!("mixercraftd{}", std::env::consts::EXE_SUFFIX));
    if !program.is_file() {
        return Err(format!("mixercraftd not found next to the app ({})", program.display()));
    }
    let mut args = Vec::new();
    if let Some(config) = std::env::var_os("MIXERCRAFT_CONFIG").filter(|c| !c.is_empty()) {
        args.push("--config".into());
        args.push(config.to_string_lossy().into_owned());
    }
    Ok(mixercraft_platform::autostart::AutostartSpec { program, args })
}

/// Reads (and with `change` first sets) the sign-in registrations on a
/// background thread; Task Scheduler calls take a moment.
fn autostart_task(ui: &MainWindow, change: Option<(bool, bool)>) {
    use mixercraft_platform::autostart::{self, AutostartState};
    if !autostart::app_supported() {
        return;
    }
    let mut st = ui.get_settings();
    st.autostart_busy = true;
    ui.set_settings(st);
    let w = ui.as_weak();
    let _ = std::thread::Builder::new().name("mixercraft-gui-autostart".into()).spawn(move || {
        let result = match change {
            Some((true, true)) => {
                app_autostart_spec().and_then(|s| autostart::enable_app(&s).map_err(|e| e.to_string()))
            }
            Some((true, false)) => autostart::disable_app().map_err(|e| e.to_string()),
            Some((false, true)) => {
                engine_autostart_spec().and_then(|s| autostart::enable(&s, false).map_err(|e| e.to_string()))
            }
            Some((false, false)) => autostart::disable().map_err(|e| e.to_string()),
            None => Ok(()),
        };
        let enabled = |s: Result<AutostartState, _>| matches!(s, Ok(AutostartState::Enabled { .. }));
        let (app, engine) = (enabled(autostart::app_status()), enabled(autostart::status()));
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let mut st = ui.get_settings();
            st.autostart_busy = false;
            st.app_autostart = app;
            st.engine_autostart = engine;
            ui.set_settings(st);
            if let Err(e) = result {
                show_notice(&ui, format!("Cannot change the sign-in start: {e}"));
            }
        });
    });
}

/// Handles a control on the Settings tab.
fn on_setting(ui: &MainWindow, tx: &mpsc::Sender<Cmd>, kind: SettingKind, value: i32) {
    let on = value != 0;
    match kind {
        SettingKind::AppAutostart => autostart_task(ui, Some((true, on))),
        SettingKind::EngineAutostart => autostart_task(ui, Some((false, on))),
        SettingKind::CloseToTray => change_gui_settings(ui, |g| g.close_to_tray = on),
        SettingKind::PeakHold => change_gui_settings(ui, |g| g.peak_hold = on),
        SettingKind::SampleRate => {
            change_engine_settings(ui, tx, |e, _| e.sample_rate = if value == 0 { 44_100 } else { 48_000 });
        }
        SettingKind::Latency => change_engine_settings(ui, tx, |e, _| {
            e.latency_profile = match value {
                0 => LatencyProfile::LowLatency,
                2 => LatencyProfile::MaxStability,
                _ => LatencyProfile::Balanced,
            };
        }),
        SettingKind::MasterBus => change_engine_settings(ui, tx, |e, ids| {
            e.master_bus = usize::try_from(value - 1).ok().and_then(|i| ids.get(i).cloned());
        }),
        SettingKind::RestartEngine => {
            let _ = tx.send(Cmd::RestartEngine);
        }
        SettingKind::StartEngine => {
            let _ = tx.send(Cmd::StartEngine);
        }
        SettingKind::GetUsbip => open_external(USBIP_WIN2_URL.as_ref()),
        SettingKind::OpenConfigFolder => {
            let path = PathBuf::from(ui.get_settings().config_path.as_str());
            if let Some(dir) = path.parent() {
                open_external(dir.as_os_str());
            }
        }
    }
}

fn apply(ui: &MainWindow, update: Update) {
    match update {
        Update::Disconnected { hint } => {
            ui.set_connected(false);
            let mut st = ui.get_settings();
            set_local_virtual_state(&mut st);
            st.restart_required = false;
            ui.set_settings(st);
            ui.set_status_text("Engine not running".into());
            ui.set_status_color(Color::from_rgb_u8(0x7c, 0x85, 0x92));
            ui.set_connection_hint(hint.into());
        }
        Update::Status(s) => apply_status(ui, &s),
        Update::Meters { strips, buses } => {
            apply_meters(&ui.get_strip_meters(), &strips);
            apply_meters(&ui.get_bus_meters(), &buses);
            return;
        }
        Update::Devices(devices) => {
            apply_devices(ui, Some(devices), None);
            return;
        }
        Update::Notice(text) => {
            show_notice(ui, text);
            return;
        }
    }
    update_tray(ui);
}

/// Shows a message from the engine for a few seconds.
fn show_notice(ui: &MainWindow, text: String) {
    let text = SharedString::from(text);
    ui.set_notice(text.clone());
    let w = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(6), move || {
        if let Some(ui) = w.upgrade()
            && ui.get_notice() == text
        {
            ui.set_notice(SharedString::new());
        }
    });
}

/// Updates one strip row in place for immediate visual feedback.
fn edit_row(model: &ModelRc<StripView>, i: usize, f: impl FnOnce(&mut StripView)) -> Option<StripView> {
    let m = vec_model(model)?;
    let mut row = m.row_data(i)?;
    f(&mut row);
    m.set_row_data(i, row.clone());
    Some(row)
}

fn set_gain(row: &mut StripView, pos: f32) -> f32 {
    let db = fader_law::pos_to_db(pos);
    row.pos = pos;
    row.db = db.max(fader_law::MIN_DB);
    row.db_text = fader_law::knob_text(db).into();
    db
}

fn main() -> Result<(), slint::PlatformError> {
    let start_hidden = std::env::args().any(|a| a == "--tray");

    // Single instance: if the app is already running, show it and exit.
    let instance = match mixercraft_ipc::gui_instance::claim() {
        Ok(listener) => Some(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if mixercraft_ipc::gui_instance::activate_existing().is_ok() {
                return Ok(());
            }
            None
        }
        Err(_) => None,
    };

    let ui = MainWindow::new()?;
    ui.set_strips(ModelRc::new(VecModel::<StripView>::default()));
    ui.set_buses(ModelRc::new(VecModel::<StripView>::default()));
    ui.set_strip_meters(ModelRc::new(VecModel::<MeterView>::default()));
    ui.set_bus_meters(ModelRc::new(VecModel::<MeterView>::default()));
    ui.set_status_text("Connecting…".into());

    let settings_path = settings::path();
    let gui = settings_path.as_deref().map(settings::load).unwrap_or_default();
    GUI_SETTINGS.with(|g| *g.borrow_mut() = (settings_path, gui));
    let mut st = SettingsState {
        autostart_supported: mixercraft_platform::autostart::app_supported(),
        has_tray: cfg!(any(windows, target_os = "macos")),
        close_to_tray: gui.close_to_tray,
        peak_hold: gui.peak_hold,
        sample_rate_index: 1,
        latency_index: 1,
        ..SettingsState::default()
    };
    set_local_virtual_state(&mut st);
    ui.set_settings(st);
    autostart_task(&ui, None);

    let (tx, rx) = mpsc::channel::<Cmd>();
    let weak = ui.as_weak();
    std::thread::Builder::new()
        .name("mixercraft-gui-engine".into())
        .spawn(move || {
            engine::run(&rx, &|update| {
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        apply(&ui, update);
                    }
                });
            });
        })
        .expect("spawn engine connection thread");

    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_gain(move |i, pos| {
        let mut db = 0.0;
        if edit_row(&w.unwrap().get_strips(), i as usize, |r| db = set_gain(r, pos)).is_some() {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripGain(i as u16, db));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_bus_gain(move |i, pos| {
        let mut db = 0.0;
        if edit_row(&w.unwrap().get_buses(), i as usize, |r| db = set_gain(r, pos)).is_some() {
            mark_edit(true, i as usize);
            let _ = t.send(Cmd::BusGain(i as u16, db));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_mute(move |i| {
        if let Some(row) = edit_row(&w.unwrap().get_strips(), i as usize, |r| r.mute = !r.mute) {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripMute(i as u16, row.mute));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_bus_mute(move |i| {
        if let Some(row) = edit_row(&w.unwrap().get_buses(), i as usize, |r| r.mute = !r.mute) {
            mark_edit(true, i as usize);
            let _ = t.send(Cmd::BusMute(i as u16, row.mute));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_solo(move |i| {
        if let Some(row) = edit_row(&w.unwrap().get_strips(), i as usize, |r| r.solo = !r.solo) {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripSolo(i as u16, row.solo));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_route(move |i, b| {
        let ui = w.unwrap();
        let Some(row) = vec_model(&ui.get_strips()).and_then(|m| m.row_data(i as usize)) else { return };
        let on = !row.routes.row_data(b as usize).unwrap_or(false);
        row.routes.set_row_data(b as usize, on);
        mark_edit(false, i as usize);
        let _ = t.send(Cmd::Route { strip: i as u16, bus: b as u16, on });
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_fx(move |i, kind, v| {
        let mut knobs = None;
        let edited = edit_row(&w.unwrap().get_strips(), i as usize, |r| {
            let v = v.clamp(0.0, 1.0);
            match kind {
                0 => r.gate = v,
                1 => r.comp = v,
                2 => r.low = v,
                3 => r.mid = v,
                _ => r.high = v,
            }
            knobs = Some(knobs_of(r));
        });
        if let (Some(_), Some(k)) = (edited, knobs) {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripFx(i as u16, k.to_fx()));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_denoise(move |i| {
        let ui = w.unwrap();
        if !ui.get_denoise_available() {
            show_notice(&ui, "Noise suppression needs the engine to run at 48 kHz.".into());
            return;
        }
        let mut knobs = None;
        if edit_row(&ui.get_strips(), i as usize, |r| {
            r.denoise = !r.denoise;
            knobs = Some(knobs_of(r));
        })
        .is_some()
            && let Some(k) = knobs
        {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripFx(i as u16, k.to_fx()));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_bus_keep_awake(move |i| {
        let ui = w.unwrap();
        let mut enabled = false;
        if edit_row(&ui.get_buses(), i as usize, |r| {
            r.keep_awake = !r.keep_awake;
            enabled = r.keep_awake;
        })
        .is_some()
        {
            mark_edit(true, i as usize);
            // Keep a level or frequency set elsewhere (CLI, config file).
            let mut keep_awake = BUS_KEEP_AWAKE.with(|k| k.borrow().get(i as usize).copied()).unwrap_or_default();
            keep_awake.enabled = enabled;
            let _ = t.send(Cmd::BusKeepAwake(i as u16, keep_awake));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_device(move |i, d| {
        let Some(choice) = DEVICE_CHOICES.with(|c| c.borrow().0.get(d as usize).cloned()) else { return };
        let ui = w.unwrap();
        let Some(owner) = vec_model(&ui.get_strips()).and_then(|m| m.row_data(i as usize)) else { return };
        let choice = resolve_choice(choice, &owner.name);
        let text = choice_text(&choice, true);
        if edit_row(&ui.get_strips(), i as usize, |r| r.device = text.into()).is_some() {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::StripSource(i as u16, choice));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_bus_device(move |i, d| {
        let Some(choice) = DEVICE_CHOICES.with(|c| c.borrow().1.get(d as usize).cloned()) else { return };
        let ui = w.unwrap();
        let Some(owner) = vec_model(&ui.get_buses()).and_then(|m| m.row_data(i as usize)) else { return };
        let choice = resolve_choice(choice, &owner.name);
        let text = choice_text(&choice, false);
        if edit_row(&ui.get_buses(), i as usize, |r| r.device = text.into()).is_some() {
            mark_edit(true, i as usize);
            let _ = t.send(Cmd::BusSink(i as u16, choice));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_strip_rename(move |i, name| {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if edit_row(&w.unwrap().get_strips(), i as usize, |r| r.name = name.as_str().into()).is_some() {
            mark_edit(false, i as usize);
            let _ = t.send(Cmd::RenameStrip(i as u16, name));
        }
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_bus_rename(move |i, name| {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        if edit_row(&w.unwrap().get_buses(), i as usize, |r| r.name = name.as_str().into()).is_some() {
            mark_edit(true, i as usize);
            let _ = t.send(Cmd::RenameBus(i as u16, name));
        }
    });
    let t = tx.clone();
    ui.on_strip_remove(move |i| {
        let _ = t.send(Cmd::RemoveStrip(i as u16));
    });
    let t = tx.clone();
    ui.on_bus_remove(move |i| {
        let _ = t.send(Cmd::RemoveBus(i as u16));
    });
    let t = tx.clone();
    ui.on_add_strip(move || {
        let _ = t.send(Cmd::AddStrip);
    });
    let t = tx.clone();
    ui.on_add_bus(move || {
        let _ = t.send(Cmd::AddBus);
    });
    install_window_actions(&ui);
    let t = tx.clone();
    ui.on_start_engine(move || {
        let _ = t.send(Cmd::StartEngine);
    });
    let w = ui.as_weak();
    let t = tx.clone();
    ui.on_setting_changed(move |kind, value| {
        if let Some(ui) = w.upgrade() {
            on_setting(&ui, &t, kind, value);
        }
    });

    if let Some(listener) = instance {
        let w = ui.as_weak();
        std::thread::Builder::new()
            .name("mixercraft-gui-instance".into())
            .spawn(move || {
                mixercraft_ipc::gui_instance::serve(&listener, || {
                    let w = w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            show_window(&ui);
                        }
                    });
                });
            })
            .expect("spawn instance listener");
    }

    #[cfg(any(windows, target_os = "macos"))]
    install_tray(&ui, tx);
    #[cfg(not(any(windows, target_os = "macos")))]
    drop(tx);

    ui.window().on_close_requested(|| {
        if !hides_to_tray() {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
    let w = ui.as_weak();
    ui.on_close_window(move || {
        if hides_to_tray() {
            if let Some(ui) = w.upgrade() {
                let _ = ui.hide();
            }
        } else {
            let _ = slint::quit_event_loop();
        }
    });

    if !(start_hidden && cfg!(any(windows, target_os = "macos"))) {
        ui.show()?;
    }
    slint::run_event_loop_until_quit()
}

/// Title-bar dragging, the window buttons and edge resizing of the
/// frameless window.
fn install_window_actions(ui: &MainWindow) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::window::ResizeDirection;

    /// Two presses within this time count as a double click.
    const DOUBLE_PRESS: Duration = Duration::from_millis(400);

    ui.set_version(env!("CARGO_PKG_VERSION").into());

    let w = ui.as_weak();
    ui.on_drag_window(move || {
        let Some(ui) = w.upgrade() else { return };
        let now = Instant::now();
        let double = LAST_TITLE_PRESS.with(|p| {
            let double = p.borrow().is_some_and(|t| now - t < DOUBLE_PRESS);
            *p.borrow_mut() = if double { None } else { Some(now) };
            double
        });
        if double {
            ui.invoke_toggle_maximize();
        } else {
            ui.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
    });
    let w = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = w.upgrade() {
            ui.window().set_minimized(true);
        }
    });
    let w = ui.as_weak();
    ui.on_toggle_maximize(move || {
        if let Some(ui) = w.upgrade() {
            let maximized = !ui.window().is_maximized();
            ui.window().set_maximized(maximized);
            ui.set_max_state(maximized);
        }
    });
    let w = ui.as_weak();
    ui.on_resize(move |dir| {
        let Some(ui) = w.upgrade() else { return };
        let dir = match dir {
            0 => ResizeDirection::North,
            1 => ResizeDirection::South,
            2 => ResizeDirection::East,
            3 => ResizeDirection::West,
            4 => ResizeDirection::NorthEast,
            5 => ResizeDirection::NorthWest,
            6 => ResizeDirection::SouthEast,
            _ => ResizeDirection::SouthWest,
        };
        ui.window().with_winit_window(|w| {
            let _ = w.drag_resize_window(dir);
        });
    });

    // Keeps the maximise button right when the system changes the state
    // (Win+Up, snapping).
    let w = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(300), move || {
        if let Some(ui) = w.upgrade() {
            let maximized = ui.window().is_maximized();
            if ui.get_max_state() != maximized {
                ui.set_max_state(maximized);
            }
        }
    });
    std::mem::forget(timer);
}

/// Creates the tray once the event loop runs and routes its events.
#[cfg(any(windows, target_os = "macos"))]
fn install_tray(ui: &MainWindow, tx: mpsc::Sender<Cmd>) {
    use tray_icon::menu::MenuEvent;
    use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};

    fn act(ui: &MainWindow, action: tray::Action, tx: &mpsc::Sender<Cmd>) {
        match action {
            tray::Action::Show => show_window(ui),
            tray::Action::ToggleMute(i) => ui.invoke_strip_mute(i as i32),
            tray::Action::ToggleEngine => {
                let _ = tx.send(if ui.get_connected() { Cmd::StopEngine } else { Cmd::StartEngine });
            }
            tray::Action::Quit => {
                let _ = slint::quit_event_loop();
            }
        }
    }

    let w = ui.as_weak();
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let (w, tx) = (w.clone(), menu_tx.clone());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            let action = TRAY.with(|t| t.borrow().as_ref().and_then(|t| t.action_for(&event.id)));
            if let Some(action) = action {
                act(&ui, action, &tx);
            }
        });
    }));
    let w = ui.as_weak();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let show = matches!(
            event,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
        );
        if show {
            let w = w.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    show_window(&ui);
                }
            });
        }
    }));

    let w = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::ZERO, move || match tray::Tray::new() {
        Ok(tray) => {
            TRAY.with(|t| *t.borrow_mut() = Some(tray));
            if let Some(ui) = w.upgrade() {
                update_tray(&ui);
            }
        }
        Err(e) => {
            // Without a tray, closing the window must quit and the window
            // must be visible.
            eprintln!("no tray icon: {e}");
            if let Some(ui) = w.upgrade() {
                let mut st = ui.get_settings();
                st.has_tray = false;
                ui.set_settings(st);
                show_window(&ui);
            }
        }
    });
}
