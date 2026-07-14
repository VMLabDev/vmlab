# Container

_api object_

A container handle obtained from lab.container(name): lifecycle, snapshots, exec, file copy, logs, health, interactive terminal and stats — the VM API minus display/input.

A `Container` handle is returned by `lab.container(name)` (or `lab.containers()`).
It mirrors the `Vm` handle's lifecycle, snapshot and guest-agent surface; there
are no input or screen methods (containers have no display).


| Method | Meaning |
| --- | --- |
| `name()` | The container's lab name |
| `start()` / `stop()` / `stop_force()` / `restart()` | Lifecycle (stop is the graceful ladder: stop signal → guest shutdown → kill) |
| `state()` | `stopped` / `starting` / `running` / `stopping` |
| `is_ready()` / `wait_ready(secs)` | Process started + healthcheck passing (when declared) |
| `is_healthy()` | Latest healthcheck verdict (a container without one counts healthy once ready) |
| `wait_shutdown(secs)` | Wait until stopped |
| `ip()` / `ip_nic(i)` | The DHCP lease (errors cleanly on an air-gapped container) |
| `snapshot(name)` / `snapshots()` / `delete_snapshot(name)` | Snapshots, same semantics as VMs (offline + online full-parity) |
| `exec(cmd, args)` / `exec_timeout(cmd, args, secs)` | Run a command inside the container; returns an [ExecResult](../references/entity_exec_result_type.md) |
| `copy_to(local, path)` / `copy_from(path, local)` | File copy in/out of the container filesystem |
| `logs(lines)` | Tail of the container's stdout/stderr (the serial console log) |
| `terminal()` | Interactive send/expect shell inside the container's PID namespace (the workload is PID 1) — a [Term](../references/fact_vm_agent.md) handle |
| `stats()` | Live `GuestStats` (cpu/mem/disks), as on [Vm](../references/fact_vm_agent.md) |

```wscript
use vmlab

fn main(lab: Lab) {
    let web = lab.container("web").unwrap()
    web.wait_ready(120).unwrap()
    let r = web.exec("nginx", ["-t"]).unwrap()
    lab.log("config check: " + r.stderr)
}
```

## Related

- [Lab](../references/entity_lab_api.md)

- [Vm](../references/entity_vm_api.md)

- [container {} block](../references/entity_container_block.md)

- [ExecResult](../references/entity_exec_result_type.md)

- [Vm: guest agent methods (exec, files, terminal, stats)](../references/fact_vm_agent.md)

[← Back to SKILL.md](../SKILL.md)
