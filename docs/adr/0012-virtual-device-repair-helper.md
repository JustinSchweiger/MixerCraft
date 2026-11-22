# ADR 0012: Repair helper for virtual devices after Fast Startup

Status: Accepted (Windows)
Date: 2026-11-22

## Context

Windows virtual devices are emulated USB audio devices attached through
usbip-win2 (ADR 0007). On the owner's PC, every "Shut down" with Fast
Startup on (the Windows default: sign-out plus hibernation of Windows
itself) left them broken after the next start: applications got
`AUDCLNT_E_DEVICE_INVALIDATED`, and MixerCraft could not bring them back.
Only a real restart helped. The owner keeps Fast Startup on and expects
MixerCraft to cope.

Findings on hardware (2026-11-17 to 2026-11-22, details in
`docs/roadmap.md`):

* After a Fast Startup boot, usbip-win2's port table is empty (`usbip
  port` lists nothing), but Windows still has the previous session's devices
  plugged into ports 1–3 of the virtual hub (present, arrival time of the
  previous session, no removal time).
* Anything attached to those ports is ignored, including a device Windows
  had never seen. With placeholders on ports 1–3, a test device on port 4
  came up at once. So the hub ports are blocked, not the serial numbers.
* Nothing a normal user may do removes the stale copies: reconnecting,
  `usbip detach`, `CM_Request_Device_Eject` (device then sits in problem 47)
  and `CM_Reenumerate_DevNode` on the hub (`CR_ACCESS_DENIED`).
* Unplugging the devices cleanly at sign-out (0.3.0: hidden window,
  `WM_ENDSESSION`, wait for removal) did not prevent it: the devices were
  disconnected 2 s into the shutdown, and Windows still saved them.
  Removal itself is fast in a normal session, even with an application
  playing into the device (110 ms).
* `pnputil /restart-device /deviceid ROOT\USBIP_WIN2\UDE` (restart of the
  virtual USB host controller, needs administrator rights) removed all
  three stale copies; the engine then reconnected the devices with their
  old identities (same serial numbers, ports, endpoint ids; Windows'
  per-app assignments and default devices kept).

## Decision

When the attach worker sees a virtual device blocked (its old copy stays
present for 3 s, or Windows does not detect it again after an attach), the
engine first unplugs its own connected devices and waits (up to 5 s) until
Windows has removed them, then starts a Task Scheduler task, **"MixerCraft
Repair Virtual Devices"**, which runs exactly that `pnputil` command as
SYSTEM, and waits for its result (the task's last result is pnputil's exit
code). Nothing is connected meanwhile. After success, failed attaches are
retried quietly every second for 30 s; automatic repairs are at most one
per 2 minutes. Exit code 3010 or 50 means Windows finishes the restart only
when it restarts: the engine then stops repairing and says so.

Unplugging first matters: a repair started while the devices were in use
(2026-11-22, test) was vetoed by the Windows audio engine
(`Kernel-PnP` 1000/225: `audiodg.exe`, veto type "outstanding open" on the
Browser device); Windows removed the devices but left the controller in
`DN_NEED_RESTART` until a restart, and pnputil, disable and enable all
answered "a system restart is pending". Unplugged devices are removed
without asking anyone (surprise removal, 110 ms with a client playing).

The task is set up once with administrator rights: the app's banner
("Repair") or `mixercraft repair-helper install`, which re-runs the CLI
elevated (`ShellExecuteExW` "runas", `mixercraft-winaudio::run_elevated`).
Its access list grants the user who set it up read and execute
(`FRFX`), so the engine starts it with `schtasks /Run` without asking
again. It has no trigger; it never runs by itself.

Without the helper, the device is reported as failed with a message that
says how to set it up, and the app shows the banner.

## Alternatives

* **Disable Fast Startup** (it works): rejected by the owner; a program
  should not need a system setting changed.
* **Placeholders on the blocked ports, devices on free ports** (no
  administrator rights): devices would get new identities after every
  shutdown (lost per-app assignments and defaults), and stale copies would
  pile up until a real restart.
* **A Windows service owning the USB/IP server** (devices would survive
  sign-out and hibernation like real hardware): large change (audio across
  processes, machine-wide install); may still be worth it later.
* **Fix in usbip-win2**: the right place long term (its controller should
  drop devices whose connection is gone after hibernation); to be reported
  upstream. MixerCraft cannot wait for it.

## Consequences

* One administrator prompt per machine and user; afterwards repairs are
  automatic. Removing the helper also needs administrator rights
  (`mixercraft repair-helper remove`); the per-user MSI cannot remove it,
  so uninstalling MixerCraft leaves the harmless task behind unless it is
  removed first.
* Any process running as that user can start the task. It only restarts
  usbip-win2's controller: at worst other USB/IP devices drop for a moment.
  The command is fixed in the task; nothing the user passes reaches it.
* A repair drops all virtual devices for a few seconds; applications
  reconnect as after any device change.
* The clean unplug at sign-out (0.3.0) stays: harmless, and it keeps the
  engine from reconnecting while Windows saves its state.
