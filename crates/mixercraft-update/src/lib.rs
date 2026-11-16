//! Update checks for MixerCraft (ADR 0006).
//!
//! A release is described by a small JSON **feed manifest** at a
//! configurable URL (see [`Manifest`]). The app fetches it on start and once a
//! day (if the user did not turn checks off), compares the version with its
//! own, and — only after the user clicks — downloads the installer for this
//! platform, checks its size and SHA-256 against the manifest and hands it to
//! the platform installer.
//!
//! Supported feed and download URLs: `https://…`, `file://…` (a local or
//! network folder) and `http://` on the loopback interface only (tests and a
//! local release preview). Relative asset URLs are resolved against the
//! manifest URL, so one manifest works on GitHub Releases
//! (`…/releases/latest/download/latest.json`), a static host or a folder.
//!
//! What the checksum does and does not prove is recorded in ADR 0006: it
//! detects corrupt and truncated downloads; authenticity rests on HTTPS and
//! the release host until manifests are signed.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Manifest format understood by this build.
pub const MANIFEST_SCHEMA: u32 = 1;
/// Automatic checks run at most this often.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Feed compiled into release builds (`MIXERCRAFT_UPDATE_FEED` at build
/// time); [`Settings::feed_url`] overrides it.
pub const BUILT_IN_FEED: Option<&str> = option_env!("MIXERCRAFT_UPDATE_FEED");
/// Marker file the Windows installer puts next to the executables. Only an
/// installed copy replaces itself; a development build just reports updates.
pub const MSI_MARKER: &str = "installed-by-msi.txt";

const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_ASSET_BYTES: u64 = 1024 * 1024 * 1024;

/// Update failures, worded for the About page.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// No feed URL is configured for this build.
    #[error("update checks are not configured for this build")]
    NoFeed,
    /// The URL scheme is not allowed.
    #[error("refusing to use {0}: only https, file and local http addresses are allowed")]
    InsecureUrl(String),
    /// Network failure or an HTTP error status.
    #[error("cannot reach the update server: {0}")]
    Http(String),
    /// The feed does not exist (yet): HTTP 404 or a missing file, e.g.
    /// before the first release is published.
    #[error("no release has been published yet")]
    NoRelease,
    /// Reading or writing a file failed.
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    /// The manifest is malformed.
    #[error("the release feed is not valid: {0}")]
    Manifest(String),
    /// The manifest uses a newer format; this version cannot read it.
    #[error("the release feed needs a newer MixerCraft (feed format {0}); please update manually")]
    UnsupportedSchema(u32),
    /// The download is larger than announced.
    #[error("the download is larger than announced ({0} bytes)")]
    TooLarge(u64),
    /// Size or SHA-256 mismatch.
    #[error("the download is damaged or was altered ({0}); it was deleted")]
    Verification(String),
    /// No installer for this platform in the release.
    #[error("this release has no installer for this platform")]
    NoAsset,
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> UpdateError {
    let context = context.into();
    move |source| UpdateError::Io { context, source }
}

/// The release feed (`latest.json`).
///
/// ```json
/// {
///   "schema": 1,
///   "version": "0.2.0",
///   "published": "2026-11-20",
///   "notes": "Installer and in-app updates.",
///   "notes_url": "https://example.org/releases/v0.2.0",
///   "assets": [
///     { "target": "windows-x86_64-msi", "url": "MixerCraft-0.2.0-x64.msi",
///       "sha256": "…64 hex digits…", "size": 12345678 }
///   ]
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version ([`MANIFEST_SCHEMA`]).
    pub schema: u32,
    /// Semantic version of the release.
    pub version: String,
    /// Release date (ISO 8601), informational.
    #[serde(default)]
    pub published: String,
    /// Short release notes (plain text).
    #[serde(default)]
    pub notes: String,
    /// Web page with the full release notes.
    #[serde(default)]
    pub notes_url: String,
    /// Installers, one per platform target.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// One downloadable installer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// Platform and package kind, e.g. `windows-x86_64-msi` ([`current_target`]).
    pub target: String,
    /// Absolute URL, or relative to the manifest URL.
    pub url: String,
    /// SHA-256 of the file, lowercase or uppercase hex.
    pub sha256: String,
    /// File size in bytes.
    pub size: u64,
}

