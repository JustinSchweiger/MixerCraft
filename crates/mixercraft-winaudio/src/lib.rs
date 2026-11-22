//! Windows audio endpoints: read and restore the default devices, and name
//! MixerCraft's virtual endpoints. Also the plug-and-play state of devices
//! and sleep notifications ([`device_state`], [`watch_power`]), which keep
//! the virtual devices healthy across sleep.
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
//!   only `SetPropertyValue` and `SetDefaultEndpoint` are called.
//! * `PROPVARIANT`s read from property stores are cleared with
//!   `PropVariantClear`. The string in the one passed to `SetPropertyValue`
//!   is allocated with `CoTaskMemAlloc` and handed over: `IPolicyConfig`
//!   frees it (a Rust-owned buffer there corrupted the heap, seen on
//!   hardware). If some Windows version does not free it, a few bytes leak
//!   per rename, which happens rarely.
//! * Configuration Manager calls get NUL-terminated device ids and output
//!   buffers whose stated size matches their real size.
//! * The power-notification context is leaked, never freed, so the callback
//!   can never see a dangling pointer, even while it is being unregistered.
//!   The callback catches panics so none unwinds into Windows.
//! * [`run_elevated`] passes `ShellExecuteExW` NUL-terminated buffers that
//!   outlive the call and closes the returned process handle exactly once.
//!
//! On other platforms the functions return [`Error::Unsupported`].
#![allow(unsafe_code)]

use std::fmt;

mod system;
pub use system::{
    DeviceState, PowerEvent, PowerWatch, SessionEvent, SessionWatch, device_state, filetime_to_system_time,
    run_elevated, watch_power, watch_session_end,
};

/// Errors from the Windows APIs.
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
            Self::Com(m) => write!(f, "Windows API: {m}"),
            Self::Unsupported => f.write_str("only available on Windows"),
        }
    }
}

impl std::error::Error for Error {}

/// Data flow of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// Playback.
    Render,
    /// Recording.
    Capture,
}

/// Windows' three default-device roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Every flow and role (read only by the Windows implementation).
#[cfg_attr(not(windows), allow(dead_code))]
const ALL: [(Flow, Role); 6] = [
    (Flow::Render, Role::Console),
    (Flow::Render, Role::Multimedia),
    (Flow::Render, Role::Communications),
    (Flow::Capture, Role::Console),
    (Flow::Capture, Role::Multimedia),
    (Flow::Capture, Role::Communications),
];

/// An active audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Endpoint id (as used by WASAPI).
    pub id: String,
    /// Playback or recording.
    pub flow: Flow,
    /// The endpoint's own name, e.g. "Speakers" (user-renamable).
    pub description: String,
    /// The device's name, shown in parentheses, e.g. "MixerCraft Game".
    pub device_name: String,
}

/// Lists active playback and recording endpoints.
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn endpoints() -> Result<Vec<Endpoint>, Error> {
    imp::endpoints()
}

/// Sets an endpoint's own name (the part Windows shows before the device
/// name in parentheses), like renaming it in the Sound settings.
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn set_description(id: &str, description: &str) -> Result<(), Error> {
    imp::set_description(id, description)
}

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
    restore_where(saved, |_| true)
}

