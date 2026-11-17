//! System tray icon and menu on Linux: a StatusNotifierItem over D-Bus
//! (`ksni`), shown by KDE Plasma, GNOME with the AppIndicator extension
//! (default on Ubuntu), Xfce, Cinnamon, MATE, Budgie and most panels. No
//! GTK. Same menu and interface as the Windows/macOS tray (`tray.rs`).
//!
//! The D-Bus service runs on its own thread; menu clicks call `on_action`
//! there, which forwards them to the UI thread. If the desktop has no
//! StatusNotifier host, [`Tray::new`] fails and the app behaves as without a
//! tray (closing the window quits).

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};

/// What a tray interaction asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Show,
    ToggleMute(usize),
    ToggleEngine,
    Quit,
}

const ICON_PNG: &[u8] = include_bytes!("../ui/icon.png");

/// The app icon as ARGB32 (network byte order), as StatusNotifierItem wants.
fn load_icon() -> Option<Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(ICON_PNG));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    for px in buf.as_chunks_mut::<4>().0 {
        px.rotate_right(1); // RGBA -> ARGB
    }
    Some(Icon { width: i32::try_from(info.width).ok()?, height: i32::try_from(info.height).ok()?, data: buf })
}

type OnAction = Box<dyn Fn(Action) + Send>;

/// State the D-Bus service reads when the host asks for it.
struct Sni {
    strips: Vec<(String, bool)>,
    connected: bool,
    tooltip: String,
    icon: Vec<Icon>,
    on_action: OnAction,
}

impl ksni::Tray for Sni {
    fn id(&self) -> String {
        "mixercraft".into()
    }

    fn title(&self) -> String {
        "MixerCraft".into()
    }

    /// Installed by the packages (hicolor theme); hosts fall back to the
    /// pixmap when the theme has no such icon.
    fn icon_name(&self) -> String {
        "mixercraft".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: self.tooltip.clone(), ..ToolTip::default() }
    }

    /// Left click opens the mixer.
    fn activate(&mut self, _x: i32, _y: i32) {
        (self.on_action)(Action::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: "Open mixer".into(),
                activate: Box::new(|t: &mut Self| (t.on_action)(Action::Show)),
                ..StandardItem::default()
            }
            .into(),
            MenuItem::Separator,
        ];
        if self.connected && !self.strips.is_empty() {
            for (i, (name, muted)) in self.strips.iter().enumerate() {
                items.push(
                    CheckmarkItem {
                        label: format!("Mute {name}"),
                        checked: *muted,
                        activate: Box::new(move |t: &mut Self| (t.on_action)(Action::ToggleMute(i))),
                        ..CheckmarkItem::default()
                    }
                    .into(),
                );
            }
            items.push(MenuItem::Separator);
        }
        items.push(
            StandardItem {
                label: if self.connected { "Stop engine" } else { "Start engine" }.into(),
                activate: Box::new(|t: &mut Self| (t.on_action)(Action::ToggleEngine)),
                ..StandardItem::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Quit MixerCraft (stops audio)".into(),
                activate: Box::new(|t: &mut Self| (t.on_action)(Action::Quit)),
                ..StandardItem::default()
            }
            .into(),
        );
        items
    }
}

/// The tray icon with its menu.
pub struct Tray {
    handle: Handle<Sni>,
    shown: (Vec<(String, bool)>, bool, String),
}

impl std::fmt::Debug for Tray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tray").field("tooltip", &self.shown.2).finish_non_exhaustive()
    }
}

impl Tray {
    /// Registers the tray icon. `on_action` runs on the tray's D-Bus thread.
    ///
    /// # Errors
    /// No D-Bus session or no StatusNotifier host (e.g. GNOME without the
    /// AppIndicator extension).
    pub fn new(on_action: impl Fn(Action) + Send + 'static) -> Result<Self, String> {
        let tooltip = "MixerCraft — connecting…".to_owned();
        let sni = Sni {
            strips: Vec::new(),
            connected: false,
            tooltip: tooltip.clone(),
            icon: load_icon().into_iter().collect(),
            on_action: Box::new(on_action),
        };
        let handle = sni.spawn().map_err(|e| e.to_string())?;
        Ok(Self { handle, shown: (Vec::new(), false, tooltip) })
    }

    /// Reflects the current state; sends an update only when it changed.
    pub fn update(&mut self, strips: &[(String, bool)], connected: bool, tooltip: &str) {
        let state = (strips.to_vec(), connected, tooltip.to_owned());
        if state == self.shown {
            return;
        }
        self.shown = state.clone();
        let (strips, connected, tooltip) = state;
        self.handle.update(move |t| {
            t.strips = strips;
            t.connected = connected;
            t.tooltip = tooltip;
        });
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        // Removes the icon now instead of when the process exits.
        let _ = self.handle.shutdown();
    }
}
