//! The Hotkeys tab: list, record and edit the engine's global keyboard
//! shortcuts and choose their confirmation tone.
//!
//! The engine listens for the keys (it keeps running when this window is
//! closed); this page only edits the bindings over IPC. Recording uses
//! [`mixercraft_hotkey::capture`], which reads the key layout-independently
//! (Ctrl+Alt+M on a German layout would otherwise arrive as "µ"). It runs in
//! a short-lived copy of this program ([`RECORD_ARG`]): a keyboard hook in
//! the process whose window has the focus did not see the keys (observed on
//! Windows 11), and this window has the focus while the user records.

use std::cell::RefCell;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use mixercraft_ipc::{EngineStatus, Hotkey, HotkeyAction, HotkeyFeedback, HotkeyMode};
use slint::{ComponentHandle, Model as _, ModelRc, SharedString, VecModel};

use crate::engine::Cmd;
use crate::ui::{HotkeyEditor, HotkeyEvent, HotkeyRow, HotkeysState, MainWindow};

/// Fader steps offered for volume shortcuts (dB).
const STEPS: [f32; 6] = [-6.0, -3.0, -1.0, 1.0, 3.0, 6.0];
/// Default step: +3 dB.
const DEFAULT_STEP: i32 = 4;
/// Confirmation tone levels: quiet, medium, loud (dBFS peak).
const LEVELS: [f32; 3] = [-32.0, -20.0, -10.0];
/// How long the recorder waits for a key.
const RECORD_TIMEOUT: Duration = Duration::from_secs(10);
/// Command-line argument that makes this program record one key
/// combination, print it and exit (no window).
pub const RECORD_ARG: &str = "--record-hotkey";

/// What the recorder process prints: `keys <combo>`, `none` or
/// `error <message>`.
pub fn record_child() {
    match mixercraft_hotkey::capture(RECORD_TIMEOUT) {
        Ok(Some(combo)) => println!("keys {combo}"),
        Ok(None) => println!("none"),
        Err(e) => println!("error {e}"),
    }
}

/// Runs the recorder process and reads its answer.
fn record_in_child() -> Result<Option<mixercraft_hotkey::Combo>, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(RECORD_ARG).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().map_err(|e| format!("cannot start recording: {e}"))?;
    parse_record_output(&String::from_utf8_lossy(&out.stdout))
}

fn parse_record_output(text: &str) -> Result<Option<mixercraft_hotkey::Combo>, String> {
    let line = text.lines().next().unwrap_or("").trim();
    if let Some(keys) = line.strip_prefix("keys ") {
        keys.parse().map(Some)
    } else if line == "none" {
        Ok(None)
    } else if let Some(e) = line.strip_prefix("error ") {
        Err(e.to_owned())
    } else {
        Err("recording failed".into())
    }
}

/// What the last status said, for building the editor and requests.
#[derive(Debug, Default)]
struct Known {
    strips: Vec<String>,
    /// Bus labels ("A1 · Main").
    buses: Vec<String>,
    hotkeys: Vec<Hotkey>,
    feedback: HotkeyFeedback,
}

