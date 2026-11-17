//! The platform capability model.
//!
//! Every user-facing feature that depends on the operating system has an
//! explicit support state per platform. The UI must consult this model and
//! hide or disable (with an explanation) anything that is not
//! [`Support::Full`] or [`Support::Partial`] — never show a control that
//! silently does nothing.
//!
//! The table here describes **this build**, not the roadmap. Keep it in sync
//! with `docs/feature-matrix.md`.

/// Support state of a feature on the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// Implemented and expected to work.
    Full,
    /// Works with documented limitations.
    Partial,
    /// Possible on this platform but not implemented yet.
    Planned,
    /// Not possible due to platform restrictions.
    Unsupported,
}

/// A platform-dependent feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// Capture from and play to physical devices.
    PhysicalIo,
    /// Mixing several devices with independent clocks (drift compensation).
    MultiDeviceClockSync,
    /// Streams that follow the system default device.
    FollowDefaultDevice,
    /// Virtual playback/recording endpoints visible to other applications.
    VirtualEndpoints,
    /// Capturing the audio of individual applications.
    PerAppCapture,
    /// Running the engine before any user logs in.
    PreLoginEngine,
    /// Engine starts automatically with the user session.
    SessionAutostart,
    /// In-app update installation.
    InAppUpdates,
}

impl Feature {
    /// All features, for building the UI and diagnostics tables.
    pub const ALL: [Feature; 8] = [
        Feature::PhysicalIo,
        Feature::MultiDeviceClockSync,
        Feature::FollowDefaultDevice,
        Feature::VirtualEndpoints,
        Feature::PerAppCapture,
        Feature::PreLoginEngine,
        Feature::SessionAutostart,
        Feature::InAppUpdates,
    ];

    /// Short display name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::PhysicalIo => "Physical device I/O",
            Self::MultiDeviceClockSync => "Multi-device clock sync",
            Self::FollowDefaultDevice => "Follow default device",
            Self::VirtualEndpoints => "Virtual endpoints",
            Self::PerAppCapture => "Per-application capture",
            Self::PreLoginEngine => "Engine before login",
            Self::SessionAutostart => "Autostart with session",
            Self::InAppUpdates => "In-app updates",
        }
    }
}

/// Support state and explanation of `feature` in this build on this OS.
#[must_use]
pub fn support(feature: Feature) -> (Support, &'static str) {
    use Feature as F;
    use Support as S;
    match feature {
        F::PhysicalIo => (S::Full, "Shared-mode streams via the native audio API"),
        F::MultiDeviceClockSync => (S::Full, "Drift-compensated resampling on every non-master device"),
        F::FollowDefaultDevice => follow_default(),
        F::VirtualEndpoints => virtual_endpoints(),
        F::PerAppCapture => per_app_capture(),
        F::PreLoginEngine => pre_login(),
        F::SessionAutostart => session_autostart(),
        F::InAppUpdates => (S::Planned, "Updater is scheduled for phase 9"),
    }
}

#[cfg(windows)]
fn follow_default() -> (Support, &'static str) {
    (Support::Full, "WASAPI reroutes default-device streams automatically")
}
#[cfg(not(windows))]
fn follow_default() -> (Support, &'static str) {
    (Support::Partial, "Default device is resolved when the stream opens; changes apply on restart")
}

#[cfg(windows)]
fn session_autostart() -> (Support, &'static str) {
    (
        Support::Partial,
        "Task Scheduler logon task (`mixercraft autostart enable`); start at actual sign-in not yet verified",
    )
}
#[cfg(target_os = "linux")]
fn session_autostart() -> (Support, &'static str) {
    (Support::Partial, "systemd user unit (`mixercraft autostart enable`); not yet verified on hardware")
}
#[cfg(target_os = "macos")]
fn session_autostart() -> (Support, &'static str) {
    (Support::Partial, "LaunchAgent (`mixercraft autostart enable`); not yet verified on hardware")
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn session_autostart() -> (Support, &'static str) {
    (Support::Unsupported, "No supported service manager")
}

#[cfg(windows)]
fn virtual_endpoints() -> (Support, &'static str) {
    match crate::virtual_dev::unavailable_reason() {
        None => (Support::Partial, "Emulated USB audio devices through usbip-win2; about 65 ms between applications"),
        Some(_) => (Support::Unsupported, "Install usbip-win2 to create virtual devices (see ADR 0007)"),
    }
}
#[cfg(target_os = "linux")]
fn virtual_endpoints() -> (Support, &'static str) {
    match crate::virtual_dev::unavailable_reason() {
        None => (
            Support::Partial,
            "PipeWire sink/source nodes owned by the engine; tested against PipeWire without audio hardware only",
        ),
        Some(_) => (Support::Unsupported, "Virtual devices need PipeWire running in this session"),
    }
}
#[cfg(target_os = "macos")]
fn virtual_endpoints() -> (Support, &'static str) {
    (Support::Planned, "Requires an Audio Server Plug-in installed by the package")
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn virtual_endpoints() -> (Support, &'static str) {
    (Support::Unsupported, "No virtual device mechanism on this platform")
}

#[cfg(windows)]
fn per_app_capture() -> (Support, &'static str) {
    (Support::Planned, "WASAPI process loopback (Windows 10 2004+)")
}
#[cfg(target_os = "linux")]
fn per_app_capture() -> (Support, &'static str) {
    (Support::Planned, "PipeWire stream linking/target metadata")
}
#[cfg(target_os = "macos")]
fn per_app_capture() -> (Support, &'static str) {
    (Support::Planned, "Core Audio process taps (macOS 14.2+)")
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn per_app_capture() -> (Support, &'static str) {
    (Support::Unsupported, "No per-application capture API")
}

#[cfg(windows)]
fn pre_login() -> (Support, &'static str) {
    (Support::Planned, "Windows service; feasibility must be verified on hardware")
}
#[cfg(target_os = "linux")]
fn pre_login() -> (Support, &'static str) {
    (Support::Unsupported, "PipeWire runs per user session; the engine starts with the session")
}
#[cfg(target_os = "macos")]
fn pre_login() -> (Support, &'static str) {
    (Support::Unsupported, "Firmware startup chime is not routable; engine starts as a LaunchAgent")
}
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn pre_login() -> (Support, &'static str) {
    (Support::Unsupported, "Not supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_feature_has_an_explanation() {
        for f in Feature::ALL {
            let (_, why) = support(f);
            assert!(!why.is_empty(), "{f:?}");
            assert!(!f.name().is_empty());
        }
        assert_eq!(support(Feature::PhysicalIo).0, Support::Full);
    }
}
