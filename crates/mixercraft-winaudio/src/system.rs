//! Plug-and-play device state and power (sleep) notifications.
//!
//! MixerCraft's virtual devices are USB devices that Windows must remove and
//! detect again whenever the engine reconnects them. If the system goes to
//! sleep in the middle of that, Windows can keep the old copy forever (seen
//! on hardware: every application then got "device invalidated" until
//! Windows was restarted). The engine uses [`device_state`] to wait until an
//! old copy is gone and to confirm that a new one arrived, and
//! [`watch_power`] to connect nothing while the system suspends.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Error;

/// What Windows knows about a device instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceState {
    /// The device is plugged in as far as Windows is concerned.
    pub present: bool,
    /// When Windows last detected it.
    pub arrived: Option<SystemTime>,
    /// When Windows last finished removing it.
    pub removed: Option<SystemTime>,
}

/// The plug-and-play state of the device instance `instance_id` (e.g.
/// `USB\VID_1209&PID_0001\MC1234ABCD`), or `None` if Windows has never seen
/// it.
///
/// # Errors
/// Configuration Manager failures, or [`Error::Unsupported`] off Windows.
pub fn device_state(instance_id: &str) -> Result<Option<DeviceState>, Error> {
    imp::device_state(instance_id)
}

/// A system power transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    /// The system is about to sleep or hibernate.
    Suspend,
    /// The system woke up.
    Resume,
}

/// Delivers [`PowerEvent`]s until dropped (see [`watch_power`]).
pub struct PowerWatch {
    /// Unregisters on drop.
    _registration: imp::Registration,
}

impl std::fmt::Debug for PowerWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerWatch").finish_non_exhaustive()
    }
}

/// Calls `on_event` on a system thread when the system suspends or resumes.
/// Keep the callback short; Windows waits for it before sleeping.
///
/// # Errors
/// If Windows refuses the registration, or [`Error::Unsupported`] off
/// Windows.
pub fn watch_power(on_event: impl Fn(PowerEvent) + Send + Sync + 'static) -> Result<PowerWatch, Error> {
    imp::register(Box::new(on_event)).map(|r| PowerWatch { _registration: r })
}

/// Converts a Windows `FILETIME` (100 ns ticks since 1601) to a time.
#[must_use]
pub fn filetime_to_system_time(ticks: u64) -> Option<SystemTime> {
    /// 1601-01-01 to 1970-01-01 in 100 ns ticks.
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let since_unix = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    UNIX_EPOCH.checked_add(Duration::from_nanos(since_unix.checked_mul(100)?))
}

type OnEvent = Box<dyn Fn(PowerEvent) + Send + Sync>;

#[cfg(windows)]
mod imp {
    use super::{DeviceState, Error, OnEvent, PowerEvent, filetime_to_system_time};
    use std::ffi::c_void;
    use std::time::SystemTime;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CM_Locate_DevNodeW,
        CR_NO_SUCH_DEVNODE, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties::{
        DEVPKEY_Device_LastArrivalDate, DEVPKEY_Device_LastRemovalDate, DEVPROP_TYPE_FILETIME, DEVPROPTYPE,
    };
    use windows::Win32::Foundation::{DEVPROPKEY, ERROR_SUCCESS, HANDLE};
    use windows::Win32::System::Power::{
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
        PowerUnregisterSuspendResumeNotification,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    };
    use windows_core::PCWSTR;

    pub(super) fn device_state(instance_id: &str) -> Result<Option<DeviceState>, Error> {
        let wide: Vec<u16> = instance_id.encode_utf16().chain(Some(0)).collect();
        let mut inst = 0u32;
        // SAFETY: `wide` is NUL-terminated and outlives the call; `inst` is a
        // valid out pointer.
        let present =
            unsafe { CM_Locate_DevNodeW(&raw mut inst, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) } == CR_SUCCESS;
        if !present {
            // SAFETY: as above.
            let r = unsafe { CM_Locate_DevNodeW(&raw mut inst, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_PHANTOM) };
            if r == CR_NO_SUCH_DEVNODE {
                return Ok(None);
            }
            if r != CR_SUCCESS {
                return Err(Error::Com(format!("CM_Locate_DevNodeW failed ({})", r.0)));
            }
        }
        Ok(Some(DeviceState {
            present,
            arrived: filetime(inst, &DEVPKEY_Device_LastArrivalDate),
            removed: filetime(inst, &DEVPKEY_Device_LastRemovalDate),
        }))
    }

