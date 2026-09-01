# Machine: snapshot methods

| Method | Returns |
| --- | --- |
| `m.snapshot(name: string)` | `Result[unit, string]` — online or offline per current state |
| `m.restore(name: string)` | `Result[unit, string]` — resumes running iff taken online |
| `m.snapshots()` | `Result[List[string], string]` |
| `m.delete_snapshot(name: string)` | `Result[unit, string]` |

Share contents are outside snapshot scope.

## Related

- [Machine](../references/entity_vm_api.md)

[← Back to SKILL.md](../SKILL.md)