/// The installer target of this build, if it can update itself.
#[must_use]
pub fn current_target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) { Some("windows-x86_64-msi") } else { None }
}

/// A newer release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its version.
    pub version: Version,
    /// Short notes.
    pub notes: String,
    /// Full notes page (may be empty).
    pub notes_url: String,
    /// Installer for this platform with an absolute URL, if there is one.
    pub asset: Option<Asset>,
}

/// Result of a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The running version is the newest (or newer, e.g. a development build).
    UpToDate {
        /// Version in the feed.
        latest: Version,
    },
    /// A newer version exists.
    Available(Release),
}

/// Compares a parsed manifest with the running version.
///
/// # Errors
/// Malformed versions or checksums, or a manifest format this build does not
/// know.
pub fn evaluate(
    manifest: &Manifest,
    feed_url: &str,
    current: &str,
    target: Option<&str>,
) -> Result<Check, UpdateError> {
    if manifest.schema > MANIFEST_SCHEMA {
        return Err(UpdateError::UnsupportedSchema(manifest.schema));
    }
    let latest = Version::parse(manifest.version.trim().trim_start_matches('v'))
        .map_err(|e| UpdateError::Manifest(format!("version {:?}: {e}", manifest.version)))?;
    let current =
        Version::parse(current).map_err(|e| UpdateError::Manifest(format!("own version {current:?}: {e}")))?;
    if latest <= current {
        return Ok(Check::UpToDate { latest });
    }
    let asset = match target.and_then(|t| manifest.assets.iter().find(|a| a.target == t)) {
        Some(a) => {
            if !is_sha256_hex(&a.sha256) {
                return Err(UpdateError::Manifest(format!("sha256 of {} is not 64 hex digits", a.target)));
            }
            if a.size == 0 || a.size > MAX_ASSET_BYTES {
                return Err(UpdateError::Manifest(format!("size of {} is {}", a.target, a.size)));
            }
            Some(Asset { url: resolve_url(feed_url, &a.url), sha256: a.sha256.to_ascii_lowercase(), ..a.clone() })
        }
        None => None,
    };
    Ok(Check::Available(Release {
        version: latest,
        notes: manifest.notes.clone(),
        notes_url: manifest.notes_url.clone(),
        asset,
    }))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Resolves `url` against the manifest URL `base` (absolute URLs are kept).
#[must_use]
pub fn resolve_url(base: &str, url: &str) -> String {
    if url.contains("://") {
        return url.to_owned();
    }
    let dir = base.rfind('/').map_or(base, |i| &base[..=i]);
    format!("{dir}{}", url.trim_start_matches("./"))
}

/// Fetches and evaluates the feed at `feed_url`.
///
/// # Errors
/// Network, format or version errors.
pub fn check(feed_url: &str, current: &str) -> Result<Check, UpdateError> {
    let mut body = Vec::new();
    let (mut reader, _) = open(feed_url, Duration::from_secs(30)).map_err(|e| match e {
        UpdateError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => UpdateError::NoRelease,
        e => e,
    })?;
    reader
        .by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|e| UpdateError::Http(e.to_string()))?;
    if body.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(UpdateError::Manifest("larger than 256 KiB".into()));
    }
    let manifest: Manifest = serde_json::from_slice(&body).map_err(|e| UpdateError::Manifest(e.to_string()))?;
    evaluate(&manifest, feed_url, current, current_target())
}

/// File name a download of `asset` is stored under (never taken from the URL).
#[must_use]
pub fn download_file_name(version: &Version, asset: &Asset) -> String {
    let safe: String =
        asset.target.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let ext = if safe.ends_with("-msi") { "msi" } else { "bin" };
    format!("MixerCraft-{version}-{safe}.{ext}")
}

