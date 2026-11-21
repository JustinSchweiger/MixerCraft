//! Versioned, migratable configuration for MixerCraft.
//!
//! The configuration is a TOML document with an explicit `schema_version`.
//! Loading goes through [`load_str`], which
//!
//! 1. rejects documents written by a *newer* schema (never silently drop
//!    settings a newer version understood),
//! 2. upgrades older documents step by step through [`migrate`],
//! 3. deserialises and [`Config::validate`]s the result.
//!
//! Saving is atomic (write to a temporary file, then rename) and keeps the
//! previous file as `*.bak`, so a crash or power loss can never leave a
//! half-written configuration behind.
//!
//! Devices are referenced by their backend-specific stable identifier *and*
//! their human-readable name, so a configuration can be re-bound when an
//! identifier changes (e.g. a USB device moved to another port).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The schema version written by this build.
pub const CURRENT_SCHEMA_VERSION: u32 = 5;

/// Gate thresholds at or below this mean "gate off" (mirrors the engine).
pub const GATE_OFF_DB: f32 = -80.0;

/// Maximum channels per strip or bus (mirrors the engine limit).
pub const MAX_CHANNELS: u8 = 8;

/// Maximum number of buses (mirrors the engine limit).
pub const MAX_BUSES: usize = 64;

/// Maximum number of strips with an attached source (mirrors the engine).
pub const MAX_STRIPS: usize = 64;

/// Errors raised while loading, validating or saving configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The document is not valid TOML or does not match the schema.
    #[error("invalid configuration: {0}")]
    Parse(String),
    /// `schema_version` is missing.
    #[error("configuration has no schema_version")]
    MissingVersion,
    /// The document was written by a newer MixerCraft.
    #[error("configuration schema {found} is newer than supported schema {supported}")]
    TooNew { found: u32, supported: u32 },
    /// The document parsed but is semantically invalid.
    #[error("invalid configuration: {0}")]
    Invalid(String),
    /// Filesystem error.
    #[error("configuration I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// How much latency the engine trades for robustness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LatencyProfile {
    /// Smallest buffers; may glitch on busy systems.
    LowLatency,
    /// Default: comfortably glitch-free on typical systems.
    #[default]
    Balanced,
    /// Large safety margins for heavily loaded or unusual systems.
    MaxStability,
}

impl LatencyProfile {
    /// Initial FIFO target per device stream, in milliseconds.
    #[must_use]
    pub fn fifo_target_ms(self) -> f32 {
        match self {
            Self::LowLatency => 6.0,
            Self::Balanced => 15.0,
            Self::MaxStability => 40.0,
        }
    }

    /// Requested device buffer size in frames at 48 kHz (`None` = device
    /// default).
    #[must_use]
    pub fn device_buffer_frames(self) -> Option<u32> {
        match self {
            Self::LowLatency => Some(128),
            Self::Balanced => Some(480),
            Self::MaxStability => None,
        }
    }
}

/// Reference to a device that survives reboots and re-plugging as well as
/// the backend allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DeviceRef {
    /// Backend-specific stable identifier (preferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Display name, used as a fallback when the identifier is not found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where a strip's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    /// Nothing; the strip is silent.
    #[default]
    None,
    /// The operating system's default capture device, following changes.
    DefaultInput,
    /// A specific capture device.
    Device(DeviceRef),
}

/// Where a bus's audio goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Sink {
    /// Nowhere; the bus is only metered.
    #[default]
    None,
    /// The operating system's default playback device, following changes.
    DefaultOutput,
    /// A specific playback device.
    Device(DeviceRef),
}

/// Engine-wide settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// Preferred internal sample rate. The master device's actual rate wins
    /// if it differs.
    pub sample_rate: u32,
    /// Maximum frames per processing cycle.
    pub max_block_frames: u32,
    /// Latency/robustness trade-off.
    pub latency_profile: LatencyProfile,
    /// Bus whose device clocks the engine (`None` = first bus with a sink).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_bus: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            max_block_frames: 1024,
            latency_profile: LatencyProfile::default(),
            master_bus: None,
        }
    }
}

