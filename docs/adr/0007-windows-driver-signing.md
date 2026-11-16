# 0007 — Windows virtual driver signing

**Status:** Proposed — needs a decision by the project owner

## Context
Virtual endpoints on Windows require a kernel-mode driver. Since the April
2026 Windows update, Windows 11 trusts only kernel drivers signed through the
Windows Hardware Compatibility Program (attestation or HLK). Attestation
signing requires a Microsoft Partner Center hardware developer account, which
requires an EV code-signing certificate issued to a registered organisation.
From March 2027, submissions must include an SPDX SBOM.

## Options
1. **The owner obtains an EV certificate and a Partner Center account.** CI
   produces the driver package; a maintainer submits it for attestation
   signing (manually, or via the submission API from the protected release
   environment).
2. **Ship without our own driver on Windows:** physical mixing, process
   loopback capture and per-app output redirection only, with virtual
   endpoints documented as unavailable.
3. **Depend on a third-party signed virtual cable** (e.g. VB-Cable). This
   conflicts with the goal of not requiring third-party components.

## Proposal
Develop the driver with test signing on dedicated machines (the path to
option 1) while shipping option 2 until signing is in place. Never ask users
to enable test signing or to disable Secure Boot.

## Open question for the owner
Who will hold the EV certificate and the Partner Center account, and is the
annual cost acceptable?
