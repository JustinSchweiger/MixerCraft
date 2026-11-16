# 0005 — GUI framework

**Status:** Proposed — final choice after a spike in phase 7

## Context
The UI must look like professional audio software (dense channel strips,
precise faders, 60 Hz meters), stay light, support HiDPI, keyboard control
and screen readers on Windows, Linux and macOS, and talk to the engine over
IPC. It must not be chosen for how quickly it produces a flashy prototype.

## Options

| | Slint | egui | iced | Tauri |
|---|---|---|---|---|
| Model | retained, declarative | immediate mode | retained (Elm) | system web view |
| Custom widgets (faders, meters) | good (paths, custom properties) | very easy (painter) | good (canvas) | HTML/CSS/canvas |
| Cost with animated meters | redraws dirty regions | redraws whole UI each frame | redraws on messages | web view + JS bridge per frame |
| Typical memory | low (tens of MB) | low–moderate | low–moderate | 100 MB+ (WebView2/WebKit) |
| Accessibility | AccessKit on desktop | AccessKit | limited | browser accessibility |
| Visual polish | good typography, fully stylable | recognisably "egui" unless heavily styled | good | depends on CSS |
| License | GPLv3 / royalty-free with attribution / commercial | MIT/Apache | MIT | MIT/Apache |
| Backing | Slint GmbH (commercial) | very active community | community | Tauri foundation |

Memory figures are typical published values, not yet measured by us.

## Proposal
**Slint** as the primary candidate, **egui** as the fallback. Tauri is
rejected for the mixer UI (memory, meter traffic through the JS bridge). Its
updater is not a reason to choose it (see ADR 0006).

The spike must measure, on all three platforms: idle and active CPU with 32
stereo meters at 60 Hz, memory, cold start time, text rendering at 100/150/
200 % scaling, keyboard navigation and screen-reader output (Narrator, Orca,
VoiceOver). The results move this record to Accepted.

## Licensing note
Under Slint's royalty-free license MixerCraft must show Slint attribution
(e.g. in the About dialog). That is compatible with an MIT/Apache-licensed
application.
