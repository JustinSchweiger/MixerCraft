//! Win32 implementation.
//!
//! # Invariants of the `unsafe` blocks
//! * Every Win32 call takes plain values or pointers to locals that outlive
//!   the call (`MSG`); nothing is retained by the system after it returns.
//! * Hotkeys are registered with a null window, so `WM_HOTKEY` arrives in the
//!   queue of the registering thread. Registration, unregistration and the
//!   message loop all run on that one thread ([`Listener`]'s worker); other
//!   threads only post messages to it (`PostThreadMessageW`, which is
//!   thread-safe). The worker creates its queue (`PeekMessageW`) before its
//!   id is handed out, so no posted message is lost.
//! * The low-level keyboard hook of [`capture`] is installed and removed on
//!   the same thread, which runs a message loop the whole time (the hook is
//!   called from it). The hook procedure only reads the `KBDLLHOOKSTRUCT`
//!   the system passes for `HC_ACTION` and touches thread-local state of
//!   that thread; it never blocks, so Windows does not time it out.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
    UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, KBDLLHOOKSTRUCT, KillTimer, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP, WM_HOTKEY,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_TIMER,
};

use crate::{Combo, Error, Event, Key, Registration};

/// While a hotkey is held, its key is polled this often to detect release.
const RELEASE_POLL_MS: u32 = 15;
/// Longest wait for the worker to apply a new set of hotkeys.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(2);
/// Worker message: new bindings are waiting in the channel.
const WM_REREGISTER: u32 = WM_APP + 1;

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
const VK_LWIN: i32 = 0x5B;
const VK_RWIN: i32 = 0x5C;
const VK_ESCAPE: u32 = 0x1B;

fn key_down(vk: i32) -> bool {
    // SAFETY: plain value in, plain value out.
    unsafe { GetAsyncKeyState(vk) < 0 }
}

fn post(thread: u32, msg: u32) -> bool {
    // SAFETY: posting to a thread id is valid for any id; a dead thread
    // makes the call fail, which the callers tolerate.
    unsafe { PostThreadMessageW(thread, msg, WPARAM(0), LPARAM(0)).is_ok() }
}

type Callback = Box<dyn FnMut(Event) + Send>;

#[derive(Debug)]
pub(crate) struct Listener {
    thread: u32,
    join: Option<JoinHandle<()>>,
    requests: Sender<Vec<(u32, Combo)>>,
    results: Receiver<Vec<Registration>>,
}

impl Listener {
    pub(crate) fn start(on_event: Callback) -> Result<Self, Error> {
        let (req_tx, req_rx) = mpsc::channel();
        let (res_tx, res_rx) = mpsc::channel();
        let (id_tx, id_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("mixercraft-hotkeys".into())
            .spawn(move || {
                let mut msg = MSG::default();
                // SAFETY: creates this thread's message queue; `msg` is a
                // local that outlives the call.
                unsafe {
                    let _ = PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE);
                }
                // SAFETY: no arguments.
                let _ = id_tx.send(unsafe { GetCurrentThreadId() });
                Worker { on_event, requests: req_rx, results: res_tx, active: HashMap::new(), held: Vec::new() }.run();
            })
            .map_err(|e| Error::Os(format!("cannot start the hotkey thread: {e}")))?;
        let thread = id_rx.recv().map_err(|_| Error::Os("the hotkey thread exited at start".into()))?;
        Ok(Self { thread, join: Some(join), requests: req_tx, results: res_rx })
    }

