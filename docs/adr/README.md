# Architecture Decision Records

Each record captures one significant decision: context, decision, consequences
and rejected alternatives. Accepted records are not rewritten; a change of
mind is a new record that supersedes the old one.

| # | Title | Status |
|---|---|---|
| [0001](0001-workspace-and-crate-boundaries.md) | Workspace and crate boundaries | Accepted |
| [0002](0002-clock-master-and-drift-compensation.md) | Clock master and drift-compensated FIFOs | Accepted |
| [0003](0003-cpal-for-physical-io.md) | cpal for physical device I/O (initially) | Accepted |
| [0004](0004-ipc-protocol.md) | Versioned NDJSON IPC over local sockets | Accepted |
| [0005](0005-gui-framework.md) | GUI framework: Slint, software renderer | Accepted |
| [0006](0006-distribution-and-updates.md) | Distribution and update architecture | Proposed |
| [0007](0007-windows-driver-signing.md) | Windows virtual endpoints and driver signing | Proposed — zero-cost path |
| [0008](0008-license.md) | Project license (MIT OR Apache-2.0) | Accepted |
| [0009](0009-linux-virtual-devices.md) | Linux virtual devices as PipeWire stream nodes | Accepted |
| [0010](0010-macos-virtual-devices.md) | macOS virtual devices: loopback Audio Server Plug-in | Proposed |

Template: copy an existing record, keep it short, link evidence.