/// An input strip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StripConfig {
    /// Display name.
    pub name: String,
    /// Audio source.
    #[serde(default)]
    pub source: Source,
    /// Channel count.
    #[serde(default = "two")]
    pub channels: u8,
    /// Fader level (dB).
    #[serde(default)]
    pub gain_db: f32,
    /// Mute.
    #[serde(default)]
    pub mute: bool,
    /// Solo.
    #[serde(default)]
    pub solo: bool,
    /// Ids of the buses this strip feeds.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Gate, EQ and compressor (schema 2).
    #[serde(default, skip_serializing_if = "FxConfig::is_off")]
    pub fx: FxConfig,
}

/// Per-strip processing settings (schema 2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FxConfig {
    /// Gate threshold (dBFS); at or below -80 the gate is off. Up to -10.
    pub gate_threshold_db: f32,
    /// Compressor amount, 0 (off) to 10.
    pub comp_amount: f32,
    /// Low shelf gain (dB, ±12).
    pub eq_low_db: f32,
    /// Mid peak gain (dB, ±12).
    pub eq_mid_db: f32,
    /// High shelf gain (dB, ±12).
    pub eq_high_db: f32,
    /// RNNoise speech noise suppression (schema 3).
    pub noise_suppression: bool,
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            gate_threshold_db: GATE_OFF_DB,
            comp_amount: 0.0,
            eq_low_db: 0.0,
            eq_mid_db: 0.0,
            eq_high_db: 0.0,
            noise_suppression: false,
        }
    }
}

impl FxConfig {
    /// Whether all processing is off (not written to the file then).
    #[must_use]
    pub fn is_off(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self, owner: &str) -> Result<(), ConfigError> {
        let ok = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        if ok(self.gate_threshold_db, -200.0, -10.0)
            && ok(self.comp_amount, 0.0, 10.0)
            && [self.eq_low_db, self.eq_mid_db, self.eq_high_db].iter().all(|g| ok(*g, -12.0, 12.0))
        {
            Ok(())
        } else {
            Err(ConfigError::Invalid(format!("'{owner}' has processing settings out of range")))
        }
    }
}

/// An output bus.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusConfig {
    /// Stable short identifier used for routing (e.g. `"A1"`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Audio destination.
    #[serde(default)]
    pub sink: Sink,
    /// Channel count.
    #[serde(default = "two")]
    pub channels: u8,
    /// Fader level (dB).
    #[serde(default)]
    pub gain_db: f32,
    /// Mute.
    #[serde(default)]
    pub mute: bool,
    /// Inaudible tone that keeps auto-standby speakers on (schema 3).
    #[serde(default, skip_serializing_if = "KeepAwakeConfig::is_default")]
    pub keep_awake: KeepAwakeConfig,
}

/// Keep-awake signal of a bus (schema 3; `mode` and `interval_s` since
/// schema 4): keeps speakers with auto-standby from switching off.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeepAwakeConfig {
    /// Whether keep-awake is used.
    pub enabled: bool,
    /// Pulse (default) or infrasound.
    pub mode: KeepAwakeMode,
    /// Signal level (dBFS peak), -70 to -20.
    pub level_db: f32,
    /// Infrasound frequency (Hz), 5 to 40.
    pub frequency_hz: f32,
    /// Seconds of silence between pulses, 30 to 900.
    pub interval_s: f32,
}

/// What a keep-awake bus plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeepAwakeMode {
    /// A short quiet noise burst after each interval of silence.
    #[default]
    Pulse,
    /// A continuous inaudible low sine after 10 s of silence.
    Infrasound,
}

impl Default for KeepAwakeConfig {
    fn default() -> Self {
        Self { enabled: false, mode: KeepAwakeMode::Pulse, level_db: -45.0, frequency_hz: 15.0, interval_s: 240.0 }
    }
}

impl KeepAwakeConfig {
    /// Whether these are the defaults (not written to the file then).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self, owner: &str) -> Result<(), ConfigError> {
        let ok = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
        if ok(self.level_db, -70.0, -20.0) && ok(self.frequency_hz, 5.0, 40.0) && ok(self.interval_s, 30.0, 900.0) {
            Ok(())
        } else {
            Err(ConfigError::Invalid(format!("'{owner}' has keep-awake settings out of range")))
        }
    }
}