/// Downloads `asset` into `dir` (other files there are removed: it holds
/// only the latest download) and verifies size and SHA-256. A file that is
/// already there and verifies is reused. `progress(done, total)` is called
/// while downloading.
///
/// # Errors
/// Network and file errors; a size or checksum mismatch deletes the file.
pub fn download(
    version: &Version,
    asset: &Asset,
    dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, UpdateError> {
    std::fs::create_dir_all(dir).map_err(io_err(format!("cannot create {}", dir.display())))?;
    let path = dir.join(download_file_name(version, asset));
    if verify_file(&path, &asset.sha256, asset.size).is_ok() {
        progress(asset.size, asset.size);
        return Ok(path);
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.path().is_file() {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let part = path.with_extension("part");
    let result = fetch_to(asset, &part, progress);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, &path).map_err(io_err(format!("cannot rename {}", part.display())))?;
    Ok(path)
}

fn fetch_to(asset: &Asset, part: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<(), UpdateError> {
    let (reader, announced) = open(&asset.url, Duration::from_secs(30 * 60))?;
    if announced.is_some_and(|n| n > asset.size) {
        return Err(UpdateError::TooLarge(announced.unwrap_or_default()));
    }
    let mut file = File::create(part).map_err(io_err(format!("cannot write {}", part.display())))?;
    let mut hasher = Sha256::new();
    let mut limited = reader.take(asset.size + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        let n = limited.read(&mut buf).map_err(|e| UpdateError::Http(e.to_string()))?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done > asset.size {
            return Err(UpdateError::TooLarge(done));
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(io_err(format!("cannot write {}", part.display())))?;
        progress(done, asset.size);
    }
    file.sync_all().map_err(io_err(format!("cannot write {}", part.display())))?;
    if done != asset.size {
        return Err(UpdateError::Verification(format!("{done} of {} bytes", asset.size)));
    }
    let got = hex(&hasher.finalize());
    if !got.eq_ignore_ascii_case(&asset.sha256) {
        return Err(UpdateError::Verification(format!("SHA-256 {got}, expected {}", asset.sha256)));
    }
    Ok(())
}

/// Checks size and SHA-256 of a file on disk.
///
/// # Errors
/// Missing file or mismatch.
pub fn verify_file(path: &Path, sha256: &str, size: u64) -> Result<(), UpdateError> {
    let len = std::fs::metadata(path).map_err(io_err(format!("cannot read {}", path.display())))?.len();
    if len != size {
        return Err(UpdateError::Verification(format!("{len} of {size} bytes")));
    }
    let got = sha256_file(path)?;
    if got.eq_ignore_ascii_case(sha256) {
        Ok(())
    } else {
        Err(UpdateError::Verification(format!("SHA-256 {got}, expected {sha256}")))
    }
}

/// SHA-256 of a file as lowercase hex.
///
/// # Errors
/// If the file cannot be read.
pub fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(io_err(format!("cannot read {}", path.display())))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(io_err(format!("cannot read {}", path.display())))?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Opens `url` for reading; also returns the announced length, if any.
fn open(url: &str, body_timeout: Duration) -> Result<(Box<dyn Read + Send>, Option<u64>), UpdateError> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("file://") {
        let path = file_url_path(&url["file://".len()..]);
        let file = File::open(&path).map_err(io_err(format!("cannot read {}", path.display())))?;
        let len = file.metadata().ok().map(|m| m.len());
        return Ok((Box::new(file), len));
    }
    let https = lower.starts_with("https://");
    if !https && !(lower.starts_with("http://") && is_loopback(&lower["http://".len()..])) {
        return Err(UpdateError::InsecureUrl(url.to_owned()));
    }
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder()
        .provider(TlsProvider::Rustls)
        .root_certs(RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        // A redirect must not downgrade an https download.
        .https_only(https)
        .user_agent(concat!("MixerCraft-updater/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(body_timeout))
        .build()
        .into();
    let resp = agent.get(url).call().map_err(|e| match e {
        ureq::Error::StatusCode(404) => {
            UpdateError::Io { context: url.to_owned(), source: io::Error::from(io::ErrorKind::NotFound) }
        }
        e => UpdateError::Http(e.to_string()),
    })?;
    let len = resp.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
    Ok((Box::new(resp.into_body().into_reader()), len))
}

fn is_loopback(after_scheme: &str) -> bool {
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// `file://` URL path → file system path (`file:///C:/x%20y/a.json`).
fn file_url_path(rest: &str) -> PathBuf {
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    // Windows drive paths come as `/C:/...`.
    let rest =
        if rest.len() > 2 && rest.as_bytes()[0] == b'/' && rest.as_bytes()[2] == b':' { &rest[1..] } else { rest };
    PathBuf::from(percent_decode(rest))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// User settings of the updater (`updates.toml` in the configuration folder,
/// next to `config.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Check on start and once a day.
    pub check_automatically: bool,
    /// Feed URL; empty means the one built into this release
    /// ([`BUILT_IN_FEED`]).
    pub feed_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self { check_automatically: true, feed_url: String::new() }
    }
}

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "MixerCraft")
}

impl Settings {
    /// Location of the settings file.
    #[must_use]
    pub fn path() -> Option<PathBuf> {
        project_dirs().map(|d| d.config_dir().join("updates.toml"))
    }

    /// Loads the settings; a missing or unreadable file gives the defaults.
    #[must_use]
    pub fn load() -> Self {
        Self::path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    /// Loads from `path`; missing or invalid files give the defaults.
    #[must_use]
    pub fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
    }

    /// Saves the settings.
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn save(&self) -> Result<(), UpdateError> {
        let path = Self::path().ok_or_else(|| UpdateError::Io {
            context: "no configuration folder".into(),
            source: io::Error::from(io::ErrorKind::NotFound),
        })?;
        self.save_to(&path)
    }

    /// Saves to `path` (atomically: temporary file, then rename).
    ///
    /// # Errors
    /// If the file cannot be written.
    pub fn save_to(&self, path: &Path) -> Result<(), UpdateError> {
        let text = toml::to_string(self).map_err(|e| UpdateError::Io {
            context: "cannot encode update settings".into(),
            source: io::Error::other(e),
        })?;
        write_atomic(path, &text)
    }

    /// The feed to use: the configured one, else the built-in one.
    #[must_use]
    pub fn feed(&self) -> Option<String> {
        let own = self.feed_url.trim();
        if own.is_empty() { BUILT_IN_FEED.filter(|f| !f.is_empty()).map(str::to_owned) } else { Some(own.to_owned()) }
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), UpdateError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io_err(format!("cannot create {}", dir.display())))?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text).map_err(io_err(format!("cannot write {}", tmp.display())))?;
    std::fs::rename(&tmp, path).map_err(io_err(format!("cannot write {}", path.display())))
}

/// Folder for the check timestamp and the installer log.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|d| d.data_local_dir().join("updates"))
}

/// Folder that holds the latest download ([`download`] empties it).
#[must_use]
pub fn download_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("downloads"))
}

