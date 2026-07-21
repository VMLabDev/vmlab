# vmlab-agent

_software_

vmlab's first-party in-guest agent on the vmlab.agent.0 virtio-serial port: terminals, exec, file transfer, tail, event logs, clipboard and metrics — no guest network involved.

`vmlab-agent` runs inside guests and talks to the host over a dedicated
`vmlab.agent.0` virtio-serial port, so everything it offers works on
air-gapped machines and never depends on guest networking. One channel
multiplexes: interactive **terminals** (a real PTY — root shell on Linux,
SYSTEM PowerShell via ConPTY on Windows), streaming **exec**, \*\*file
push/pull**, **tail** (`tail -F` semantics), the Windows **event log\*\*,
**clipboard** get/set, and subscribed **guest metrics**. Both full VMs and
container micro-VMs carry it; playbooks push over the same channel.


| Consumer | Surface |
| --- | --- |
| CLI | `vmlab exec` / `shell` / `cp` / `tail` / `eventlog`; `vmlab container exec` / `shell` (see the CLI reference) |
| wscript | `vm.exec` / `exec_timeout` / `copy_to` / `copy_from` / `terminal()` / `stats()` — [method tables](../references/fact_vm_agent.md); containers expose the same via the [Container handle](../references/entity_container_api.md) |
| Web console | Machine **Terminal** tabs, guest metrics meters, clipboard sync |

**How it gets into guests.** Template builds auto-attach a \*\*VMLAB bootstrap
ISO\*\* (agent binaries + install script) when the template's `agent` field is
true (the default); the guest's own unattended-install hook (cloud-init
runcmd, installer late-commands, autounattend first-logon) runs the script:
Linux gets a systemd unit (or OpenRC on Alpine), Windows a service under
`C:\ProgramData\vmlab`. The handshake is verified live on the channel before
the image is sealed, and only then is `agent_version` recorded in template meta —
the marker that unlocks agent-only features for clones. Container micro-VMs
get the agent injected by the init at boot, no baking needed. With `agent = false` or on
vintage guests (no agent channel), there is no exec/copy transport at all —
only screen-driven automation — and the VM never reports ready.
`vm.wait_ready()` means agent-level readiness.


## Related

- [Vm: guest agent methods (exec, files, terminal, stats)](../references/fact_vm_agent.md)

- [Container](../references/entity_container_api.md)

- [template {} block](../references/entity_template_block.md)

- [Templates](../references/concept_templates.md)

- [Playbooks (config-weave)](../references/concept_playbooks.md)

[← Back to SKILL.md](../SKILL.md)
