//! Global keyboard shortcuts.
//!
//! * [`Combo`]: a key with modifiers such as `Ctrl+Alt+M`, written and parsed
//!   in a platform-neutral text form (stored in the configuration, typed in
//!   the CLI, shown in the GUI).
//! * [`Listener`]: system-wide hotkeys that fire while any application has
//!   the focus, including key release (for push-to-talk). Used by the engine,
//!   which keeps running when the GUI is closed.
//! * [`capture`]: records the next key combination the user presses
//!   anywhere (for the GUI's "record" button), independent of the keyboard
//!   layout.
//!
//! # Platforms
//! Windows: `RegisterHotKey` on a dedicated message-loop thread; release is
//! detected by polling `GetAsyncKeyState` every 15 ms while a hotkey is held.
//! Recording uses a low-level keyboard hook that exists only while
//! [`capture`] runs. Other platforms: [`Error::Unsupported`] (see
//! `docs/feature-matrix.md`).
//!
//! # Unsafe code
//! This crate opts out of the workspace-wide `unsafe_code` denial because it
//! calls the Win32 API; the invariants are documented in the `windows`
//! module. The key model is safe, portable code.
#![cfg_attr(windows, allow(unsafe_code))]

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

#[cfg(windows)]
mod windows;

/// Whether global hotkeys are implemented on this platform.
pub const SUPPORTED: bool = cfg!(windows);

/// Errors from the hotkey system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not implemented on this platform.
    Unsupported,
    /// An operating system call failed.
    Os(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => f.write_str("global hotkeys are not implemented on this platform yet"),
            Self::Os(m) => write!(f, "hotkey system: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// A key that can be part of a [`Combo`]: its canonical name and its Windows
/// virtual-key code. Other platforms map the name themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(u8);

/// `(name, Windows virtual-key code)`, in display order.
const KEYS: &[(&str, u16)] = &[
    ("A", 0x41),
    ("B", 0x42),
    ("C", 0x43),
    ("D", 0x44),
    ("E", 0x45),
    ("F", 0x46),
    ("G", 0x47),
    ("H", 0x48),
    ("I", 0x49),
    ("J", 0x4A),
    ("K", 0x4B),
    ("L", 0x4C),
    ("M", 0x4D),
    ("N", 0x4E),
    ("O", 0x4F),
    ("P", 0x50),
    ("Q", 0x51),
    ("R", 0x52),
    ("S", 0x53),
    ("T", 0x54),
    ("U", 0x55),
    ("V", 0x56),
    ("W", 0x57),
    ("X", 0x58),
    ("Y", 0x59),
    ("Z", 0x5A),
    ("0", 0x30),
    ("1", 0x31),
    ("2", 0x32),
    ("3", 0x33),
    ("4", 0x34),
    ("5", 0x35),
    ("6", 0x36),
    ("7", 0x37),
    ("8", 0x38),
    ("9", 0x39),
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("F13", 0x7C),
    ("F14", 0x7D),
    ("F15", 0x7E),
    ("F16", 0x7F),
    ("F17", 0x80),
    ("F18", 0x81),
    ("F19", 0x82),
    ("F20", 0x83),
    ("F21", 0x84),
    ("F22", 0x85),
    ("F23", 0x86),
    ("F24", 0x87),
    ("Num0", 0x60),
    ("Num1", 0x61),
    ("Num2", 0x62),
    ("Num3", 0x63),
    ("Num4", 0x64),
    ("Num5", 0x65),
    ("Num6", 0x66),
    ("Num7", 0x67),
    ("Num8", 0x68),
    ("Num9", 0x69),
    ("NumMultiply", 0x6A),
    ("NumAdd", 0x6B),
    ("NumSubtract", 0x6D),
    ("NumDecimal", 0x6E),
    ("NumDivide", 0x6F),
    ("Space", 0x20),
    ("Enter", 0x0D),
    ("Tab", 0x09),
    ("Escape", 0x1B),
    ("Backspace", 0x08),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Left", 0x25),
    ("Up", 0x26),
    ("Right", 0x27),
    ("Down", 0x28),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
];

/// Other spellings accepted when parsing (lower case).
const ALIASES: &[(&str, &str)] = &[
    ("esc", "Escape"),
    ("return", "Enter"),
    ("del", "Delete"),
    ("ins", "Insert"),
    ("pgup", "PageUp"),
    ("pgdn", "PageDown"),
    ("pagedn", "PageDown"),
    ("break", "Pause"),
    ("scroll", "ScrollLock"),
    ("numpad0", "Num0"),
    ("numpad1", "Num1"),
    ("numpad2", "Num2"),
    ("numpad3", "Num3"),
    ("numpad4", "Num4"),
    ("numpad5", "Num5"),
    ("numpad6", "Num6"),
    ("numpad7", "Num7"),
    ("numpad8", "Num8"),
    ("numpad9", "Num9"),
    ("num*", "NumMultiply"),
    ("num+", "NumAdd"),
    ("num-", "NumSubtract"),
    ("num.", "NumDecimal"),
    ("num/", "NumDivide"),
];

impl Key {
    /// Canonical name, e.g. `"M"`, `"F13"`, `"Num5"`.
    #[must_use]
    pub fn name(self) -> &'static str {
        KEYS[usize::from(self.0)].0
    }

    /// Windows virtual-key code.
    #[must_use]
    pub fn windows_vk(self) -> u16 {
        KEYS[usize::from(self.0)].1
    }

    /// The key with this Windows virtual-key code, if supported.
    #[must_use]
    pub fn from_windows_vk(vk: u16) -> Option<Self> {
        KEYS.iter().position(|k| k.1 == vk).map(Self::index)
    }

    /// The key with this name (case-insensitive; aliases such as `Esc`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.trim().to_ascii_lowercase();
        let canonical = ALIASES.iter().find(|a| a.0 == lower).map_or(lower.as_str(), |a| a.1);
        KEYS.iter().position(|k| k.0.eq_ignore_ascii_case(canonical)).map(Self::index)
    }

    /// Every supported key, in display order.
    pub fn all() -> impl Iterator<Item = Key> {
        (0..KEYS.len()).map(Self::index)
    }

    fn index(i: usize) -> Self {
        Self(u8::try_from(i).expect("key table fits in u8"))
    }

    /// Keys that may be bound without a modifier: they are not used for
    /// typing, so taking them system-wide does not break text input.
    #[must_use]
    pub fn standalone(self) -> bool {
        let n = self.name();
        (n.starts_with('F') && n.len() > 1 && n[1..].bytes().all(|b| b.is_ascii_digit()))
            || n.starts_with("Num")
            || n == "Pause"
            || n == "ScrollLock"
    }
}