thread_local! {
    static KNOWN: RefCell<Known> = RefCell::new(Known::default());
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

fn strings(v: &[String]) -> ModelRc<SharedString> {
    model(v.iter().map(SharedString::from).collect())
}

/// Sets up the tab and its callback.
pub fn install(ui: &MainWindow, tx: mpsc::Sender<Cmd>) {
    ui.set_hotkeys(HotkeysState {
        level: 1,
        feedback_text: "Off".into(),
        feedback_options: strings(&["Off".into()]),
        rows: model(Vec::new()),
        editor: HotkeyEditor { targets: model(Vec::new()), step: DEFAULT_STEP, ..HotkeyEditor::default() },
        ..HotkeysState::default()
    });
    let w = ui.as_weak();
    ui.on_hotkey_event(move |kind, value| {
        if let Some(ui) = w.upgrade() {
            on_event(&ui, &tx, kind, value);
        }
    });
}

fn action_index(a: &HotkeyAction) -> (i32, u16, f32) {
    match *a {
        HotkeyAction::StripMute { strip } => (0, strip, 0.0),
        HotkeyAction::StripSolo { strip } => (1, strip, 0.0),
        HotkeyAction::StripGain { strip, step_db } => (2, strip, step_db),
        HotkeyAction::BusMute { bus } => (3, bus, 0.0),
        HotkeyAction::BusGain { bus, step_db } => (4, bus, step_db),
    }
}

fn describe(a: &HotkeyAction, k: &Known) -> String {
    let strip = |i: u16| k.strips.get(usize::from(i)).cloned().unwrap_or_else(|| format!("Input {}", i + 1));
    let bus = |i: u16| k.buses.get(usize::from(i)).cloned().unwrap_or_else(|| format!("Output {}", i + 1));
    match *a {
        HotkeyAction::StripMute { strip: i } => format!("Mute {}", strip(i)),
        HotkeyAction::StripSolo { strip: i } => format!("Solo {}", strip(i)),
        HotkeyAction::StripGain { strip: i, step_db } => format!("{}  {step_db:+} dB", strip(i)),
        HotkeyAction::BusMute { bus: i } => format!("Mute {}", bus(i)),
        HotkeyAction::BusGain { bus: i, step_db } => format!("{}  {step_db:+} dB", bus(i)),
    }
}

/// Fills the editor's target list for its action, keeping the choice when
/// it still exists.
fn fill_targets(e: &mut HotkeyEditor, k: &Known) {
    let list = if e.action <= 2 { &k.strips } else { &k.buses };
    if usize::try_from(e.target).map_or(true, |t| t >= list.len()) {
        e.target = 0;
    }
    e.target_text = list.get(usize::try_from(e.target).unwrap_or(0)).cloned().unwrap_or_default().into();
    e.targets = strings(list);
}

/// Shows a fresh engine status on the tab.
pub fn apply_status(ui: &MainWindow, s: &EngineStatus) {
    let mut st = ui.get_hotkeys();
    st.engine_outdated = s.hotkeys.is_none();
    let Some(h) = &s.hotkeys else {
        ui.set_hotkeys(st);
        return;
    };
    KNOWN.with(|k| {
        let mut k = k.borrow_mut();
        k.strips = s.strips.iter().map(|x| x.name.clone()).collect();
        k.buses = s.buses.iter().map(|b| crate::bus_label(&b.id, &b.name)).collect();
        k.hotkeys = h.bindings.iter().map(|b| b.hotkey.clone()).collect();
        k.feedback = h.feedback;
        st.available = h.available;
        st.note = h.note.as_str().into();
        st.rows = model(
            h.bindings
                .iter()
                .map(|b| HotkeyRow {
                    id: i32::try_from(b.hotkey.id).unwrap_or(0),
                    keys: b.hotkey.keys.as_str().into(),
                    action: describe(&b.hotkey.action, &k).into(),
                    mode: if b.hotkey.mode == HotkeyMode::Hold { "While held".into() } else { SharedString::new() },
                    error: b.error.clone().unwrap_or_default().into(),
                })
                .collect(),
        );
        let mut options = vec!["Off".to_string()];
        options.extend(k.buses.iter().cloned());
        st.feedback_text =
            h.feedback.bus.and_then(|b| k.buses.get(usize::from(b)).cloned()).unwrap_or_else(|| "Off".into()).into();
        st.feedback_options = strings(&options);
        st.level = LEVELS.iter().position(|l| (l - h.feedback.level_db).abs() < 0.5).map_or(-1, |i| i as i32);
        fill_targets(&mut st.editor, &k);
    });
    ui.set_hotkeys(st);
}

fn editor_for(h: &Hotkey, k: &Known) -> HotkeyEditor {
    let (action, target, step_db) = action_index(&h.action);
    let step = STEPS
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1 - step_db).abs().total_cmp(&(b.1 - step_db).abs()))
        .map_or(DEFAULT_STEP, |(i, _)| i as i32);
    let mut e = HotkeyEditor {
        open: true,
        id: i32::try_from(h.id).unwrap_or(0),
        keys: h.keys.as_str().into(),
        action,
        target: i32::from(target),
        step: if action == 2 || action == 4 { step } else { DEFAULT_STEP },
        hold: h.mode == HotkeyMode::Hold,
        ..HotkeyEditor::default()
    };
    fill_targets(&mut e, k);
    e
}

