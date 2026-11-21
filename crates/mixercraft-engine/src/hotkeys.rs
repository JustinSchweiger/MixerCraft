//! Global keyboard shortcuts of the engine.
//!
//! The engine owns the system-wide hotkey listener because it is the process
//! that keeps running when the GUI is closed or hidden in the tray. The
//! listener thread only forwards key events as
//! [`Request::TriggerHotkey`](mixercraft_ipc::Request::TriggerHotkey) into
//! the supervisor's request queue; the supervisor runs the action through the
//! same path as an IPC request (ramped gains, persisted state) and asks the
//! real-time core for the confirmation tone.

use std::collections::HashMap;

use mixercraft_config::{Config, HotkeyAction, HotkeyBinding, HotkeyMode, HotkeysConfig};
use mixercraft_hotkey::{Combo, Listener};
use mixercraft_ipc as ipc;
use mixercraft_platform::capabilities::{self, Feature};
use tracing::{info, warn};

/// Fader range reachable with gain-step shortcuts (dB).
pub const STEP_MIN_DB: f32 = -60.0;
/// Upper end of the fader range (dB).
pub const STEP_MAX_DB: f32 = 12.0;

/// The listener and the state of the registered shortcuts.
#[derive(Debug)]
pub struct Hotkeys {
    listener: Option<Listener>,
    /// Why shortcuts do not work (empty when they do).
    note: String,
    /// Registration result per binding id.
    registered: HashMap<u32, Result<(), String>>,
    /// Hold-mode shortcuts currently held: id → value to restore on release.
    held: HashMap<u32, bool>,
}

impl Hotkeys {
    /// Wraps the listener the engine started (or why it could not).
    #[must_use]
    pub fn new(listener: Result<Listener, mixercraft_hotkey::Error>) -> Self {
        match listener {
            Ok(l) => Self { listener: Some(l), note: String::new(), registered: HashMap::new(), held: HashMap::new() },
            Err(mixercraft_hotkey::Error::Unsupported) => {
                Self::without_listener(capabilities::support(Feature::GlobalHotkeys).1)
            }
            Err(e) => {
                warn!("global hotkeys unavailable: {e}");
                Self::without_listener(&e.to_string())
            }
        }
    }

    /// No listener: bindings are stored and can be triggered over IPC, but
    /// keys do nothing.
    #[must_use]
    pub fn without_listener(note: &str) -> Self {
        Self { listener: None, note: note.into(), registered: HashMap::new(), held: HashMap::new() }
    }

    /// Registers the configured shortcuts with the system (replacing all).
    pub fn apply(&mut self, config: &HotkeysConfig) {
        let Some(listener) = &self.listener else { return };
        let bindings: Vec<(u32, Combo)> =
            config.bindings.iter().filter_map(|b| b.keys.parse().ok().map(|c| (b.id, c))).collect();
        self.registered = listener.register(&bindings).into_iter().collect();
        for b in &config.bindings {
            match self.registered.get(&b.id) {
                Some(Ok(())) => info!(keys = %b.keys, "hotkey registered"),
                Some(Err(e)) => warn!(keys = %b.keys, "hotkey inactive: {e}"),
                None => {}
            }
        }
        self.held.retain(|id, _| config.bindings.iter().any(|b| b.id == *id));
    }

    /// Remembers the value a hold-mode shortcut restores on release; `false`
    /// if it is already held (repeated press).
    pub fn hold(&mut self, id: u32, previous: bool) -> bool {
        if self.held.contains_key(&id) {
            return false;
        }
        self.held.insert(id, previous);
        true
    }

    /// The value to restore when a hold-mode shortcut is released.
    pub fn release(&mut self, id: u32) -> Option<bool> {
        self.held.remove(&id)
    }

    /// Status for clients.
    #[must_use]
    pub fn status(&self, config: &Config) -> ipc::HotkeysStatus {
        let h = &config.hotkeys;
        ipc::HotkeysStatus {
            available: self.listener.is_some(),
            note: self.note.clone(),
            bindings: h
                .bindings
                .iter()
                .filter_map(|b| {
                    let (active, error) = match self.registered.get(&b.id) {
                        Some(Ok(())) => (true, None),
                        Some(Err(e)) => (false, Some(e.clone())),
                        None if self.listener.is_none() => (false, None),
                        None => (false, Some("not registered".into())),
                    };
                    Some(ipc::HotkeyStatus { hotkey: to_ipc(b, config)?, active, error })
                })
                .collect(),
            feedback: ipc::HotkeyFeedback {
                bus: h.feedback_bus.as_deref().and_then(|id| config.bus_index(id)).and_then(|i| u16::try_from(i).ok()),
                level_db: h.feedback_level_db,
            },
        }
    }
}

