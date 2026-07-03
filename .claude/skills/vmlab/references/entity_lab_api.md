# Lab

_api object_

The lab handle passed to fn main(lab: Lab) / fn handle(event, lab) — find VMs and segments, log.

| Method | Returns | Notes |
| --- | --- | --- |
| `lab.name()` | `string` | Lab name from vmlab.wcl |
| `lab.log(msg: string)` | `unit` | Lab log + live CLI stream |
| `lab.vm(name: string)` | `Result[Vm, string]` | Err if not defined |
| `lab.vms()` | `List[Vm]` | All VMs |
| `lab.segment(name: string)` | `Result[Segment, string]` | Err if not declared |

## Free functions

| Function | Notes |
| --- | --- |
| `vmlab::sleep_ms(ms: int)` | Sleep; call module-qualified (or `use vmlab::sleep_ms`). Prefer `wait_*` methods over fixed sleeps. |

## Related

- [Vm](../references/entity_vm_api.md)

- [Segment](../references/entity_seg_api.md)

- [wscript: pattern matching & errors](../references/concept_wscript_matching.md)

- [Provisions & event handlers](../references/concept_provisions.md)

[← Back to SKILL.md](../SKILL.md)