fn two() -> u8 {
    2
}

/// The complete persisted configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version of this document.
    pub schema_version: u32,
    /// Engine settings.
    #[serde(default)]
    pub engine: EngineConfig,
    /// Input strips in display order.
    #[serde(default)]
    pub strips: Vec<StripConfig>,
    /// Output buses in display order.
    #[serde(default)]
    pub buses: Vec<BusConfig>,
    /// Global keyboard shortcuts (schema 5).
    #[serde(default, skip_serializing_if = "HotkeysConfig::is_default")]
    pub hotkeys: HotkeysConfig,
}

/// Global keyboard shortcuts and their confirmation tone (schema 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeysConfig {
    /// Id of the bus that plays a short confirmation tone when a hotkey
    /// fires (`None`: no tone). Should not be a bus other applications
    /// record from, or the tone goes into the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_bus: Option<String>,
    /// Tone level (dBFS peak), -60 to -6.
    pub feedback_level_db: f32,
    /// The shortcuts.
    pub bindings: Vec<HotkeyBinding>,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self { feedback_bus: None, feedback_level_db: -20.0, bindings: Vec::new() }
    }
}

impl HotkeysConfig {
    /// Whether these are the defaults (not written to the file then).
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The next free binding id.
    #[must_use]
    pub fn next_id(&self) -> u32 {
        self.bindings.iter().map(|b| b.id).max().unwrap_or(0) + 1
    }
}

/// One global keyboard shortcut.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HotkeyBinding {
    /// Stable id (unique, > 0).
    pub id: u32,
    /// Key combination, e.g. `"Ctrl+Alt+M"` (see `mixercraft_hotkey::Combo`).
    pub keys: String,
    /// Toggle on each press, or only while held.
    #[serde(default)]
    pub mode: HotkeyMode,
    /// What the shortcut does.
    pub action: HotkeyAction,
}

/// How a shortcut acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HotkeyMode {
    /// Each press switches (mute on, mute off).
    #[default]
    Toggle,
    /// Switches while the key is held and back on release: push-to-talk on
    /// a muted strip, push-to-mute on an open one. Gain steps ignore it.
    Hold,
}

/// What a shortcut does. Strips are addressed by position (0 = first), buses
/// by id; removing a strip or bus updates or removes the bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum HotkeyAction {
    /// Mute or unmute an input strip.
    StripMute { strip: u16 },
    /// Solo or unsolo an input strip.
    StripSolo { strip: u16 },
    /// Move a strip fader by `step_db` (±0.5 to ±24).
    StripGain { strip: u16, step_db: f32 },
    /// Mute or unmute an output bus.
    BusMute { bus: String },
    /// Move a bus fader by `step_db`.
    BusGain { bus: String, step_db: f32 },
}

impl HotkeyAction {
    /// Adjusts for removed strip `i`: later positions move down by one.
    /// Returns `true` if the action referred to that strip (it is then
    /// meaningless and should be dropped).
    pub fn on_strip_removed(&mut self, i: u16) -> bool {
        match self {
            Self::StripMute { strip } | Self::StripSolo { strip } | Self::StripGain { strip, .. } => {
                if *strip == i {
                    return true;
                }
                if *strip > i {
                    *strip -= 1;
                }
                false
            }
            Self::BusMute { .. } | Self::BusGain { .. } => false,
        }
    }

    /// The bus id this action refers to, if any.
    #[must_use]
    pub fn bus(&self) -> Option<&str> {
        match self {
            Self::BusMute { bus } | Self::BusGain { bus, .. } => Some(bus),
            _ => None,
        }
    }
}

impl Default for Config {
    /// Default input on a strip (metered, not routed) and bus `A1` on the
    /// default output. Routing the input is left to the user: routing a
    /// microphone to speakers by default would cause feedback.
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            engine: EngineConfig::default(),
            strips: vec![StripConfig {
                name: "Input".into(),
                source: Source::DefaultInput,
                channels: 2,
                gain_db: 0.0,
                mute: false,
                solo: false,
                routes: Vec::new(),
                fx: FxConfig::default(),
            }],
            buses: vec![BusConfig {
                id: "A1".into(),
                name: "Main".into(),
                sink: Sink::DefaultOutput,
                channels: 2,
                gain_db: 0.0,
                mute: false,
                keep_awake: KeepAwakeConfig::default(),
            }],
            hotkeys: HotkeysConfig::default(),
        }
    }
}

