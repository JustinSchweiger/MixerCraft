//! End-to-end checks against the real Windows input system: synthesized key
//! presses (`SendInput`) must fire a registered hotkey (press and release)
//! and be seen by the recorder.
//!
//! They need an interactive desktop and inject keys into whatever window
//! has the focus (an unusual combination, swallowed by the hotkey), so they
//! are not run in CI: `cargo test -p mixercraft-hotkey -- --ignored`.
#![cfg(windows)]
#![allow(unsafe_code)]

use std::sync::mpsc;
use std::time::Duration;

use mixercraft_hotkey::{Combo, Event, Listener, capture};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};

const VK_CONTROL: u16 = 0x11;
const VK_MENU: u16 = 0x12;
const VK_SHIFT: u16 = 0x10;

fn key(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: a slice of fully initialised INPUT structures and their size.
    let n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    assert_eq!(n as usize, inputs.len(), "SendInput was blocked (no interactive desktop?)");
}

/// Presses Ctrl+Alt+Shift+`vk`, holds it for `hold`, releases everything.
fn press_combo(vk: u16, hold: Duration) {
    send(&[key(VK_CONTROL, false), key(VK_MENU, false), key(VK_SHIFT, false), key(vk, false)]);
    std::thread::sleep(hold);
    send(&[key(vk, true), key(VK_SHIFT, true), key(VK_MENU, true), key(VK_CONTROL, true)]);
}

#[test]
#[ignore = "injects key presses; needs an interactive desktop"]
fn registered_hotkey_reports_press_and_release() {
    let combo: Combo = "Ctrl+Alt+Shift+F24".parse().unwrap();
    let (tx, rx) = mpsc::channel();
    let listener = Listener::start(move |e| {
        let _ = tx.send(e);
    })
    .unwrap();
    let reg = listener.register(&[(42, combo)]);
    assert_eq!(reg, vec![(42, Ok(()))]);

    press_combo(combo.key.windows_vk(), Duration::from_millis(200));
    let pressed = rx.recv_timeout(Duration::from_secs(2)).expect("hotkey fired");
    assert_eq!(pressed, Event { id: 42, pressed: true });
    let released = rx.recv_timeout(Duration::from_secs(2)).expect("release detected");
    assert_eq!(released, Event { id: 42, pressed: false });

    // A second registration of the same keys (another program) is refused.
    let (tx2, _rx2) = mpsc::channel();
    let other = Listener::start(move |e| {
        let _ = tx2.send(e);
    })
    .unwrap();
    let reg = other.register(&[(1, combo)]);
    assert!(reg[0].1.as_ref().is_err_and(|e| e.contains("already used")), "{reg:?}");

    // After unregistering, the keys no longer fire.
    assert!(listener.register(&[]).is_empty());
    press_combo(combo.key.windows_vk(), Duration::from_millis(50));
    assert!(rx.recv_timeout(Duration::from_millis(500)).is_err());
}

#[test]
#[ignore = "injects key presses; needs an interactive desktop"]
fn capture_records_the_next_combination() {
    let recorder = std::thread::spawn(|| capture(Duration::from_secs(5)));
    // Give the hook time to install.
    std::thread::sleep(Duration::from_millis(300));
    press_combo(0x87, Duration::from_millis(30)); // F24
    let got = recorder.join().unwrap().unwrap();
    assert_eq!(got.map(|c| c.to_string()).as_deref(), Some("Ctrl+Alt+Shift+F24"));
}
