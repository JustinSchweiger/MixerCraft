//! System tray icon and menu (Windows and macOS).
//!
//! Lives on the UI thread. Left click opens the mixer; the context menu
//! offers per-input mute, engine start/stop and quitting the window. Quitting
//! the window never stops audio — the engine is a separate process.

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// What a tray interaction asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Show,
    ToggleMute(usize),
    ToggleEngine,
    Quit,
}

const ICON_PNG: &[u8] = include_bytes!("../ui/icon.png");

fn load_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Icon::from_rgba(buf, info.width, info.height).ok()
}

/// Menu item ids of the current menu.
#[derive(Debug, Default)]
struct Ids {
    open: Option<MenuId>,
    engine: Option<MenuId>,
    quit: Option<MenuId>,
    mutes: Vec<MenuId>,
}

/// The tray icon with its menu.
pub struct Tray {
    icon: TrayIcon,
    ids: Ids,
    mute_items: Vec<CheckMenuItem>,
    /// Names the menu was built for, and whether the engine was connected.
    built_for: Option<(Vec<String>, bool)>,
    tooltip: String,
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray").field("tooltip", &self.tooltip).finish_non_exhaustive()
    }
}

impl Tray {
    /// Creates the tray icon. Must run on the UI thread after the event loop
    /// has started (a macOS requirement).
    ///
    /// # Errors
    /// If the platform refuses the icon.
    pub fn new() -> Result<Self, String> {
        let mut builder =
            TrayIconBuilder::new().with_tooltip("MixerCraft").with_menu_on_left_click(false).with_id("mixercraft");
        if let Some(icon) = load_icon() {
            builder = builder.with_icon(icon);
        }
        let icon = builder.build().map_err(|e| e.to_string())?;
        let mut tray =
            Self { icon, ids: Ids::default(), mute_items: Vec::new(), built_for: None, tooltip: String::new() };
        tray.update(&[], false, "MixerCraft — connecting…");
        Ok(tray)
    }

    /// Reflects the current state. Rebuilds the menu only when the strip
    /// names or the connection state changed.
    pub fn update(&mut self, strips: &[(String, bool)], connected: bool, tooltip: &str) {
        let names: Vec<String> = strips.iter().map(|(n, _)| n.clone()).collect();
        if self.built_for.as_ref() != Some(&(names.clone(), connected)) {
            self.rebuild(strips, connected);
            self.built_for = Some((names, connected));
        } else {
            for (item, (_, muted)) in self.mute_items.iter().zip(strips) {
                if item.is_checked() != *muted {
                    item.set_checked(*muted);
                }
            }
        }
        if self.tooltip != tooltip {
            let _ = self.icon.set_tooltip(Some(tooltip));
            self.tooltip = tooltip.to_owned();
        }
    }

    fn rebuild(&mut self, strips: &[(String, bool)], connected: bool) {
        let menu = Menu::new();
        let open = MenuItem::new("Open mixer", true, None);
        let engine = MenuItem::new(if connected { "Stop engine" } else { "Start engine" }, true, None);
        let quit = MenuItem::new("Quit (audio keeps running)", true, None);
        let mute_items: Vec<CheckMenuItem> = if connected {
            strips.iter().map(|(name, muted)| CheckMenuItem::new(format!("Mute {name}"), true, *muted, None)).collect()
        } else {
            Vec::new()
        };
        let _ = menu.append(&open);
        let _ = menu.append(&PredefinedMenuItem::separator());
        for item in &mute_items {
            let _ = menu.append(item);
        }
        if !mute_items.is_empty() {
            let _ = menu.append(&PredefinedMenuItem::separator());
        }
        let _ = menu.append(&engine);
        let _ = menu.append(&quit);
        self.ids = Ids {
            open: Some(open.id().clone()),
            engine: Some(engine.id().clone()),
            quit: Some(quit.id().clone()),
            mutes: mute_items.iter().map(|m| m.id().clone()).collect(),
        };
        self.mute_items = mute_items;
        self.icon.set_menu(Some(Box::new(menu)));
    }

    /// Maps a clicked menu item to an action.
    #[must_use]
    pub fn action_for(&self, id: &MenuId) -> Option<Action> {
        let is = |x: &Option<MenuId>| x.as_ref() == Some(id);
        if is(&self.ids.open) {
            Some(Action::Show)
        } else if is(&self.ids.engine) {
            Some(Action::ToggleEngine)
        } else if is(&self.ids.quit) {
            Some(Action::Quit)
        } else {
            self.ids.mutes.iter().position(|m| m == id).map(Action::ToggleMute)
        }
    }
}