impl Config {
    /// Index of the bus with `id`.
    #[must_use]
    pub fn bus_index(&self, id: &str) -> Option<usize> {
        self.buses.iter().position(|b| b.id == id)
    }

    /// The bus that clocks the engine: the configured master bus, or the
    /// first bus with a sink.
    #[must_use]
    pub fn master_bus_index(&self) -> Option<usize> {
        match &self.engine.master_bus {
            Some(id) => self.bus_index(id),
            None => self.buses.iter().position(|b| b.sink != Sink::None),
        }
    }

    /// Checks semantic constraints the type system cannot express.
    ///
    /// # Errors
    /// [`ConfigError::Invalid`] describing the first problem found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        let e = &self.engine;
        if !(8_000..=384_000).contains(&e.sample_rate) {
            return invalid(format!("engine.sample_rate {} out of range", e.sample_rate));
        }
        if !(16..=8192).contains(&e.max_block_frames) {
            return invalid(format!("engine.max_block_frames {} out of range", e.max_block_frames));
        }
        if self.buses.len() > MAX_BUSES {
            return invalid(format!("at most {MAX_BUSES} buses are supported"));
        }
        if self.strips.len() > MAX_STRIPS {
            return invalid(format!("at most {MAX_STRIPS} strips are supported"));
        }
        let mut ids = HashSet::new();
        for b in &self.buses {
            if b.id.is_empty() || !ids.insert(b.id.as_str()) {
                return invalid(format!("bus id '{}' is empty or duplicated", b.id));
            }
            check_channels(b.channels, &b.name)?;
            check_gain(b.gain_db, &b.name)?;
            b.keep_awake.validate(&b.name)?;
        }
        for s in &self.strips {
            check_channels(s.channels, &s.name)?;
            check_gain(s.gain_db, &s.name)?;
            s.fx.validate(&s.name)?;
            if let Some(r) = s.routes.iter().find(|r| !ids.contains(r.as_str())) {
                return invalid(format!("strip '{}' routes to unknown bus '{r}'", s.name));
            }
        }
        if let Some(m) = &e.master_bus
            && !ids.contains(m.as_str())
        {
            return invalid(format!("engine.master_bus '{m}' does not exist"));
        }
        self.validate_hotkeys(&ids)
    }

    fn validate_hotkeys(&self, bus_ids: &HashSet<&str>) -> Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        let h = &self.hotkeys;
        if let Some(b) = &h.feedback_bus
            && !bus_ids.contains(b.as_str())
        {
            return invalid(format!("hotkeys.feedback_bus '{b}' does not exist"));
        }
        if !(h.feedback_level_db.is_finite() && (-60.0..=-6.0).contains(&h.feedback_level_db)) {
            return invalid(format!("hotkeys.feedback_level_db {} out of range (-60 to -6)", h.feedback_level_db));
        }
        let mut seen_ids = HashSet::new();
        let mut seen_keys = HashSet::new();
        for b in &h.bindings {
            if b.id == 0 || !seen_ids.insert(b.id) {
                return invalid(format!("hotkey id {} is zero or duplicated", b.id));
            }
            let combo: mixercraft_hotkey::Combo =
                b.keys.parse().map_err(|e| ConfigError::Invalid(format!("hotkey {}: {e}", b.id)))?;
            combo.check_bindable().map_err(|e| ConfigError::Invalid(format!("hotkey {}: {e}", b.id)))?;
            if !seen_keys.insert(combo) {
                return invalid(format!("hotkey {combo} is bound twice"));
            }
            let step_ok = |s: f32| s.is_finite() && (0.5..=24.0).contains(&s.abs());
            let strip_ok = |s: u16| usize::from(s) < self.strips.len();
            let ok = match &b.action {
                HotkeyAction::StripMute { strip } | HotkeyAction::StripSolo { strip } => strip_ok(*strip),
                HotkeyAction::StripGain { strip, step_db } => strip_ok(*strip) && step_ok(*step_db),
                HotkeyAction::BusMute { bus } => bus_ids.contains(bus.as_str()),
                HotkeyAction::BusGain { bus, step_db } => bus_ids.contains(bus.as_str()) && step_ok(*step_db),
            };
            if !ok {
                return invalid(format!("hotkey {combo} refers to a missing strip or bus or has an invalid step"));
            }
        }
        Ok(())
    }

    /// Serialises to TOML.
    ///
    /// # Errors
    /// Only if serialisation fails, which indicates a bug.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))
    }
}