    /// A `FILETIME` property of a device node, if set.
    fn filetime(inst: u32, key: &DEVPROPKEY) -> Option<SystemTime> {
        let mut kind = DEVPROPTYPE(0);
        let mut buf = [0u8; 8];
        let mut size = buf.len() as u32;
        // SAFETY: `buf` holds `size` bytes; every pointer is valid for the
        // call. Configuration Manager writes at most `size` bytes.
        let r = unsafe { CM_Get_DevNode_PropertyW(inst, key, &raw mut kind, Some(buf.as_mut_ptr()), &raw mut size, 0) };
        if r != CR_SUCCESS || kind != DEVPROP_TYPE_FILETIME || size != 8 {
            return None;
        }
        filetime_to_system_time(u64::from_le_bytes(buf))
    }

    /// Lives until the process ends: Windows may still be running the
    /// callback while it is being unregistered, so the context is leaked
    /// rather than freed (one small allocation per registration).
    struct Context {
        on_event: OnEvent,
        params: DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
    }

    pub(crate) struct Registration {
        handle: isize,
    }

    unsafe extern "system" fn callback(context: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
        // SAFETY: `context` is the leaked `Context` passed at registration;
        // it is never freed, so it is valid for every call.
        let ctx = unsafe { &*context.cast::<Context>() };
        let event = match kind {
            PBT_APMSUSPEND => Some(PowerEvent::Suspend),
            PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => Some(PowerEvent::Resume),
            _ => None,
        };
        if let Some(e) = event {
            // A panic must not unwind into Windows.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.on_event)(e)));
        }
        ERROR_SUCCESS.0
    }

    pub(super) fn register(on_event: OnEvent) -> Result<Registration, Error> {
        let ctx: &'static mut Context =
            Box::leak(Box::new(Context { on_event, params: DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS::default() }));
        ctx.params.Callback = Some(callback);
        ctx.params.Context = std::ptr::from_mut(ctx).cast();
        let mut handle: *mut c_void = std::ptr::null_mut();
        // SAFETY: `params` lives forever (leaked) and points at the leaked
        // context; `handle` is a valid out pointer.
        let r = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                HANDLE(std::ptr::from_ref(&ctx.params).cast_mut().cast()),
                &raw mut handle,
            )
        };
        if r != ERROR_SUCCESS {
            return Err(Error::Com(format!("PowerRegisterSuspendResumeNotification failed ({})", r.0)));
        }
        Ok(Registration { handle: handle as isize })
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            // SAFETY: `handle` came from a successful registration and is
            // unregistered exactly once.
            let _ = unsafe { PowerUnregisterSuspendResumeNotification(HPOWERNOTIFY(self.handle)) };
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{DeviceState, Error, OnEvent};

    pub(crate) struct Registration;

    pub(super) fn device_state(_instance_id: &str) -> Result<Option<DeviceState>, Error> {
        Err(Error::Unsupported)
    }

    pub(super) fn register(_on_event: OnEvent) -> Result<Registration, Error> {
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetimes_convert() {
        // 2026-11-17 00:00:00 UTC.
        let t = filetime_to_system_time(134_393_472_000_000_000).unwrap();
        assert_eq!(t.duration_since(UNIX_EPOCH).unwrap().as_secs(), 1_794_873_600);
        assert_eq!(filetime_to_system_time(0), None, "before 1970");
    }

    /// The root of the device tree is always present; unknown ids are none.
    #[test]
    #[cfg(windows)]
    fn device_state_reads_the_device_tree() {
        let root = device_state(r"HTREE\ROOT\0").unwrap().unwrap();
        assert!(root.present);
        assert_eq!(device_state(r"USB\VID_1209&PID_0001\MCNOSUCHDEVICE").unwrap(), None);
    }

    #[test]
    #[cfg(windows)]
    fn power_notifications_register() {
        let watch = watch_power(|_| {}).unwrap();
        drop(watch);
    }
}