/// When the last automatic check ran (seconds since the Unix epoch).
#[must_use]
pub fn last_check() -> Option<u64> {
    let path = data_dir()?.join("last-check.txt");
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Records the time of a check.
///
/// # Errors
/// If the file cannot be written.
pub fn record_check(unix_secs: u64) -> Result<(), UpdateError> {
    let dir = data_dir().ok_or_else(|| UpdateError::Io {
        context: "no data folder".into(),
        source: io::Error::from(io::ErrorKind::NotFound),
    })?;
    write_atomic(&dir.join("last-check.txt"), &unix_secs.to_string())
}

/// Whether an automatic check is due.
#[must_use]
pub fn check_due(last: Option<u64>, now: u64) -> bool {
    // A clock that went backwards counts as due.
    last.is_none_or(|last| now < last || now - last >= CHECK_INTERVAL.as_secs())
}

/// Whether the program in `exe_dir` was installed by the Windows installer
/// (and may therefore replace itself with a newer one).
#[must_use]
pub fn installed_by_msi(exe_dir: &Path) -> bool {
    cfg!(windows) && exe_dir.join(MSI_MARKER).is_file()
}

/// The command that installs a downloaded MSI: per-user, progress bar only
/// (the user already confirmed in the app), no reboot, restart the app at the
/// end, verbose log to `log`. The installer itself closes the app and stops
/// the engine before replacing files.
#[must_use]
pub fn msi_install_command(msi: &Path, log: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("msiexec.exe");
    cmd.arg("/i").arg(msi).args(["/passive", "/norestart", "LAUNCHAPP=1", "/l*v"]).arg(log);
    cmd
}

#[cfg(test)]
mod tests;
