//! Windows default audio endpoints: read them, and set them back.
//!
//! Windows makes a newly connected USB audio device the default playback
//! and recording device. MixerCraft's virtual devices are such devices, so
//! the engine takes a [`snapshot`] before connecting one and [`restore`]s it
//! afterwards. Setting a default endpoint has no public API; this uses the
//! undocumented but long-stable `IPolicyConfig` interface (the same one the
//! Windows sound control panel, EarTrumpet and AudioDeviceCmdlets use).
//!
//! # Unsafe code
//! This crate opts out of the workspace-wide `unsafe_code` denial because it
//! calls COM. Invariants upheld by every `unsafe` block:
//! * COM is initialised on the calling thread for the duration of each call
//!   ([`ComScope`]) and every interface pointer is used and released on that
//!   thread before the scope ends.
//! * Strings passed to COM are NUL-terminated UTF-16 buffers that outlive the
//!   call; strings returned by COM (`PWSTR`) are copied and then freed with
//!   `CoTaskMemFree` exactly once.
//! * `IPolicyConfig` is declared with the vtable layout used since Windows 7;
//!   only `SetDefaultEndpoint` (slot 13, after `IUnknown`'s three) is called.
//!
//! On other platforms the functions return [`Error::Unsupported`].
#![allow(unsafe_code)]

use std::fmt;

/// Errors from the Windows audio API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A COM call failed.
    Com(String),
    /// Not available on this platform.
    Unsupported,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Com(m) => write!(f, "Windows audio API: {m}"),
            Self::Unsupported => f.write_str("default audio endpoints are only managed on Windows"),
        }
    }
}

impl std::error::Error for Error {}

/// Data flow of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Playback.
    Render,
    /// Recording.
    Capture,
}

/// Windows' three default-device roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Games and system sounds.
    Console,
    /// Music and video.
    Multimedia,
    /// Voice chat.
    Communications,
}

/// The default endpoint ids of every flow and role at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Defaults {
    /// `(flow, role, endpoint id)`; roles without a default are absent.
    pub entries: Vec<(Flow, Role, String)>,
}

impl Defaults {
    /// The default endpoint id for `flow` and `role`.
    #[must_use]
    pub fn get(&self, flow: Flow, role: Role) -> Option<&str> {
        self.entries.iter().find(|(f, r, _)| *f == flow && *r == role).map(|(_, _, id)| id.as_str())
    }
}

const ALL: [(Flow, Role); 6] = [
    (Flow::Render, Role::Console),
    (Flow::Render, Role::Multimedia),
    (Flow::Render, Role::Communications),
    (Flow::Capture, Role::Console),
    (Flow::Capture, Role::Multimedia),
    (Flow::Capture, Role::Communications),
];

/// Reads the current default endpoints.
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn snapshot() -> Result<Defaults, Error> {
    imp::snapshot()
}

