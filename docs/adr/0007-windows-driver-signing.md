# 0007 — Windows virtual endpoints and driver signing

**Status:** Proposed — owner preference: zero recurring cost

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

## Sources
* Microsoft: https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs
* EV prices: https://www.ssl.com/products/software-integrity/code-signing/ev/ ,
  https://onesign.sg/blog/industry-news/code-signing-certificate-prices-2026
* Azure Artifact Signing: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options
* OSSign: https://github.com/ossign · usbip-win2: https://github.com/vadimgrn/usbip-win2
* Virtual Cables (USB/IP approach): https://alternativeto.net/software/virtual-cables/about/
