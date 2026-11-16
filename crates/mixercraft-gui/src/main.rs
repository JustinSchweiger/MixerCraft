//! `mixercraft-gui` — the MixerCraft desktop mixer.
//!
//! A thin client: all audio runs in `mixercraftd`. The window can be closed,
//! crash or restart at any time without affecting audio.

// No console window for the GUI on Windows (release builds).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod engine;
mod fader_law;
mod meters;

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mixercraft_ipc::{EngineState, EngineStatus};
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
use ui::{DeviceRow, MainWindow, MeterView, StripView};

/// After a local edit, status polls do not overwrite that strip for this long
/// (the engine may not have applied the change yet).
const LOCAL_EDIT_GRACE: Duration = Duration::from_millis(1500);

thread_local! {
    static LOCAL_EDITS: RefCell<HashMap<(bool, usize), Instant>> = RefCell::new(HashMap::new());
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

fn strip_view(name: &str, source: &str, db: f32, mute: bool, solo: bool, routes: Vec<bool>) -> StripView {
    StripView {
        name: name.into(),
        source: source.into(),
        pos: fader_law::db_to_pos(db),
        db,
        db_text: fader_law::knob_text(db).into(),
        mute,
        solo,
        routes: ModelRc::new(VecModel::from(routes)),
    }
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
        }
        let same_routes = old.routes.iter().eq(row.routes.iter());
        let changed = old.name != row.name
            || old.source != row.source
            || old.pos != row.pos
            || old.db_text != row.db_text
            || old.mute != row.mute
            || old.solo != row.solo
            || !same_routes;
        if changed {
            model.set_row_data(i, row);
        }
    }
    None
}

fn apply_status(ui: &MainWindow, s: &EngineStatus) {
    ui.set_connected(true);

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
            strip_view(&st.name, &st.source, st.gain_db, st.mute, st.solo, routes)
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
        .map(|b| strip_view(&b.name, &format!("{} · {}", b.id, b.sink), b.gain_db, b.mute, false, Vec::new()))
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
    for (i, v) in values.iter().enumerate() {
        m.set_row_data(i, MeterView { l: v[0], r: v[1], lp: v[2], rp: v[3] });
    }
}

fn apply(ui: &MainWindow, update: Update) {
    match update {
        Update::Disconnected { hint } => {
            ui.set_connected(false);
            ui.set_status_text("Engine not running".into());
            ui.set_status_color(Color::from_rgb_u8(0x7c, 0x85, 0x92));
            ui.set_connection_hint(hint.into());
        }
        Update::Status(s) => apply_status(ui, &s),
        Update::Meters { strips, buses } => {
            apply_meters(&ui.get_strip_meters(), &strips);
            apply_meters(&ui.get_bus_meters(), &buses);
        }
    }
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
    let ui = MainWindow::new()?;
    ui.set_strips(ModelRc::new(VecModel::<StripView>::default()));
    ui.set_buses(ModelRc::new(VecModel::<StripView>::default()));
    ui.set_strip_meters(ModelRc::new(VecModel::<MeterView>::default()));
    ui.set_bus_meters(ModelRc::new(VecModel::<MeterView>::default()));
    ui.set_status_text("Connecting…".into());

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
    let t = tx;
    ui.on_start_engine(move || {
        let _ = t.send(Cmd::StartEngine);
    });

    ui.run()
}