/// Sets every default endpoint recorded in `saved` that differs from the
/// current one. Returns how many were changed.
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn restore(saved: &Defaults) -> Result<usize, Error> {
    let current = snapshot()?;
    let mut changed = 0;
    for (flow, role, id) in &saved.entries {
        if current.get(*flow, *role) != Some(id.as_str()) {
            imp::set_default(id, *role)?;
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(windows)]
// COM method names follow the Windows SDK.
#[allow(non_snake_case)]
mod imp {
    use super::{ALL, Defaults, Error, Flow, Role};
    use windows::Win32::Media::Audio::{
        ERole, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, interface};

    fn com(e: &windows_core::Error) -> Error {
        Error::Com(e.message())
    }

    /// Initialises COM on this thread for its lifetime (if not already).
    struct ComScope {
        owned: bool,
    }

    impl ComScope {
        fn enter() -> Self {
            // SAFETY: no reserved pointer; balanced by CoUninitialize in Drop
            // only when this call initialised COM (S_OK / S_FALSE). A thread
            // already in another apartment returns RPC_E_CHANGED_MODE, which
            // still allows calls.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            Self { owned: hr.is_ok() }
        }
    }

    impl Drop for ComScope {
        fn drop(&mut self) {
            if self.owned {
                // SAFETY: balances the successful CoInitializeEx above on
                // the same thread; all interfaces were released before.
                unsafe { CoUninitialize() };
            }
        }
    }

    const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870a_f99c_171d_4f9e_af0d_e63d_f40c_2bc9);

    /// Undocumented; vtable layout since Windows 7. Parameter types of the
    /// unused methods are placeholders that only keep the slot order.
    #[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    unsafe trait IPolicyConfig: IUnknown {
        fn GetMixFormat(&self, id: PCWSTR, format: *mut *mut core::ffi::c_void) -> HRESULT;
        fn GetDeviceFormat(&self, id: PCWSTR, default: i32, format: *mut *mut core::ffi::c_void) -> HRESULT;
        fn ResetDeviceFormat(&self, id: PCWSTR) -> HRESULT;
        fn SetDeviceFormat(&self, id: PCWSTR, endpoint: *mut core::ffi::c_void, mix: *mut core::ffi::c_void)
        -> HRESULT;
        fn GetProcessingPeriod(&self, id: PCWSTR, default: i32, period: *mut i64, minimum: *mut i64) -> HRESULT;
        fn SetProcessingPeriod(&self, id: PCWSTR, period: *mut i64) -> HRESULT;
        fn GetShareMode(&self, id: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
        fn SetShareMode(&self, id: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
        fn GetPropertyValue(&self, id: PCWSTR, key: *const core::ffi::c_void, value: *mut core::ffi::c_void)
        -> HRESULT;
        fn SetPropertyValue(&self, id: PCWSTR, key: *const core::ffi::c_void, value: *mut core::ffi::c_void)
        -> HRESULT;
        fn SetDefaultEndpoint(&self, id: PCWSTR, role: ERole) -> HRESULT;
        fn SetEndpointVisibility(&self, id: PCWSTR, visible: i32) -> HRESULT;
    }

    fn erole(role: Role) -> ERole {
        match role {
            Role::Console => eConsole,
            Role::Multimedia => eMultimedia,
            Role::Communications => eCommunications,
        }
    }

    pub(super) fn snapshot() -> Result<Defaults, Error> {
        let _com = ComScope::enter();
        // SAFETY: COM is initialised (ComScope); the enumerator is used and
        // released on this thread.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| com(&e))?;
        let mut entries = Vec::new();
        for (flow, role) in ALL {
            let dataflow = if flow == Flow::Render { eRender } else { eCapture };
            // SAFETY: valid enumerator; a missing default returns an error,
            // which only means "no default" here.
            let Ok(device) = (unsafe { enumerator.GetDefaultAudioEndpoint(dataflow, erole(role)) }) else {
                continue;
            };
            // SAFETY: valid device; the returned PWSTR is copied and freed
            // exactly once below.
            let raw = unsafe { device.GetId() }.map_err(|e| com(&e))?;
            // SAFETY: `raw` is a NUL-terminated string allocated by COM.
            let id = unsafe { raw.to_string() };
            // SAFETY: frees the COM allocation once; `raw` is not used after.
            unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
            entries.push((flow, role, id.map_err(|e| Error::Com(e.to_string()))?));
        }
        Ok(Defaults { entries })
    }

    pub(super) fn set_default(id: &str, role: Role) -> Result<(), Error> {
        let _com = ComScope::enter();
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: COM is initialised; the object is used and released on this
        // thread.
        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }.map_err(|e| com(&e))?;
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        unsafe { policy.SetDefaultEndpoint(PCWSTR(wide.as_ptr()), erole(role)) }.ok().map_err(|e| com(&e))
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{Defaults, Error, Role};

    pub(super) fn snapshot() -> Result<Defaults, Error> {
        Err(Error::Unsupported)
    }

    pub(super) fn set_default(_id: &str, _role: Role) -> Result<(), Error> {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_finds_entries() {
        let d = Defaults { entries: vec![(Flow::Render, Role::Console, "a".into())] };
        assert_eq!(d.get(Flow::Render, Role::Console), Some("a"));
        assert_eq!(d.get(Flow::Capture, Role::Console), None);
    }

    /// Reading the defaults works on a Windows machine with audio devices;
    /// restoring the unchanged snapshot changes nothing.
    #[test]
    #[cfg(windows)]
    fn snapshot_and_noop_restore() {
        let Ok(d) = snapshot() else { return }; // no audio service (CI)
        assert_eq!(restore(&d), Ok(0));
    }
}