    pub(crate) fn register(&self, bindings: &[(u32, Combo)]) -> Vec<Registration> {
        let failed = |why: &str| bindings.iter().map(|(id, _)| (*id, Err(why.to_owned()))).collect();
        // Drop answers to earlier requests that timed out.
        while self.results.try_recv().is_ok() {}
        if self.requests.send(bindings.to_vec()).is_err() || !post(self.thread, WM_REREGISTER) {
            return failed("the hotkey thread is not running");
        }
        self.results.recv_timeout(REGISTER_TIMEOUT).unwrap_or_else(|_| failed("the hotkey thread did not answer"))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        post(self.thread, WM_QUIT);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct Worker {
    on_event: Callback,
    requests: Receiver<Vec<(u32, Combo)>>,
    results: Sender<Vec<Registration>>,
    /// Win32 hotkey id → (binding id, virtual-key code).
    active: HashMap<i32, (u32, u16)>,
    /// Hotkeys currently held down: (binding id, virtual-key code).
    held: Vec<(u32, u16)>,
}

impl Worker {
    fn run(mut self) {
        let mut msg = MSG::default();
        let mut timer = 0usize;
        loop {
            // SAFETY: `msg` outlives the call; null window = thread messages.
            let r = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
            if r.0 <= 0 {
                break; // WM_QUIT or error
            }
            match msg.message {
                WM_REREGISTER => {
                    while let Ok(bindings) = self.requests.try_recv() {
                        let res = self.reregister(&bindings);
                        let _ = self.results.send(res);
                    }
                }
                WM_HOTKEY => {
                    let Ok(hk) = i32::try_from(msg.wParam.0) else { continue };
                    let Some(&(id, vk)) = self.active.get(&hk) else { continue };
                    if self.held.iter().any(|h| h.0 == id) {
                        continue;
                    }
                    self.held.push((id, vk));
                    (self.on_event)(Event { id, pressed: true });
                    if timer == 0 {
                        // SAFETY: thread timer without window or callback;
                        // WM_TIMER arrives in this loop.
                        timer = unsafe { SetTimer(None, 0, RELEASE_POLL_MS, None) };
                    }
                }
                WM_TIMER if timer != 0 && msg.wParam.0 == timer => {
                    let on_event = &mut self.on_event;
                    self.held.retain(|&(id, vk)| {
                        let down = key_down(i32::from(vk));
                        if !down {
                            on_event(Event { id, pressed: false });
                        }
                        down
                    });
                    if self.held.is_empty() {
                        // SAFETY: `timer` was returned by SetTimer on this thread.
                        unsafe {
                            let _ = KillTimer(None, timer);
                        }
                        timer = 0;
                    }
                }
                _ => {}
            }
        }
        self.unregister_all();
    }

    fn unregister_all(&mut self) {
        for hk in self.active.keys() {
            // SAFETY: the id was registered on this thread with no window.
            unsafe {
                let _ = UnregisterHotKey(None, *hk);
            }
        }
        self.active.clear();
    }

    fn reregister(&mut self, bindings: &[(u32, Combo)]) -> Vec<Registration> {
        self.unregister_all();
        // Releases of keys that are no longer bound are still reported, so a
        // held push-to-talk never sticks.
        let mut out = Vec::with_capacity(bindings.len());
        for (n, (id, combo)) in bindings.iter().enumerate() {
            let Ok(hk) = i32::try_from(n + 1) else { break };
            let mut mods = MOD_NOREPEAT;
            for (on, m) in
                [(combo.ctrl, MOD_CONTROL), (combo.alt, MOD_ALT), (combo.shift, MOD_SHIFT), (combo.win, MOD_WIN)]
            {
                if on {
                    mods = HOT_KEY_MODIFIERS(mods.0 | m.0);
                }
            }
            let vk = combo.key.windows_vk();
            // SAFETY: null window: WM_HOTKEY goes to this thread's queue.
            let res = unsafe { RegisterHotKey(None, hk, mods, u32::from(vk)) };
            match res {
                Ok(()) => {
                    self.active.insert(hk, (*id, vk));
                    out.push((*id, Ok(())));
                }
                Err(e) => {
                    let why = if e.code().0 as u32 & 0xFFFF == 1409 {
                        // ERROR_HOTKEY_ALREADY_REGISTERED
                        format!("{combo} is already used by another application or by Windows")
                    } else {
                        format!("cannot register {combo}: {}", e.message())
                    };
                    out.push((*id, Err(why)));
                }
            }
        }
        out
    }
}

thread_local! {
    /// Result of the running capture: `Some(None)` = cancelled.
    static CAPTURED: Cell<Option<Option<Combo>>> = const { Cell::new(None) };
    static CAPTURE_THREAD: Cell<u32> = const { Cell::new(0) };
}

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x10..=0x12 | 0xA0..=0xA5 | 0x5B | 0x5C)
}

unsafe extern "system" fn capture_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN)
        && CAPTURED.with(Cell::get).is_none()
    {
        // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT that is
        // valid for the duration of this call.
        let vk = unsafe { (*(lparam.0 as *const KBDLLHOOKSTRUCT)).vkCode };
        if !is_modifier(vk) {
            let ctrl = key_down(VK_CONTROL);
            let alt = key_down(VK_MENU);
            let shift = key_down(VK_SHIFT);
            let win = key_down(VK_LWIN) || key_down(VK_RWIN);
            let result = if vk == VK_ESCAPE && !(ctrl || alt || shift || win) {
                Some(None)
            } else {
                u16::try_from(vk)
                    .ok()
                    .and_then(Key::from_windows_vk)
                    .map(|key| Some(Combo { ctrl, alt, shift, win, key }))
            };
            if let Some(r) = result {
                CAPTURED.with(|c| c.set(Some(r)));
                post(CAPTURE_THREAD.with(Cell::get), WM_APP);
                // Swallow the key: it was meant for the recorder.
                return LRESULT(1);
            }
        }
    }
    // SAFETY: forwards the unchanged arguments to the next hook.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

pub(crate) fn capture(timeout: Duration) -> Result<Option<Combo>, Error> {
    // SAFETY: no arguments.
    let thread = unsafe { GetCurrentThreadId() };
    CAPTURED.with(|c| c.set(None));
    CAPTURE_THREAD.with(|c| c.set(thread));
    // SAFETY: the module handle of this executable; the hook is removed
    // below on this same thread, which pumps messages meanwhile.
    let hook = unsafe {
        let module = GetModuleHandleW(None).map_err(|e| Error::Os(e.message()))?;
        SetWindowsHookExW(WH_KEYBOARD_LL, Some(capture_hook), Some(module.into()), 0)
            .map_err(|e| Error::Os(format!("cannot record keys: {}", e.message())))?
    };
    let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: thread timer; WM_TIMER arrives in the loop below.
    let timer = unsafe { SetTimer(None, 0, ms.max(1), None) };
    let deadline = Instant::now() + timeout;
    let mut msg = MSG::default();
    while CAPTURED.with(Cell::get).is_none() && Instant::now() < deadline {
        // SAFETY: `msg` outlives the call. The hook runs inside this call.
        let r = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
    }
    // SAFETY: both were created above on this thread.
    unsafe {
        let _ = KillTimer(None, timer);
        let _ = UnhookWindowsHookEx(hook);
    }
    Ok(CAPTURED.with(Cell::take).flatten())
}
