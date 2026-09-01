# Machine

_api object_

The machine handle obtained from lab.machine/vm/container: the entry point to lifecycle, snapshots, input, screen matching and the guest agent, for a VM or a container alike.

A `Machine` handle is returned by `lab.machine(name)`, `lab.vm(name)`,
`lab.container(name)` and their plural forms. There is one handle for both
kinds, and every method below is available on all of them. Its methods fall
into five groups, each documented as its own reference:


- [Lifecycle & state](../references/fact_vm_lifecycle.md) — start/stop/restart, state, readiness, IPs.
- [Snapshots](../references/fact_vm_snapshots.md) — take, restore, list and delete snapshots.
- [Keyboard & mouse](../references/fact_vm_input.md) — send keys, type text, move/click/drag the mouse.
- [Screen, image matching & OCR](../references/fact_vm_vision.md) — screenshot, wait-for-image, OCR and text matching.
- [Guest agent](../references/fact_vm_agent.md) — exec commands, copy files in and out, interactive send/expect terminals, and guest stats.

**What a machine cannot do is reported at call time and names the capability.**
`screenshot` on a machine with no display fails with \*"machine `api` has no
display"\* — not "no such method", and not a claim that its kind could never
have one. That is deliberate: the day a container reports a display, the same
script works unchanged.


Fallible calls return `Result[..., string]`; the matched screen hits return a [Match](../references/entity_match_type.md) and exec returns an [ExecResult](../references/entity_exec_result_type.md).

## Related

- [Lab](../references/entity_lab_api.md)

- [Machine: lifecycle & state methods](../references/fact_vm_lifecycle.md)

- [Machine: snapshot methods](../references/fact_vm_snapshots.md)

- [Machine: keyboard & mouse methods](../references/fact_vm_input.md)

- [Machine: screen, image matching & OCR methods](../references/fact_vm_vision.md)

- [Machine: guest agent methods (exec, files, terminal, stats)](../references/fact_vm_agent.md)

[← Back to SKILL.md](../SKILL.md)
