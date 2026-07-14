# Vm

_api object_

A VM handle obtained from lab.vm(name): the entry point to lifecycle, snapshots, input, screen matching and the guest agent.

A `Vm` handle is returned by `lab.vm(name)` (or `lab.vms()`). Its methods fall into
five groups, each documented as its own reference:


- [Lifecycle & state](../references/fact_vm_lifecycle.md) — start/stop/restart, state, readiness, IPs.
- [Snapshots](../references/fact_vm_snapshots.md) — take, restore, list and delete snapshots.
- [Keyboard & mouse](../references/fact_vm_input.md) — send keys, type text, move/click/drag the mouse.
- [Screen, image matching & OCR](../references/fact_vm_vision.md) — screenshot, wait-for-image, OCR and text matching.
- [Guest agent](../references/fact_vm_agent.md) — exec commands, copy files in and out, interactive send/expect terminals, and guest stats.

Fallible calls return `Result[..., string]`; the matched screen hits return a [Match](../references/entity_match_type.md) and exec returns an [ExecResult](../references/entity_exec_result_type.md).

## Related

- [Lab](../references/entity_lab_api.md)

- [Vm: lifecycle & state methods](../references/fact_vm_lifecycle.md)

- [Vm: snapshot methods](../references/fact_vm_snapshots.md)

- [Vm: keyboard & mouse methods](../references/fact_vm_input.md)

- [Vm: screen, image matching & OCR methods](../references/fact_vm_vision.md)

- [Vm: guest agent methods (exec, files, terminal, stats)](../references/fact_vm_agent.md)

[← Back to SKILL.md](../SKILL.md)
