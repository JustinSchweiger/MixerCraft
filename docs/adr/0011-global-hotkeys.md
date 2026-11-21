# ADR 0011: Global hotkeys in the engine

Status: Accepted (Windows implemented; Linux and macOS planned)
Date: 2026-11-21

## Context

The owner wants a key press to mute the microphone strip or an output while
a game has the focus, with an optional confirmation tone, "like the keyboard
mapping in DMXCraft". DMXCraft maps keys and MIDI notes to controls through
one model (`Mapping { input: Key | Midi, control, mode: toggle/value, flash }`),
but its keys only work while its own window has the focus (egui input).
MixerCraft's GUI often runs hidden in the tray or not at all; the engine
(`mixercraftd`) is the process that always runs.

## Decision

1. **The engine owns the listener.** A new FFI crate `mixercraft-hotkey`
   (opts out of `unsafe_code`, documents its invariants) runs a thread with
   a Win32 message loop that registers each binding with `RegisterHotKey`
   (null window, `MOD_NOREPEAT`). `WM_HOTKEY` has no release event, so while
   a hotkey is held the thread polls `GetAsyncKeyState` every 15 ms and
   reports the release. Events go into the supervisor's existing request
   queue as `Request::TriggerHotkey`, so they are handled at once and exactly
   like a client request: mute/solo/gain changes use the normal path
   (`engine::Command`, ramped by `SmoothedGain`, persisted).
2. **Bindings** (config schema 5, `[hotkeys]`): `keys` as text
   (`"Ctrl+Alt+M"`, parsed by `mixercraft_hotkey::Combo`), `mode` `toggle` or
   `hold` (DMXCraft's "flash": switch while held, back on release, which is
   push-to-talk on a muted strip and push-to-mute on an open one), and an
   `action`: strip mute/solo/gain step, bus mute/gain step. Strips by
   position, buses by id; removing a strip or bus updates or drops the
   bindings. The `input` is a key combination only; a MIDI input can be
   added as another input kind later without changing actions.
3. **Typing keys need Ctrl, Alt or Win** (validated in the config):
   `RegisterHotKey` takes the key away from every application, so "M" or
   "Shift+M" alone would break typing. F13-F24, Pause, ScrollLock and numpad
   keys may be bound alone.
4. **Confirmation tone** in the real-time core (`chime.rs`): two 70 ms sine
   notes, rising for on/up and falling for off/down, at a configurable
   level, on one chosen bus. It is added after the bus fader and meter like
   the keep-awake signal, so it is heard even when the hotkey muted that bus.
   Allocation-free (`realtime_safety.rs`).
5. **Recording** a combination in the GUI uses a temporary low-level keyboard
   hook (`capture`), which reads virtual-key codes independent of the layout
   (on a German layout Ctrl+Alt+M arrives in the window as "µ"). The hook
   swallows the recorded key and exists only while recording (10 s at most).
   It runs in a short-lived copy of the GUI process (`--record-hotkey`): a
   hook in the process whose window has the focus did not receive the keys
   (observed on Windows 11).
6. IPC (protocol v1, additive): `SetHotkey`, `RemoveHotkey`,
   `SetHotkeyFeedback`, `TriggerHotkey`; `EngineStatus.hotkeys` (`None` from
   older engines) reports per binding whether it is registered or why not.

## Consequences

* Shortcuts work while the GUI is closed and while any application has the
  focus. A combination another program already registered is reported as
  inactive ("already used by another application or by Windows") instead of
  failing silently.
* `RegisterHotKey` consumes the keys: the focused game does not see them.
  That is wanted for mute keys and the reason for rule 3.
* Not verified: elevated (administrator) games and games with anti-cheat
  that filter input; an engine running as a service (no interactive
  desktop) cannot register hotkeys.
* Linux and macOS report `GlobalHotkeys` as Planned; bindings are stored and
  can be triggered over IPC/CLI there, but keys do nothing yet.

## Alternatives considered

* **Low-level keyboard hook in the engine for everything**: sees releases
  directly and could pass keys through, but every keystroke of the user
  passes through MixerCraft (keylogger-like, flagged by security software),
  hooks are skipped for elevated foreground windows, and Windows silently
  removes hooks that answer slowly. Rejected; used only briefly for
  recording.
* **Listener in the GUI**: does not work when the GUI is not running, and the
  GUI may only be in the tray. Rejected.
* **Slint key events for recording**: only while the window has the focus,
  and they deliver text, not keys (layout-dependent, AltGr). Rejected.
* **Global-hotkey crates** (`global-hotkey`, `livesplit-hotkey`): would add
  a dependency for roughly 200 lines of Win32 code we need to control
  (release polling, error reporting, thread ownership). Not needed now.
