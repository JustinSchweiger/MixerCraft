//! Plug-and-play device state, power (sleep) and session-end notifications.
//!
//! MixerCraft's virtual devices are USB devices that Windows must remove and
//! detect again whenever the engine reconnects them. If Windows saves its
//! state in the middle of that (shutting down with Fast Startup, or going to
//! sleep), it can keep the old copy until the next real restart (seen on
//! hardware: every application then got "device invalidated"). The engine
//! uses [`device_state`] to wait until an old copy is gone and to confirm
//! that a new one arrived, [`watch_session_end`] to unplug its devices
//! cleanly before the user session ends, and [`watch_power`] to connect
//! nothing while the system suspends.

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

/// The user session (sign-out, shutdown, restart) is ending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// Windows asks whether the session may end (`WM_QUERYENDSESSION`).
    Ending,
    /// Another application refused; the session goes on.
    Cancelled,
    /// The session ends (`WM_ENDSESSION`). The process may be terminated as
    /// soon as the callback returns; Windows waits for it a few seconds.
    Ended,
}

/// Delivers [`SessionEvent`]s until dropped (see [`watch_session_end`]).
pub struct SessionWatch {
    /// Closes the window on drop.
    _window: imp::SessionWindow,
}

impl std::fmt::Debug for SessionWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionWatch").finish_non_exhaustive()
    }
}

/// Calls `on_event` when the user session ends (sign-out, shutdown, also
/// the shutdown that Fast Startup turns into hibernation).
///
/// Windows tells only processes with a top-level window; others (the engine
/// has no window) are terminated without notice. This creates a hidden
/// window on its own thread. The callback runs on that thread; for
/// [`SessionEvent::Ended`] it may take a few seconds to clean up.
///
/// # Errors
/// If the window cannot be created, or [`Error::Unsupported`] off Windows.
pub fn watch_session_end(on_event: impl Fn(SessionEvent) + Send + Sync + 'static) -> Result<SessionWatch, Error> {
    imp::session_window(Box::new(on_event)).map(|w| SessionWatch { _window: w })
}