fn check_channels(c: u8, owner: &str) -> Result<(), ConfigError> {
    if (1..=MAX_CHANNELS).contains(&c) {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!("'{owner}' has {c} channels (1..={MAX_CHANNELS})")))
    }
}

fn check_gain(db: f32, owner: &str) -> Result<(), ConfigError> {
    if db.is_finite() && (-200.0..=24.0).contains(&db) {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!("'{owner}' has invalid gain {db} dB")))
    }
}

/// Upgrades a raw document from its schema version to
/// [`CURRENT_SCHEMA_VERSION`], one version at a time.
///
/// Each future schema change adds one `n => migrate_n_to_n_plus_1` arm, so
/// every historical document has a tested upgrade path.
///
/// # Errors
/// If the version is missing, newer than supported, or a step fails.
pub fn migrate(doc: toml::Table) -> Result<toml::Table, ConfigError> {
    let version = doc
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or(ConfigError::MissingVersion)?;
    match version {
        0 => Err(ConfigError::MissingVersion),
        v if v > CURRENT_SCHEMA_VERSION => Err(ConfigError::TooNew { found: v, supported: CURRENT_SCHEMA_VERSION }),
        // Each schema change adds one arm that upgrades a single step and
        // recurses, so every historical document has a tested path.
        1 => migrate(migrate_1_to_2(doc)),
        2 => migrate(migrate_2_to_3(doc)),
        3 => migrate(migrate_3_to_4(doc)),
        4 => migrate(migrate_4_to_5(doc)),
        _ => Ok(doc),
    }
}

/// Schema 2 added optional per-strip `fx` tables; their absence means "off",
/// so the upgrade only bumps the version.
fn migrate_1_to_2(mut doc: toml::Table) -> toml::Table {
    doc.insert("schema_version".into(), toml::Value::Integer(2));
    doc
}

/// Schema 3 added `fx.noise_suppression` and per-bus `keep_awake` tables;
/// both default to off, so the upgrade only bumps the version. (Older
/// engines reject the new fields, hence the version bump.)
fn migrate_2_to_3(mut doc: toml::Table) -> toml::Table {
    doc.insert("schema_version".into(), toml::Value::Integer(3));
    doc
}

/// Schema 4 added keep-awake `mode` (pulse or infrasound) and `interval_s`.
/// Existing keep-awake tables were infrasound, so they keep that mode; new
/// ones default to pulses.
fn migrate_3_to_4(mut doc: toml::Table) -> toml::Table {
    if let Some(toml::Value::Array(buses)) = doc.get_mut("buses") {
        for bus in buses {
            if let Some(toml::Value::Table(ka)) = bus.get_mut("keep_awake") {
                ka.entry("mode").or_insert_with(|| toml::Value::String("infrasound".into()));
            }
        }
    }
    doc.insert("schema_version".into(), toml::Value::Integer(4));
    doc
}

/// Schema 5 added the optional `[hotkeys]` table; its absence means "no
/// shortcuts", so the upgrade only bumps the version. (Older engines reject
/// the table, hence the bump.)
fn migrate_4_to_5(mut doc: toml::Table) -> toml::Table {
    doc.insert("schema_version".into(), toml::Value::Integer(5));
    doc
}

/// Parses, migrates and validates a configuration document.
///
/// # Errors
/// See [`ConfigError`].
pub fn load_str(text: &str) -> Result<Config, ConfigError> {
    let doc: toml::Table = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let doc = migrate(doc)?;
    let config: Config =
        toml::Value::Table(doc).try_into().map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))?;
    config.validate()?;
    Ok(config)
}