/// Like [`restore`], but only undoes a change whose current default
/// endpoint id satisfies `undo` (e.g. "is a newly appeared endpoint").
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn restore_where(saved: &Defaults, undo: impl Fn(&str) -> bool) -> Result<usize, Error> {
    let current = snapshot()?;
    let mut changed = 0;
    for (flow, role, id) in &saved.entries {
        match current.get(*flow, *role) {
            Some(now) if now == id => {}
            Some(now) if !undo(now) => {}
            _ => {
                imp::set_default(id, *role)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

/// Makes endpoint `id` the default for `role` (its flow is implied by the
/// endpoint).
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn set_default(id: &str, role: Role) -> Result<(), Error> {
    imp::set_default(id, role)
}

/// Ids of every endpoint Windows knows, including unplugged and disabled
/// ones.
///
/// # Errors
/// COM failures, or [`Error::Unsupported`] off Windows.
pub fn known_endpoint_ids() -> Result<std::collections::HashSet<String>, Error> {
    imp::known_endpoint_ids()
}

#[cfg(windows)]
// COM method names follow the Windows SDK.
#[allow(non_snake_case)]
mod imp {
    use super::Endpoint;
    use super::{ALL, Defaults, Error, Flow, Role};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATEMASK_ALL, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, eAll, eCapture, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToStringAlloc};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemAlloc, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows_core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR, PWSTR, interface};

    /// `PKEY_Device_DeviceDesc`: the endpoint's own name.
    const PKEY_DEVICE_DESC: PROPERTYKEY =
        PROPERTYKEY { fmtid: GUID::from_u128(0xa45c_254e_df1c_4efd_8020_67d1_46a8_50e0), pid: 2 };
    /// `PKEY_DeviceInterface_FriendlyName`: the device's name.
    const PKEY_INTERFACE_NAME: PROPERTYKEY =
        PROPERTYKEY { fmtid: GUID::from_u128(0x026e_516e_b814_414b_83cd_856d_6fef_4822), pid: 2 };

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
        fn SetPropertyValue(
            &self,
            id: PCWSTR,
            fx_store: i32,
            key: *const PROPERTYKEY,
            value: *const PROPVARIANT,
        ) -> HRESULT;
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

    /// Copies a COM-allocated string and frees it.
    ///
    /// # Safety
    /// `raw` must be a NUL-terminated string allocated with `CoTaskMemAlloc`
    /// and not used afterwards.
    unsafe fn take_string(raw: PWSTR) -> Result<String, Error> {
        // SAFETY: per the function contract.
        let s = unsafe { raw.to_string() };
        // SAFETY: freed exactly once; `raw` is not used after this.
        unsafe { CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void)) };
        s.map_err(|e| Error::Com(e.to_string()))
    }

    fn read_string(device: &IMMDevice, key: &PROPERTYKEY) -> String {
        // SAFETY: valid device on this thread; read-only store.
        let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else { return String::new() };
        // SAFETY: `key` is a valid PROPERTYKEY for the call.
        let Ok(mut value) = (unsafe { store.GetValue(key) }) else { return String::new() };
        // SAFETY: `value` is a valid PROPVARIANT; the returned string is
        // COM-allocated and taken (freed) once.
        let text = unsafe { PropVariantToStringAlloc(&value) }
            .ok()
            .and_then(|raw| unsafe { take_string(raw) }.ok())
            .unwrap_or_default();
        // SAFETY: clears the PROPVARIANT returned by GetValue exactly once.
        let _ = unsafe { PropVariantClear(&mut value) };
        text
    }

    pub(super) fn endpoints() -> Result<Vec<Endpoint>, Error> {
        let _com = ComScope::enter();
        // SAFETY: COM is initialised; objects are used on this thread only.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| com(&e))?;
        let mut out = Vec::new();
        for (flow, dataflow) in [(Flow::Render, eRender), (Flow::Capture, eCapture)] {
            let dataflow: EDataFlow = dataflow;
            // SAFETY: valid enumerator.
            let devices =
                unsafe { enumerator.EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE) }.map_err(|e| com(&e))?;
            // SAFETY: valid collection.
            let count = unsafe { devices.GetCount() }.map_err(|e| com(&e))?;
            for i in 0..count {
                // SAFETY: `i` is below the collection's count.
                let Ok(device) = (unsafe { devices.Item(i) }) else { continue };
                // SAFETY: valid device; the id string is taken once.
                let Ok(raw) = (unsafe { device.GetId() }) else { continue };
                // SAFETY: `raw` comes from GetId (CoTaskMemAlloc) and is not reused.
                let Ok(id) = (unsafe { take_string(raw) }) else { continue };
                out.push(Endpoint {
                    id,
                    flow,
                    description: read_string(&device, &PKEY_DEVICE_DESC),
                    device_name: read_string(&device, &PKEY_INTERFACE_NAME),
                });
            }
        }
        Ok(out)
    }

    pub(super) fn known_endpoint_ids() -> Result<std::collections::HashSet<String>, Error> {
        let _com = ComScope::enter();
        // SAFETY: COM is initialised; objects are used on this thread only.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(|e| com(&e))?;
        // SAFETY: valid enumerator.
        let devices =
            unsafe { enumerator.EnumAudioEndpoints(eAll, DEVICE_STATE(DEVICE_STATEMASK_ALL)) }.map_err(|e| com(&e))?;
        // SAFETY: valid collection.
        let count = unsafe { devices.GetCount() }.map_err(|e| com(&e))?;
        let mut ids = std::collections::HashSet::new();
        for i in 0..count {
            // SAFETY: `i` is below the collection's count.
            let Ok(device) = (unsafe { devices.Item(i) }) else { continue };
            // SAFETY: valid device; the id string is taken once.
            let Ok(raw) = (unsafe { device.GetId() }) else { continue };
            // SAFETY: `raw` comes from GetId (CoTaskMemAlloc) and is not reused.
            if let Ok(id) = unsafe { take_string(raw) } {
                ids.insert(id);
            }
        }
        Ok(ids)
    }

    pub(super) fn set_description(id: &str, description: &str) -> Result<(), Error> {
        let _com = ComScope::enter();
        let wide_id: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        let wide_text: Vec<u16> = description.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: COM is initialised; the object is used on this thread only.
        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }.map_err(|e| com(&e))?;
        // SAFETY: allocates a COM buffer large enough for the string; its
        // ownership passes to SetPropertyValue below (see the crate docs).
        let buffer = unsafe { CoTaskMemAlloc(wide_text.len() * 2) }.cast::<u16>();
        if buffer.is_null() {
            return Err(Error::Com("out of memory".into()));
        }
        // SAFETY: `buffer` holds `wide_text.len()` u16s and does not overlap.
        unsafe { std::ptr::copy_nonoverlapping(wide_text.as_ptr(), buffer, wide_text.len()) };
        let mut value = PROPVARIANT::default();
        // SAFETY: writes the VT_LPWSTR variant of a zeroed PROPVARIANT.
        unsafe {
            let inner = &mut *value.Anonymous.Anonymous;
            inner.vt = VT_LPWSTR;
            inner.Anonymous.pwszVal = PWSTR(buffer);
        }
        // SAFETY: all pointers are valid for the duration of the call.
        unsafe { policy.SetPropertyValue(PCWSTR(wide_id.as_ptr()), 0, &PKEY_DEVICE_DESC, &value) }
            .ok()
            .map_err(|e| com(&e))
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
    use super::{Defaults, Endpoint, Error, Role};

    pub(super) fn endpoints() -> Result<Vec<Endpoint>, Error> {
        Err(Error::Unsupported)
    }

    pub(super) fn known_endpoint_ids() -> Result<std::collections::HashSet<String>, Error> {
        Err(Error::Unsupported)
    }

    pub(super) fn set_description(_id: &str, _description: &str) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

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
