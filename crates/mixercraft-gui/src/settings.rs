//! Preferences of the desktop app itself (not the engine).
//!
//! Stored as `gui.toml` next to the engine configuration (in the directory
//! of `MIXERCRAFT_CONFIG` when that is set, else the per-user configuration
//! directory). Missing or unreadable files give the defaults; unknown keys
//! are ignored so older and newer versions of the app can share the file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the configuration directory.
const FILE_NAME: &str = "gui.toml";

/// App preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiSettings {
    /// The window's close button hides the window into the tray (`true`,
    /// audio keeps running) or quits the app, which also stops the engine.
    pub close_to_tray: bool,
    /// Meters keep a peak marker for a moment.
    pub peak_hold: bool,
    /// Where the window was last, restored at start.
    pub window: Option<WindowGeometry>,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self { close_to_tray: true, peak_hold: true, window: None }
    }
}

/// The window's normal (not maximised) position and size in physical
/// pixels, and whether it was maximised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

/// Where the preferences live, if there is a configuration directory.
#[must_use]
pub fn path() -> Option<PathBuf> {
    let config = std::env::var_os("MIXERCRAFT_CONFIG")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(mixercraft_config::default_path)?;
    Some(config.parent().map_or_else(|| PathBuf::from(FILE_NAME), |dir| dir.join(FILE_NAME)))
}

/// Reads the preferences; any problem gives the defaults.
#[must_use]
pub fn load(path: &Path) -> GuiSettings {
    std::fs::read_to_string(path).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
}

/// Writes the preferences atomically (temporary file, then rename).
///
/// # Errors
/// I/O failures; the previous file is then untouched.
pub fn save(path: &Path, settings: &GuiSettings) -> std::io::Result<()> {
    let text = toml::to_string(settings).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_broken_files_give_defaults_and_unknown_keys_are_ignored() {
        let dir = std::env::temp_dir().join(format!("mixercraft-gui-settings-{}", std::process::id()));
        let file = dir.join(FILE_NAME);
        assert_eq!(load(&file), GuiSettings::default());

        let s = GuiSettings {
            close_to_tray: false,
            peak_hold: false,
            window: Some(WindowGeometry { x: -1200, y: 40, width: 1240, height: 780, maximized: true }),
        };
        save(&file, &s).unwrap();
        assert_eq!(load(&file), s);

        std::fs::write(&file, "peak_hold = false\nfuture_key = 3\n").unwrap();
        assert_eq!(load(&file), GuiSettings { close_to_tray: true, peak_hold: false, window: None });

        std::fs::write(&file, "not toml [").unwrap();
        assert_eq!(load(&file), GuiSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
