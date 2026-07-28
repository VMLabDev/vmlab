# Vm: lifecycle & state methods

| Method | Returns | Notes |
| --- | --- | --- |
| `vm.name()` | `string` |  |
| `vm.start()` / `vm.stop()` / `vm.stop_force()` / `vm.restart()` | `Result[unit, string]` | stop = graceful ladder (agent → ACPI → kill) |
| `vm.poweroff()` | `Result[unit, string]` | Clean QMP `quit` — exits QEMU flushing block caches. The only safe seal for guests with no ACPI (DOS, Win 3.x), where a force-kill can drop qcow2 writes |
| `vm.state()` | `string` | one of `"stopped"` / `"starting"` / `"running"` / `"stopping"` |
| `vm.is_ready()` | `bool` | The **sticky** ready flag — once set it stays set while QEMU runs, so it does not drop across a guest reboot |
| `vm.agent_answering()` | `bool` | The **live** agent probe, ungated — goes false while the guest is down or mid-reboot. Use this to watch a reboot you requested from inside the guest |
| `vm.wait_ready(timeout_secs: int)` | `Result[unit, string]` | Block until agent responds |
| `vm.wait_shutdown(timeout_secs: int)` | `Result[unit, string]` | Block until powered off |
| `vm.ip()` | `Result[string, string]` | Primary NIC IPv4 (DHCP lease / agent) |
| `vm.ip_nic(nic: int)` | `Result[string, string]` | By NIC index (0-based) |

Inside a machine's **own** first-boot script the readiness pair is special-cased:
`is_ready()` / `wait_ready()` mean "does the agent answer right now", because the
sticky flag is deliberately withheld until that script returns. Everywhere else
they report full readiness, and `agent_answering()` is how you get the live
signal.


## Related

- [Vm](../references/entity_vm_api.md)

- [Lab](../references/entity_lab_api.md)

[← Back to SKILL.md](../SKILL.md)