/// A key with modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The Windows key (Super, Command).
    pub win: bool,
    pub key: Key,
}

impl Combo {
    /// Whether any modifier is held.
    #[must_use]
    pub fn has_modifier(&self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }

    /// Checks that binding this system-wide does not take away a key used
    /// for typing: letters, digits, Space, Enter and similar need a
    /// modifier other than Shift.
    ///
    /// # Errors
    /// A message explaining why the combination is not allowed.
    pub fn check_bindable(&self) -> Result<(), String> {
        if self.key.standalone() || self.ctrl || self.alt || self.win {
            Ok(())
        } else {
            Err(format!(
                "{self} would take the key away from every other application; add Ctrl, Alt or Win, \
                 or use F13-F24, Pause, ScrollLock or a numpad key"
            ))
        }
    }
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [(self.ctrl, "Ctrl+"), (self.alt, "Alt+"), (self.shift, "Shift+"), (self.win, "Win+")] {
            if on {
                f.write_str(name)?;
            }
        }
        f.write_str(self.key.name())
    }
}

impl FromStr for Combo {
    type Err = String;

    /// Parses `Ctrl+Alt+M`, `shift+f13`, `Win+Num5` (case-insensitive,
    /// modifiers in any order, spaces allowed).
    fn from_str(s: &str) -> Result<Self, String> {
        let (mut ctrl, mut alt, mut shift, mut win) = (false, false, false, false);
        let mut key = None;
        let parts: Vec<&str> = s.split('+').map(str::trim).collect();
        // `Num+` ends in an empty part after the split.
        let parts: Vec<String> = if parts.len() >= 2 && parts[parts.len() - 1].is_empty() {
            let mut p: Vec<String> = parts[..parts.len() - 2].iter().map(|s| (*s).to_owned()).collect();
            p.push(format!("{}+", parts[parts.len() - 2]));
            p
        } else {
            parts.iter().map(|s| (*s).to_owned()).collect()
        };
        for part in &parts {
            let flag = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" | "strg" => &mut ctrl,
                "alt" => &mut alt,
                "shift" => &mut shift,
                "win" | "super" | "meta" | "cmd" => &mut win,
                _ => {
                    if key.is_some() {
                        return Err(format!("'{s}' names more than one key"));
                    }
                    key = Some(Key::from_name(part).ok_or_else(|| format!("unknown key '{part}' in '{s}'"))?);
                    continue;
                }
            };
            *flag = true;
        }
        let key = key.ok_or_else(|| format!("'{s}' has no key, only modifiers"))?;
        Ok(Self { ctrl, alt, shift, win, key })
    }
}

