# vmlab — glossary

| Term | Definition | Aliases |
| --- | --- | --- |
| lab | A set of VMs plus the virtual networks connecting them, declared in a `lab {}` block in `vmlab.wcl`. |  |
| segment | A virtual layer-2 switch. The lab daemon supplies DHCP, DNS, NAT, routing and L3 filtering for it in userspace. | network segment |
| template | A sealed, read-only qcow2 disk image in the store, referenced by `<arch>/<name>[@<version>]`. Labs boot linked clones of it. |  |
| linked clone | A copy-on-write qcow2 overlay a lab VM boots, backed by a template. The template is never written to. | clone |
| store | The local template store at `~/.local/share/vmlab/templates/`. Writes are serialised by the supervisor. | template store |
| provision | A wscript script run on `vmlab up` (and during template builds) to set a guest up. A failure fails `vmlab up`. | provision script |
| event handler | A wscript script bound with `on "event" {}` that reacts to a lifecycle event via `fn handle(event, lab)`. Failures are logged, never fatal. | handler |
| supervisor | The per-user daemon `vmlabd`, auto-started by the CLI. Owns the lab registry, global segments, store writes and host watchdogs. | vmlabd |
| lab daemon | The per-lab daemon spawned by the supervisor on `vmlab up`. Owns QEMU, the network fabric, snapshots and the wscript runtime. |  |
| profile | A named set of hardware defaults (machine, firmware, TPM, disk bus, NIC, display, CPUs/memory) chosen with `profile = "..."`. | guest OS profile |
| scoped provision | A `provision "x" { vms = [...] }`: it runs against those VMs and gates `depends_on` on them, so dependents wait for it. |  |
| scratch VM | A VM booted from a blank disk (`template = "scratch"`) with no template, requiring explicit `arch`, `profile` and `disk`. |  |
| OCI artifact | How a template is stored in a registry: a non-runnable artifact (frozen media type) whose qcow2 is chunked into zstd layers. |  |
| wscript | vmlab's statically typed, Rust-flavoured scripting language for guest automation. Compiled and type-checked at `vmlab validate` time. |  |
| guest agent | The QEMU guest agent (QGA) running inside a VM. `vm.is_ready()` / `vm.wait_ready()` test it; `vm.exec` / `copy_to` / `copy_from` fall back to it when the richer vmlab-agent is absent. | QEMU guest agent, QGA |
| vmlab-agent | vmlab's first-party in-guest agent on the `vmlab.agent.0` virtio-serial port (no guest network). Baked into templates at build time (meta `agent_version`); powers `vmlab shell`/`cp`/`tail`/`eventlog`, wscript `terminal()`/`stats()`, and agent-first exec/copy with QGA fallback. Present in both full VMs and container micro-VMs. | agent channel |

[← Back to SKILL.md](../SKILL.md)
