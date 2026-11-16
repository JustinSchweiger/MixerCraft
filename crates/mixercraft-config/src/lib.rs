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
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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
            }],
            buses: vec![BusConfig {
                id: "A1".into(),
                name: "Main".into(),
                sink: Sink::DefaultOutput,
                channels: 2,
                gain_db: 0.0,
                mute: false,
            }],
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
        }
        for s in &self.strips {
            check_channels(s.channels, &s.name)?;
            check_gain(s.gain_db, &s.name)?;
            if let Some(r) = s.routes.iter().find(|r| !ids.contains(r.as_str())) {
                return invalid(format!("strip '{}' routes to unknown bus '{r}'", s.name));
            }
        }
        if let Some(m) = &e.master_bus
            && !ids.contains(m.as_str())
        {
            return invalid(format!("engine.master_bus '{m}' does not exist"));
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
        // Schema 1 is the first released schema. Each future schema change
        // adds one arm that upgrades a single step and recurses, e.g.
        //   1 => migrate(migrate_1_to_2(doc)?),
        _ => Ok(doc),
    }
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
