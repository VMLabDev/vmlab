# ExecResult

_data type_

The result of a guest command run via vm.exec: exit_code, stdout, stderr.

Returned by the guest-agent exec methods (`vm.exec`, `vm.exec_timeout`).

```rust
struct ExecResult { exit_code: int, stdout: string, stderr: string }
```

## Related

- [Vm](../references/entity_vm_api.md)

- [Vm: guest agent methods](../references/fact_vm_agent.md)

[← Back to SKILL.md](../SKILL.md)
