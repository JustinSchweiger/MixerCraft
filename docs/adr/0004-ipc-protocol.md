# 0004 — Versioned NDJSON IPC over local sockets

**Status:** Accepted (2026-11-16)

## Context
The GUI must be able to start, close, crash and reconnect without affecting
audio, and GUI and engine may be updated separately.

## Decision
* Transport: `interprocess` local sockets — a named pipe on Windows, a Unix
  socket in the user's runtime directory elsewhere.
* Framing: one compact JSON object per line, at most 1 MiB.
* Handshake: `hello` carrying `PROTOCOL_VERSION`; mismatches are rejected
  explicitly. Within a version, changes are additive and unknown fields are
  ignored.
* The endpoint is per user, and binding it is the single-instance guard.

## Consequences
* Debuggable with standard tools and easy to implement in other languages.
* JSON cost is irrelevant at control rates (a few requests per second, meters
  at ~30 Hz). If meter traffic grows (many channels at 60 Hz), a shared-memory
  meter channel can be added without changing the control protocol.
* The Windows named pipe needs an explicit security descriptor before the
  engine runs as a service.
