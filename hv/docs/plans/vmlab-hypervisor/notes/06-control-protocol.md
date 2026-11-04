---
id: "06"
decision: D6
---

# Control protocol

## Question
QMP-compatible subset or a new typed protocol between vmlab and the hypervisor?

## Answer
A new typed protocol. vmlab replaces `Control::qmp()` with typed seam operations that both the QEMU adapter (translating to QMP) and the new hypervisor implement.

## Why
QMP compatibility would import QEMU's job model and event shapes into a clean-room design for about 19 commands. vmlab already has typed-vocabulary machinery to follow (ADR-0007). Recommended option, accepted.

## Consequences
- vmlab's seam must grow typed operations for snapshot save/load/delete, input, screen capture and device hot-plug before the new backend can offer them (Q18).
- The protocol needs a home both repos can depend on (Q17).
- The launch description is no longer an argv for the new backend; `LaunchSpec` (`mod.rs:137-155`) must carry something the new hypervisor understands (Q18).
