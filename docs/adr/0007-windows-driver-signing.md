# 0007 — Windows virtual endpoints and driver signing

**Status:** Proposed — owner preference: zero recurring cost. USB/IP route
spiked successfully on 2026-11-16 (see results below).

## Context
Windows has no user-mode API that creates playback/recording endpoints
visible to all applications; virtual endpoints need a kernel driver. Since
the April 2026 Windows update, Windows 11 loads only kernel drivers signed
through Microsoft (attestation or WHCP). Microsoft's documentation states
that a Partner Center hardware dashboard account needs **at least one EV
code-signing certificate** associated with it to submit drivers for
attestation signing. EV certificates are issued only to registered
organisations. From March 2027 submissions must include an SPDX SBOM.

## Cost of each route (researched 2026-11-16, list prices)

| Route | Recurring cost | Notes |
|---|---|---|
| Own EV certificate + Partner Center | ≈ $150–$800 per year | SSL.com lists $349/yr (1 year) down to $149/yr on a 5-year term; DigiCert/GlobalSign/Sectigo higher. Requires a registered business. Azure Artifact Signing ($9.99/month) does **not** issue EV certificates, so it cannot register a driver account. |
| Open-source signing sponsor (OSSign) | possibly $0 | "Open Source Codesigning Initiative" signs drivers of open-source projects via Microsoft attestation (e.g. usbip-win2). Eligibility and terms are not published; needs contact. Depends on a third party for every driver release. |
| Virtual USB audio device via USB/IP | $0 for us | Emulate a USB Audio Class 2 device in user space and attach it through the already-signed open-source usbip-win2 driver (BSD-2); Windows' inbox USB audio driver then creates normal endpoints. Precedent: the BSD-2 "Virtual Cables" project. Unverified for latency and robustness; adds a dependency on usbip-win2's signed releases. |
| No virtual endpoints on Windows | $0 | Physical mixing and routing, per-application capture (WASAPI process loopback) and per-application output redirection. Applications cannot *record from* a MixerCraft device (no "virtual microphone"). |
| Test signing | $0 | Development machines only. Never for users: requires disabling Secure Boot/test mode. |

Other platforms for comparison: Linux needs no signing. macOS needs the
Apple Developer Program ($99/year) for Developer ID signing and notarization
of the HAL plug-in package; the free alternative is building from source or
an ad-hoc-signed package that users must explicitly allow in Gatekeeper.
Windows application binaries (not drivers) can be Authenticode-signed for free
through SignPath Foundation's open-source programme; unsigned binaries only
trigger SmartScreen warnings.

## Proposal (zero-cost path)
1. Ship Windows without virtual endpoints first (row 4); the capability model
   already reports them as unavailable.
2. Spike the USB/IP virtual-device route (row 3): latency, clock behaviour,
   install/uninstall, sleep/resume. Adopt it if it meets the stability bar.
3. In parallel, ask OSSign whether MixerCraft qualifies (row 2) for a native
   WaveRT driver, which remains the technically best option.
4. Revisit buying an EV certificate only if both free routes fail and the
   owner decides the cost is acceptable.

Users are never asked to enable test signing or disable Secure Boot.

## USB/IP spike results (2026-11-16)

Prototype: `spikes/usbip-audio/` — a user-space USB/IP server (Rust, no
kernel code) emulating a full-speed USB Audio Class 1 device (48 kHz,
16-bit stereo, isochronous OUT "speaker" + IN "microphone", looped back like
a cable), attached through usbip-win2 0.9.8.1 (Microsoft-signed, hardware
certification report) on Windows 11 Pro 26300.

What works (verified on hardware):
* Windows' inbox `usbaudio.sys` enumerates it and creates two ordinary
  endpoints, "Lautsprecher (MixerCraft Virtual Cable)" and
  "Mikrofon (MixerCraft Virtual Cable)". Apps open them in shared mode at
  48 kHz with 10 ms periods.
* Audio is bit-transparent through the cable: 59 of 60 clicks came back
  (the first was played before recording started), level 0.74 for 0.8 sent
  (Windows' capture path), no loopback underruns.
* The emulator is the clock: it completes URBs in real time (10 packets of
  1 ms per URB). Completion lateness averaged 0.25–0.3 ms, at most 2.7 ms.
* **App-to-app latency 63.8–66.6 ms (mean 65.2 ms)**, very stable. Most of
  it is Windows queuing 10 ms URBs plus the two 10 ms shared-mode buffers;
  this is higher than VB-Audio's kernel cables (typically 10–30 ms), fine
  for streaming, recording and voice chat, too high for live monitoring.
* `usbip detach` removes the endpoints cleanly.

Problems found:
* **Windows made the new device the default output and default input on
  every attach.** Detaching restored the previous defaults. A product needs
  to restore the user's defaults after attaching (undocumented
  `IPolicyConfig`, as for per-application routing) or tell the user.
* usbip-win2 must be installed by the user (admin, restarts all USB 3 hubs,
  so other USB audio drops out briefly) and is a third-party kernel driver
  at version 0.9.x whose release notes warn of BSOD regressions in some
  builds. MixerCraft would depend on its signed releases.
* Not yet tested: several cables at once (one emulated device per cable,
  busids `1-1`, `1-2`, …), sleep/resume, reattach after reboot (usbip-win2
  has persistent reattachment), long soak, CPU cost, UAC2/high-speed for
  lower latency, and whether Windows still picks it as default after the
  first attach.

Assessment: the route is technically viable at zero cost for "virtual
cable" use cases with ~65 ms latency. Next: integrate the emulator into
`mixercraftd` (paced by the engine clock, one device per virtual strip/bus),
handle the default-device takeover, then soak-test.

## Sources
* Microsoft: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs
* EV prices: https://www.ssl.com/products/software-integrity/code-signing/ev/ ,
  https://onesign.sg/blog/industry-news/code-signing-certificate-prices-2026
* Azure Artifact Signing: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options
* OSSign: https://github.com/ossign · usbip-win2: https://github.com/vadimgrn/usbip-win2
* Virtual Cables (USB/IP approach): https://alternativeto.net/software/virtual-cables/about/