/// The shortcut the editor describes, or why it cannot be saved.
fn hotkey_from(e: &HotkeyEditor) -> Result<Hotkey, String> {
    let combo: mixercraft_hotkey::Combo = e.keys.parse().map_err(|_| "Record a key combination first.".to_string())?;
    combo.check_bindable()?;
    if e.targets.row_count() == 0 {
        return Err("There is nothing to control yet.".into());
    }
    let t = u16::try_from(e.target).unwrap_or(0);
    let step_db = STEPS[usize::try_from(e.step).unwrap_or(4).min(STEPS.len() - 1)];
    let action = match e.action {
        0 => HotkeyAction::StripMute { strip: t },
        1 => HotkeyAction::StripSolo { strip: t },
        2 => HotkeyAction::StripGain { strip: t, step_db },
        3 => HotkeyAction::BusMute { bus: t },
        _ => HotkeyAction::BusGain { bus: t, step_db },
    };
    let gain = matches!(action, HotkeyAction::StripGain { .. } | HotkeyAction::BusGain { .. });
    let mode = if e.hold && !gain { HotkeyMode::Hold } else { HotkeyMode::Toggle };
    Ok(Hotkey { id: u32::try_from(e.id).unwrap_or(0), keys: combo.to_string(), mode, action })
}

fn edit(ui: &MainWindow, f: impl FnOnce(&mut HotkeyEditor)) {
    let mut st = ui.get_hotkeys();
    f(&mut st.editor);
    ui.set_hotkeys(st);
}

fn record(ui: &MainWindow) {
    if ui.get_hotkeys().editor.recording {
        return;
    }
    edit(ui, |e| {
        e.recording = true;
        e.error = SharedString::new();
    });
    let w = ui.as_weak();
    let spawned = std::thread::Builder::new().name("mixercraft-gui-record".into()).spawn(move || {
        let result = record_in_child();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = w.upgrade() else { return };
            edit(&ui, |e| {
                e.recording = false;
                match result {
                    Ok(Some(combo)) => {
                        e.keys = combo.to_string().into();
                        e.error = combo.check_bindable().err().unwrap_or_default().into();
                    }
                    Ok(None) => {}
                    Err(err) => e.error = err.into(),
                }
            });
        });
    });
    if spawned.is_err() {
        edit(ui, |e| {
            e.recording = false;
            e.error = "Cannot start recording.".into();
        });
    }
}