/// Runs `program` as administrator (Windows asks the user) and waits for
/// it. `parameters` is the whole command line after the program, already
/// quoted. Returns its exit code, or `None` if the user said no.
///
/// # Errors
/// If Windows cannot start it, or [`Error::Unsupported`] off Windows.
pub fn run_elevated(program: &std::path::Path, parameters: &str) -> Result<Option<u32>, Error> {
    imp::run_elevated(program, parameters)
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
type OnSession = Box<dyn Fn(SessionEvent) + Send + Sync>;

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

    pub(super) use session::{SessionWindow, session_window};

    pub(super) fn run_elevated(program: &std::path::Path, parameters: &str) -> Result<Option<u32>, Error> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
        use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
        use windows::Win32::UI::Shell::{
            SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

        let file: Vec<u16> = program.as_os_str().encode_wide().chain(Some(0)).collect();
        let params: Vec<u16> = parameters.encode_utf16().chain(Some(0)).collect();
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: windows_core::w!("runas"),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: `info` is fully initialised; `file` and `params` are
        // NUL-terminated and outlive the call.
        if let Err(e) = unsafe { ShellExecuteExW(&raw mut info) } {
            if e.code() == windows_core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                return Ok(None);
            }
            return Err(Error::Com(format!("cannot start {} as administrator: {e}", program.display())));
        }
        if info.hProcess.is_invalid() {
            return Err(Error::Com("no process handle from ShellExecuteExW".into()));
        }
        let mut code = 0u32;
        // SAFETY: `hProcess` is the valid handle Windows returned
        // (SEE_MASK_NOCLOSEPROCESS); it is closed exactly once below.
        let r = unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
            let r = GetExitCodeProcess(info.hProcess, &raw mut code);
            let _ = CloseHandle(info.hProcess);
            r
        };
        r.map_err(|e| Error::Com(format!("GetExitCodeProcess failed: {e}")))?;
        Ok(Some(code))
    }

    #[cfg(test)]
    pub(super) fn session_class() -> windows_core::PCWSTR {
        session::CLASS
    }

    mod session {
        use super::super::{Error, OnSession, SessionEvent};
        use std::ffi::c_void;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::System::Threading::GetCurrentThreadId;
        use windows::Win32::UI::WindowsAndMessaging::{
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetMessageW,
            GetWindowLongPtrW, MSG, PostThreadMessageW, RegisterClassW, SetWindowLongPtrW, WINDOW_EX_STYLE,
            WM_ENDSESSION, WM_NCCREATE, WM_QUERYENDSESSION, WM_QUIT, WNDCLASSW, WS_OVERLAPPED,
        };
        use windows_core::w;

        /// The window class; registered once per process.
        pub(super) const CLASS: windows_core::PCWSTR = w!("MixerCraftSessionWatch");

        /// Ends the window's thread on drop.
        pub(crate) struct SessionWindow {
            thread_id: u32,
        }

        impl Drop for SessionWindow {
            fn drop(&mut self) {
                // SAFETY: plain message post to a thread id; harmless if the
                // thread has already ended.
                let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            }
        }

        unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
            if msg == WM_NCCREATE {
                // SAFETY: for WM_NCCREATE, `lparam` points at the
                // CREATESTRUCTW whose `lpCreateParams` is the leaked
                // `OnSession` passed to CreateWindowExW.
                let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
                // SAFETY: `hwnd` is the window being created.
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
            }
            let event = match msg {
                WM_QUERYENDSESSION => Some(SessionEvent::Ending),
                WM_ENDSESSION if wparam.0 != 0 => Some(SessionEvent::Ended),
                WM_ENDSESSION => Some(SessionEvent::Cancelled),
                _ => None,
            };
            if let Some(event) = event {
                // SAFETY: GWLP_USERDATA holds the leaked `OnSession` (set at
                // WM_NCCREATE, never freed) or 0 before that.
                let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const OnSession;
                if !ptr.is_null() {
                    // SAFETY: see above; the closure lives forever.
                    let on_event = unsafe { &*ptr };
                    // A panic must not unwind into Windows.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_event(event)));
                }
                // TRUE for WM_QUERYENDSESSION: never block the session end.
                return LRESULT(isize::from(msg == WM_QUERYENDSESSION));
            }
            // SAFETY: default handling with the arguments Windows gave us.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        pub(crate) fn session_window(on_event: OnSession) -> Result<SessionWindow, Error> {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("mixercraft-session-watch".into())
                .spawn(move || {
                    // Leaked: the window procedure may use it until the
                    // process ends.
                    let on_event: &'static OnSession = Box::leak(Box::new(on_event));
                    match create(on_event) {
                        Ok(()) => {
                            // SAFETY: no preconditions.
                            let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
                            pump();
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                        }
                    }
                })
                .map_err(|e| Error::Com(format!("cannot start the session watch thread: {e}")))?;
            let thread_id = rx.recv().map_err(|_| Error::Com("session watch thread ended".into()))??;
            Ok(SessionWindow { thread_id })
        }

        fn create(on_event: &'static OnSession) -> Result<(), Error> {
            // SAFETY: no preconditions; `None` asks for this executable.
            let module = unsafe { GetModuleHandleW(None) }.map_err(|e| Error::Com(e.to_string()))?;
            let instance = HINSTANCE(module.0);
            let class = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            // SAFETY: `class` is fully initialised; registering twice in one
            // process fails harmlessly (the first registration is used).
            unsafe { RegisterClassW(&raw const class) };
            // A hidden top-level window: message-only windows do not get
            // WM_QUERYENDSESSION. SAFETY: the class is registered; the
            // create parameter is the leaked closure (see `wndproc`).
            unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    CLASS,
                    w!("MixerCraft session watch"),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(instance),
                    Some(std::ptr::from_ref(on_event).cast::<c_void>()),
                )
            }
            .map_err(|e| Error::Com(format!("CreateWindowExW failed: {e}")))?;
            Ok(())
        }

        fn pump() {
            let mut msg = MSG::default();
            // SAFETY: `msg` is a valid out pointer; the loop ends on WM_QUIT
            // (0) or an error (-1).
            while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.0 > 0 {
                // SAFETY: `msg` came from GetMessageW.
                unsafe { DispatchMessageW(&raw const msg) };
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{DeviceState, Error, OnEvent, OnSession};

    pub(crate) struct Registration;
    pub(crate) struct SessionWindow;

    pub(super) fn session_window(_on_event: OnSession) -> Result<SessionWindow, Error> {
        Err(Error::Unsupported)
    }

    pub(super) fn run_elevated(_program: &std::path::Path, _parameters: &str) -> Result<Option<u32>, Error> {
        Err(Error::Unsupported)
    }

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

    /// The hidden window turns the session messages into events (sent to
    /// our own window here; Windows sends the same at sign-out/shutdown).
    #[test]
    #[cfg(windows)]
    fn session_end_messages_become_events() {
        use std::sync::{Arc, Mutex};
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SendMessageW, WM_ENDSESSION, WM_QUERYENDSESSION};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = Arc::clone(&seen);
        let watch = watch_session_end(move |e| s.lock().unwrap().push(e)).unwrap();
        // SAFETY: plain window lookup and messages to our own window.
        unsafe {
            let hwnd = FindWindowW(imp::session_class(), windows_core::w!("MixerCraft session watch")).unwrap();
            assert_eq!(SendMessageW(hwnd, WM_QUERYENDSESSION, None, None).0, 1, "never blocks the session end");
            SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(0)), Some(LPARAM(0)));
            SendMessageW(hwnd, WM_ENDSESSION, Some(WPARAM(1)), Some(LPARAM(0)));
        }
        assert_eq!(*seen.lock().unwrap(), [SessionEvent::Ending, SessionEvent::Cancelled, SessionEvent::Ended]);
        drop(watch);
    }
}