/// A hotkey was pressed or released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// The id given to [`Listener::register`].
    pub id: u32,
    /// `true` when pressed, `false` when released.
    pub pressed: bool,
}

/// Outcome of registering one hotkey: `Err` explains why it is inactive
/// (usually: another application already uses the combination).
pub type Registration = (u32, Result<(), String>);

/// System-wide hotkeys, served by a background thread until dropped.
#[derive(Debug)]
pub struct Listener {
    #[cfg(windows)]
    inner: windows::Listener,
}

impl Listener {
    /// Starts the listener thread. `on_event` runs on that thread and must
    /// return quickly (forward the event, do not do the work there).
    ///
    /// # Errors
    /// [`Error::Unsupported`] off Windows, or if the thread cannot start.
    pub fn start(on_event: impl FnMut(Event) + Send + 'static) -> Result<Self, Error> {
        #[cfg(windows)]
        {
            windows::Listener::start(Box::new(on_event)).map(|inner| Self { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = on_event;
            Err(Error::Unsupported)
        }
    }

    /// Replaces all registered hotkeys with `bindings` and reports for each
    /// whether it is active.
    #[must_use]
    pub fn register(&self, bindings: &[(u32, Combo)]) -> Vec<Registration> {
        #[cfg(windows)]
        {
            self.inner.register(bindings)
        }
        #[cfg(not(windows))]
        {
            bindings.iter().map(|(id, _)| (*id, Err(Error::Unsupported.to_string()))).collect()
        }
    }
}

/// Waits up to `timeout` for the user to press a key combination anywhere
/// and returns it; the key press is swallowed. `Ok(None)`: timed out, or the
/// user pressed Escape alone (cancel). Blocks the calling thread; run it on
/// a background thread.
///
/// # Errors
/// [`Error::Unsupported`] off Windows, or [`Error::Os`].
pub fn capture(timeout: Duration) -> Result<Option<Combo>, Error> {
    #[cfg(windows)]
    {
        windows::capture(timeout)
    }
    #[cfg(not(windows))]
    {
        let _ = timeout;
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos_parse_and_print_canonically() {
        let c: Combo = "alt + ctrl+m".parse().unwrap();
        assert_eq!(c.to_string(), "Ctrl+Alt+M");
        assert_eq!(c.key.windows_vk(), 0x4D);
        let c: Combo = "Shift+Win+numpad5".parse().unwrap();
        assert_eq!(c.to_string(), "Shift+Win+Num5");
        assert_eq!("f13".parse::<Combo>().unwrap().to_string(), "F13");
        assert_eq!("Ctrl+Esc".parse::<Combo>().unwrap().key.name(), "Escape");
        assert_eq!("Ctrl+Num+".parse::<Combo>().unwrap().to_string(), "Ctrl+NumAdd");
        assert_eq!("Num+".parse::<Combo>().unwrap().to_string(), "NumAdd");
        for c in ["Ctrl+Alt+M", "F24", "Alt+Shift+PageDown", "Win+0"] {
            assert_eq!(c.parse::<Combo>().unwrap().to_string(), c, "roundtrip");
        }
    }

    #[test]
    fn bad_combos_are_rejected() {
        assert!("Ctrl+Alt".parse::<Combo>().is_err());
        assert!("Ctrl+M+N".parse::<Combo>().is_err());
        assert!("Ctrl+Hyper".parse::<Combo>().is_err());
        assert!("".parse::<Combo>().is_err());
    }

    #[test]
    fn typing_keys_need_a_modifier() {
        let ok = |s: &str| s.parse::<Combo>().unwrap().check_bindable().is_ok();
        assert!(!ok("M"));
        assert!(!ok("Shift+M"), "Shift alone still types");
        assert!(!ok("Space"));
        assert!(ok("Ctrl+M"));
        assert!(ok("Win+Space"));
        assert!(ok("F13"));
        assert!(ok("Pause"));
        assert!(ok("Num7"));
        assert!(!Key::from_name("F").unwrap().standalone(), "the letter F is not a function key");
    }

    #[test]
    fn every_key_has_a_unique_name_and_code() {
        let keys: Vec<Key> = Key::all().collect();
        for (i, a) in keys.iter().enumerate() {
            assert_eq!(Key::from_name(a.name()), Some(*a));
            assert_eq!(Key::from_windows_vk(a.windows_vk()), Some(*a));
            for b in &keys[i + 1..] {
                assert_ne!(a.windows_vk(), b.windows_vk());
            }
        }
    }
}