fn bus_index(config: &Config, id: &str) -> Option<u16> {
    config.bus_index(id).and_then(|i| u16::try_from(i).ok())
}

/// A configured binding in IPC form (`None` if its bus vanished, which a
/// valid configuration rules out).
#[must_use]
pub fn to_ipc(b: &HotkeyBinding, config: &Config) -> Option<ipc::Hotkey> {
    let action = match &b.action {
        HotkeyAction::StripMute { strip } => ipc::HotkeyAction::StripMute { strip: *strip },
        HotkeyAction::StripSolo { strip } => ipc::HotkeyAction::StripSolo { strip: *strip },
        HotkeyAction::StripGain { strip, step_db } => ipc::HotkeyAction::StripGain { strip: *strip, step_db: *step_db },
        HotkeyAction::BusMute { bus } => ipc::HotkeyAction::BusMute { bus: bus_index(config, bus)? },
        HotkeyAction::BusGain { bus, step_db } => {
            ipc::HotkeyAction::BusGain { bus: bus_index(config, bus)?, step_db: *step_db }
        }
    };
    let mode = match b.mode {
        HotkeyMode::Toggle => ipc::HotkeyMode::Toggle,
        HotkeyMode::Hold => ipc::HotkeyMode::Hold,
    };
    Some(ipc::Hotkey { id: b.id, keys: b.keys.clone(), mode, action })
}

/// An IPC binding in configuration form, with the keys written canonically
/// (`alt+ctrl+m` → `Ctrl+Alt+M`). The id is taken as given.
///
/// # Errors
/// Unknown keys or a bus index that does not exist.
pub fn from_ipc(h: &ipc::Hotkey, config: &Config) -> Result<HotkeyBinding, String> {
    let combo: Combo = h.keys.parse()?;
    let bus_id =
        |i: u16| config.buses.get(usize::from(i)).map(|b| b.id.clone()).ok_or_else(|| "no such bus".to_string());
    let action = match &h.action {
        ipc::HotkeyAction::StripMute { strip } => HotkeyAction::StripMute { strip: *strip },
        ipc::HotkeyAction::StripSolo { strip } => HotkeyAction::StripSolo { strip: *strip },
        ipc::HotkeyAction::StripGain { strip, step_db } => HotkeyAction::StripGain { strip: *strip, step_db: *step_db },
        ipc::HotkeyAction::BusMute { bus } => HotkeyAction::BusMute { bus: bus_id(*bus)? },
        ipc::HotkeyAction::BusGain { bus, step_db } => HotkeyAction::BusGain { bus: bus_id(*bus)?, step_db: *step_db },
    };
    let mode = match h.mode {
        ipc::HotkeyMode::Toggle => HotkeyMode::Toggle,
        ipc::HotkeyMode::Hold => HotkeyMode::Hold,
    };
    Ok(HotkeyBinding { id: h.id, keys: combo.to_string(), mode, action })
}

/// The fader value after a gain step, kept within the fader range.
#[must_use]
pub fn stepped_gain(current_db: f32, step_db: f32) -> f32 {
    (current_db.max(STEP_MIN_DB) + step_db).clamp(STEP_MIN_DB, STEP_MAX_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_steps_stay_in_the_fader_range() {
        assert_eq!(stepped_gain(0.0, 3.0), 3.0);
        assert_eq!(stepped_gain(11.0, 3.0), 12.0);
        assert_eq!(stepped_gain(-200.0, 3.0), -57.0, "from -inf the step starts at the bottom of the range");
        assert_eq!(stepped_gain(-58.0, -6.0), -60.0);
    }

    #[test]
    fn hold_remembers_the_first_press_only() {
        let mut h = Hotkeys::without_listener("test");
        assert!(h.hold(1, true));
        assert!(!h.hold(1, false), "repeat while held");
        assert_eq!(h.release(1), Some(true));
        assert_eq!(h.release(1), None);
    }
}
