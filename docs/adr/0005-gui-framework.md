# 0005 — GUI framework

**Status:** Accepted (2026-11-16) — Slint with the software renderer

## Context
The UI must look like professional audio software (dense channel strips,
precise faders, 60 Hz meters), stay light, support HiDPI, keyboard control
and screen readers on Windows, Linux and macOS, and talk to the engine over
IPC. It must not be chosen for how quickly it produces a flashy prototype.

Tauri was ruled out on paper (web view memory, meter traffic through a JS
bridge). Slint and egui were compared with a spike: the same mixer view
(24 input strips + 8 buses = 32 stereo meters at 60 Hz, custom faders,
mute/solo, dB readouts, accessibility metadata) built in both, measured by
`spikes/measure.ps1`. Code and raw results: `spikes/`.

## Measurements (Windows 11, 12 logical cores, window ≈ 1680 × 420)

Three runs each; CPU is the 20 s average in % of **one** core.

| | CPU animating | CPU silent | Private memory | Working set | First frame |
|---|---|---|---|---|---|
| **Slint, software renderer** | **7.0–8.4 %** | 0 % | **7.6–8.4 MB** | **26–28 MB** | not measurable¹ |
| Slint, OpenGL (femtovg) | 17.3–23.0 % | 0 % | 77 MB | 75–77 MB | 226–293 ms |
| egui/eframe (glow) | 10.9–13.7 %² | 0–0.7 % | 362–365 MB | 218–222 MB | 571–611 ms |

¹ The software renderer has no rendering notifier; the event loop was
running 11–13 ms after process start.
² A first, cold run measured 27.9 %.

Binary size (release, thin LTO): Slint 11.4 MB, egui 13.1 MB.

**Accessibility (Windows UI Automation, i.e. what Narrator sees):** both expose
32 named sliders and 56 named check boxes with their states. Slint exposes a
full numeric range (value 91, 0–100, step 1) when `accessible-value` is a
number; with a text value such as "91 %" it falls back to a text value only.
egui exposes the numeric range directly. Keyboard focus and arrow-key control
work in both prototypes.

**Visual quality:** comparable; see `spikes/results/*.png`. Slint's layout
filled the window and its gradient meters and text looked slightly crisper;
the egui prototype's fixed-width layout overflowed at 125 % scaling (a layout
detail of the prototype, not an egui limitation).

## Decision
Use **Slint** with the **software renderer** (`renderer-software`, partial
redraw) as the default for the desktop GUI.

* Lowest CPU while metering (about half of egui, a third of Slint/OpenGL) and
  an order of magnitude less memory than either GPU path — directly serving
  the "low resource usage" priority for an app that stays open all day.
* Zero CPU when nothing changes.
* Declarative `.slint` markup keeps visual design separate from logic and is
  easy to iterate on.
* Accessibility via AccessKit is complete enough for sliders and toggles.

## Consequences and follow-ups
* **License:** Slint is `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR
  LicenseRef-Slint-Software-3.0`. MixerCraft (MIT OR Apache-2.0) uses the
  royalty-free license, which requires Slint attribution (e.g. the
  `AboutSlint` widget or a "Made with Slint" notice in the About view).
  `deny.toml` must allow `LicenseRef-Slint-Royalty-free-2.0` when the GUI
  crate is added.
* **Large windows:** the software renderer's cost grows with the redrawn
  area. Before finalising, re-measure at 4K / full-screen; keep the GPU
  renderer selectable as a fallback.
* **Not yet measured on Linux and macOS** (no hardware); repeat the spike
  there when available.
* Meter values must be pushed as plain numbers via models; fader
  `accessible-value` must be numeric (with units in the label or
  description).