fn on_event(ui: &MainWindow, tx: &mpsc::Sender<Cmd>, kind: HotkeyEvent, value: i32) {
    let id = u32::try_from(value).unwrap_or(0);
    match kind {
        HotkeyEvent::Add => KNOWN.with(|k| {
            let mut e = HotkeyEditor { open: true, step: DEFAULT_STEP, ..HotkeyEditor::default() };
            fill_targets(&mut e, &k.borrow());
            edit(ui, |x| *x = e);
        }),
        HotkeyEvent::Edit => KNOWN.with(|k| {
            let k = k.borrow();
            if let Some(h) = k.hotkeys.iter().find(|h| h.id == id) {
                let e = editor_for(h, &k);
                edit(ui, |x| *x = e);
            }
        }),
        HotkeyEvent::Remove => {
            let _ = tx.send(Cmd::RemoveHotkey(id));
            edit(ui, |e| {
                if e.id == value {
                    e.open = false;
                }
            });
        }
        HotkeyEvent::Test => {
            let _ = tx.send(Cmd::TriggerHotkey(id, true));
            let _ = tx.send(Cmd::TriggerHotkey(id, false));
        }
        HotkeyEvent::Record => record(ui),
        HotkeyEvent::Cancel => edit(ui, |e| e.open = false),
        HotkeyEvent::Save => {
            let editor = ui.get_hotkeys().editor;
            if editor.recording {
                return;
            }
            match hotkey_from(&editor) {
                Ok(h) => {
                    let _ = tx.send(Cmd::SetHotkey(h));
                    edit(ui, |e| e.open = false);
                }
                Err(why) => edit(ui, |e| e.error = why.into()),
            }
        }
        HotkeyEvent::Action => KNOWN.with(|k| {
            edit(ui, |e| {
                let was_strip = e.action <= 2;
                e.action = value;
                if was_strip != (value <= 2) {
                    e.target = 0;
                }
                fill_targets(e, &k.borrow());
            });
        }),
        HotkeyEvent::Target => KNOWN.with(|k| {
            edit(ui, |e| {
                e.target = value;
                fill_targets(e, &k.borrow());
            });
        }),
        HotkeyEvent::Step => edit(ui, |e| e.step = value),
        HotkeyEvent::Hold => edit(ui, |e| e.hold = value == 1),
        HotkeyEvent::FeedbackBus | HotkeyEvent::FeedbackLevel => {
            let mut feedback = KNOWN.with(|k| k.borrow().feedback);
            if kind == HotkeyEvent::FeedbackBus {
                feedback.bus = value.checked_sub(1).and_then(|b| u16::try_from(b).ok());
            } else if let Some(l) = usize::try_from(value).ok().and_then(|i| LEVELS.get(i)) {
                feedback.level_db = *l;
            }
            KNOWN.with(|k| k.borrow_mut().feedback = feedback);
            let mut st = ui.get_hotkeys();
            st.level = LEVELS.iter().position(|l| (l - feedback.level_db).abs() < 0.5).map_or(-1, |i| i as i32);
            ui.set_hotkeys(st);
            let _ = tx.send(Cmd::HotkeyFeedback(feedback));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Known {
        Known {
            strips: vec!["Desktop".into(), "Mikro".into()],
            buses: vec!["A1 · Main".into(), "A3 · Discord".into()],
            ..Known::default()
        }
    }

    #[test]
    fn editor_roundtrips_a_hotkey() {
        let k = known();
        for (mode, action) in [
            (HotkeyMode::Hold, HotkeyAction::StripMute { strip: 1 }),
            (HotkeyMode::Toggle, HotkeyAction::StripSolo { strip: 0 }),
            (HotkeyMode::Toggle, HotkeyAction::BusGain { bus: 1, step_db: -3.0 }),
            (HotkeyMode::Toggle, HotkeyAction::StripGain { strip: 1, step_db: 6.0 }),
            (HotkeyMode::Hold, HotkeyAction::BusMute { bus: 0 }),
        ] {
            let h = Hotkey { id: 7, keys: "Ctrl+Alt+M".into(), mode, action };
            assert_eq!(hotkey_from(&editor_for(&h, &k)).unwrap(), h);
        }
    }

    #[test]
    fn recorder_output_is_parsed() {
        assert_eq!(parse_record_output("keys Ctrl+Alt+M\n").unwrap().unwrap().to_string(), "Ctrl+Alt+M");
        assert_eq!(parse_record_output("none\r\n"), Ok(None));
        assert_eq!(parse_record_output("error nope\n"), Err("nope".into()));
        assert!(parse_record_output("").is_err());
    }

    #[test]
    fn editor_rejects_missing_or_typing_keys() {
        let k = known();
        let mut e = HotkeyEditor { open: true, ..HotkeyEditor::default() };
        fill_targets(&mut e, &k);
        assert!(hotkey_from(&e).is_err(), "no keys yet");
        e.keys = "M".into();
        assert!(hotkey_from(&e).is_err(), "would take M away from typing");
        e.keys = "F13".into();
        assert_eq!(hotkey_from(&e).unwrap().action, HotkeyAction::StripMute { strip: 0 });
        assert_eq!(describe(&HotkeyAction::BusGain { bus: 1, step_db: -3.0 }, &k), "A3 · Discord  -3 dB");
    }
}