/// Loads the configuration at `path`.
///
/// Returns `Ok(None)` if the file does not exist.
///
/// # Errors
/// I/O errors other than "not found", and any [`load_str`] error.
pub fn load_file(path: &Path) -> Result<Option<Config>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => load_str(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Io { path: path.to_owned(), source }),
    }
}

/// Validates and atomically writes `config` to `path`, keeping the previous
/// file as `<path>.bak`.
///
/// # Errors
/// Validation or I/O failures. On failure the existing file is untouched.
pub fn save_file(path: &Path, config: &Config) -> Result<(), ConfigError> {
    config.validate()?;
    let text = config.to_toml()?;
    let io = |source| ConfigError::Io { path: path.to_owned(), source };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io)?;
    }
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(io)?;
        f.write_all(text.as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    if path.exists() {
        fs::copy(path, path.with_extension("toml.bak")).map_err(io)?;
    }
    fs::rename(&tmp, path).map_err(io)?;
    Ok(())
}

/// The per-user configuration file location.
#[must_use]
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "MixerCraft").map(|d| d.config_dir().join("config.toml"))
}

/// The per-user directory for engine log files.
#[must_use]
pub fn default_log_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "MixerCraft").map(|d| d.data_local_dir().join("logs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
schema_version = 1

[engine]
sample_rate = 48000
latency_profile = "max-stability"
master_bus = "A1"

[[strips]]
name = "Microphone"
source = { kind = "device", id = "wasapi:{0.0.1.00000000}.{abc}", name = "USB Mic" }
channels = 1
gain_db = -3.5
routes = ["A1", "B1"]

[[strips]]
name = "System"
routes = ["A1"]

[[buses]]
id = "A1"
name = "Speakers"
sink = { kind = "default-output" }

[[buses]]
id = "B1"
name = "Stream"
"#;

    #[test]
    fn parses_example_document() {
        let c = load_str(EXAMPLE).unwrap();
        assert_eq!(c.engine.latency_profile, LatencyProfile::MaxStability);
        assert_eq!(c.engine.max_block_frames, 1024, "defaults fill missing fields");
        assert_eq!(c.strips[0].channels, 1);
        assert_eq!(
            c.strips[0].source,
            Source::Device(DeviceRef {
                id: Some("wasapi:{0.0.1.00000000}.{abc}".into()),
                name: Some("USB Mic".into())
            })
        );
        assert_eq!(c.strips[1].source, Source::None);
        assert_eq!(c.buses[1].sink, Sink::None);
        assert_eq!(c.master_bus_index(), Some(0));
    }

    #[test]
    fn roundtrip_is_lossless() {
        let c = load_str(EXAMPLE).unwrap();
        let again = load_str(&c.to_toml().unwrap()).unwrap();
        assert_eq!(c, again);
        let d = Config::default();
        assert_eq!(load_str(&d.to_toml().unwrap()).unwrap(), d);
    }

    #[test]
    fn schema_1_documents_migrate_to_the_current_schema() {
        assert!(EXAMPLE.contains("schema_version = 1"));
        let c = load_str(EXAMPLE).unwrap();
        assert_eq!(c.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(c.strips.iter().all(|s| s.fx.is_off()));
        assert!(c.buses.iter().all(|b| b.keep_awake.is_default()));
        let saved = c.to_toml().unwrap();
        assert!(saved.contains(&format!("schema_version = {CURRENT_SCHEMA_VERSION}")));
        assert!(!saved.contains("[strips.fx]"), "default processing is not written");
        assert!(!saved.contains("keep_awake"), "default keep-awake is not written");
    }

    #[test]
    fn schema_2_documents_migrate_to_schema_3() {
        let v2 = EXAMPLE.replace("schema_version = 1", "schema_version = 2").replace(
            r#"routes = ["A1"]"#,
            "routes = [\"A1\"]

[strips.fx]
comp_amount = 3.0",
        );
        let c = load_str(&v2).unwrap();
        assert_eq!(c.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(c.strips[1].fx.comp_amount, 3.0, "fx of the second strip kept");
        assert!(!c.strips[1].fx.noise_suppression, "new settings default to off");
    }

    #[test]
    fn noise_suppression_and_keep_awake_roundtrip_and_validation() {
        let mut c = load_str(EXAMPLE).unwrap();
        c.strips[0].fx.noise_suppression = true;
        c.buses[0].keep_awake =
            KeepAwakeConfig { enabled: true, level_db: -40.0, interval_s: 120.0, ..Default::default() };
        let text = c.to_toml().unwrap();
        assert!(text.contains("noise_suppression = true"));
        assert!(text.contains("[buses.keep_awake]"));
        assert_eq!(load_str(&text).unwrap(), c);
        let bad = text.replace("level_db = -40.0", "level_db = -5.0");
        assert!(matches!(load_str(&bad), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn schema_3_keep_awake_stays_infrasound_and_new_ones_pulse() {
        let v3 = EXAMPLE.replace("schema_version = 1", "schema_version = 3").replace(
            r#"name = "Stream""#,
            "name = \"Stream\"\n\n[buses.keep_awake]\nenabled = true\nlevel_db = -45.0\nfrequency_hz = 15.0",
        );
        assert!(v3.contains("[buses.keep_awake]"), "test document has a keep-awake table");
        let c = load_str(&v3).unwrap();
        assert_eq!(c.schema_version, CURRENT_SCHEMA_VERSION);
        let ka = c.buses.iter().find(|b| b.keep_awake.enabled).unwrap().keep_awake;
        assert_eq!(ka.mode, KeepAwakeMode::Infrasound, "existing tone keeps its behaviour");
        assert_eq!(ka.interval_s, 240.0);
        assert_eq!(KeepAwakeConfig::default().mode, KeepAwakeMode::Pulse);
        let text = c.to_toml().unwrap();
        assert!(text.contains("mode = \"infrasound\""));
        assert_eq!(load_str(&text).unwrap(), c);
    }

    #[test]
    fn schema_4_documents_migrate_to_schema_5_without_hotkeys() {
        let v4 = EXAMPLE.replace("schema_version = 1", "schema_version = 4");
        let c = load_str(&v4).unwrap();
        assert_eq!(c.schema_version, 5);
        assert!(c.hotkeys.is_default());
        assert!(!c.to_toml().unwrap().contains("hotkeys"), "no shortcuts: nothing written");
    }

    fn with_hotkeys() -> Config {
        let mut c = load_str(EXAMPLE).unwrap();
        c.hotkeys = HotkeysConfig {
            feedback_bus: Some("A1".into()),
            feedback_level_db: -24.0,
            bindings: vec![
                HotkeyBinding {
                    id: 1,
                    keys: "Ctrl+Alt+M".into(),
                    mode: HotkeyMode::Toggle,
                    action: HotkeyAction::StripMute { strip: 0 },
                },
                HotkeyBinding {
                    id: 4,
                    keys: "F13".into(),
                    mode: HotkeyMode::Hold,
                    action: HotkeyAction::StripMute { strip: 1 },
                },
                HotkeyBinding {
                    id: 2,
                    keys: "Ctrl+Shift+Down".into(),
                    mode: HotkeyMode::Toggle,
                    action: HotkeyAction::BusGain { bus: "B1".into(), step_db: -3.0 },
                },
            ],
        };
        c
    }

    #[test]
    fn hotkeys_roundtrip_and_validation() {
        let c = with_hotkeys();
        let text = c.to_toml().unwrap();
        assert!(text.contains("[[hotkeys.bindings]]"), "{text}");
        assert!(text.contains("feedback_bus = \"A1\""));
        assert_eq!(load_str(&text).unwrap(), c);
        assert_eq!(c.hotkeys.next_id(), 5);

        let bad = |f: &dyn Fn(&mut Config)| {
            let mut c = with_hotkeys();
            f(&mut c);
            matches!(c.validate(), Err(ConfigError::Invalid(_)))
        };
        assert!(!bad(&|_| {}));
        assert!(bad(&|c| c.hotkeys.bindings[1].keys = "M".into()), "typing key without modifier");
        assert!(bad(&|c| c.hotkeys.bindings[1].keys = "Ctrl+Hyper".into()), "unknown key");
        assert!(bad(&|c| c.hotkeys.bindings[1].keys = "alt+ctrl+m".into()), "same combo twice");
        assert!(bad(&|c| c.hotkeys.bindings[1].id = 1), "duplicate id");
        assert!(bad(&|c| c.hotkeys.bindings[0].action = HotkeyAction::StripSolo { strip: 9 }), "missing strip");
        assert!(bad(&|c| c.hotkeys.bindings[2].action = HotkeyAction::BusMute { bus: "Z".into() }), "missing bus");
        assert!(bad(&|c| c.hotkeys.bindings[2].action = HotkeyAction::BusGain { bus: "A1".into(), step_db: 0.0 }));
        assert!(bad(&|c| c.hotkeys.feedback_bus = Some("Z".into())));
        assert!(bad(&|c| c.hotkeys.feedback_level_db = 0.0));
    }

    #[test]
    fn hotkey_strip_positions_follow_removals() {
        let mut a = HotkeyAction::StripGain { strip: 3, step_db: 1.0 };
        assert!(!a.on_strip_removed(1));
        assert_eq!(a, HotkeyAction::StripGain { strip: 2, step_db: 1.0 });
        assert!(!a.on_strip_removed(5));
        assert!(a.on_strip_removed(2), "its own strip went away");
        assert!(!HotkeyAction::BusMute { bus: "A1".into() }.on_strip_removed(0));
    }

    #[test]
    fn strip_fx_roundtrip_and_validation() {
        let mut c = load_str(EXAMPLE).unwrap();
        c.strips[0].fx = FxConfig {
            gate_threshold_db: -45.0,
            comp_amount: 4.5,
            eq_low_db: 2.0,
            eq_mid_db: -1.5,
            eq_high_db: 3.0,
            noise_suppression: false,
        };
        let text = c.to_toml().unwrap();
        assert!(text.contains("[strips.fx]"));
        assert_eq!(load_str(&text).unwrap(), c);
        let bad = text.replace("comp_amount = 4.5", "comp_amount = 40.0");
        assert!(matches!(load_str(&bad), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn rejects_newer_and_unversioned_documents() {
        assert!(matches!(load_str("schema_version = 99"), Err(ConfigError::TooNew { found: 99, .. })));
        assert!(matches!(load_str("[engine]\nsample_rate = 48000"), Err(ConfigError::MissingVersion)));
        assert!(matches!(load_str("schema_version = 0"), Err(ConfigError::MissingVersion)));
    }

    #[test]
    fn rejects_semantic_errors() {
        let bad_route = EXAMPLE.replace(r#"routes = ["A1"]"#, r#"routes = ["Z9"]"#);
        assert!(matches!(load_str(&bad_route), Err(ConfigError::Invalid(m)) if m.contains("Z9")));
        let dup = EXAMPLE.replace(r#"id = "B1""#, r#"id = "A1""#);
        assert!(matches!(load_str(&dup), Err(ConfigError::Invalid(_))));
        let chans = EXAMPLE.replace("channels = 1", "channels = 9");
        assert!(matches!(load_str(&chans), Err(ConfigError::Invalid(_))));
        let unknown = EXAMPLE.replace("[engine]", "[engine]\nturbo = true");
        assert!(matches!(load_str(&unknown), Err(ConfigError::Parse(_))), "unknown fields are errors");
    }

    #[test]
    fn atomic_save_keeps_backup() {
        let dir = std::env::temp_dir().join(format!("mixercraft-config-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = fs::remove_dir_all(&dir);
        let mut c = Config::default();
        save_file(&path, &c).unwrap();
        c.strips[0].gain_db = -10.0;
        save_file(&path, &c).unwrap();
        assert_eq!(load_file(&path).unwrap().unwrap().strips[0].gain_db, -10.0);
        let backup = load_file(&path.with_extension("toml.bak")).unwrap().unwrap();
        assert_eq!(backup.strips[0].gain_db, 0.0);
        assert!(load_file(&dir.join("missing.toml")).unwrap().is_none());
        let mut invalid = c.clone();
        invalid.strips[0].routes = vec!["nope".into()];
        assert!(save_file(&path, &invalid).is_err());
        assert_eq!(load_file(&path).unwrap().unwrap(), c, "failed save leaves file intact");
        fs::remove_dir_all(&dir).unwrap();
    }
}
