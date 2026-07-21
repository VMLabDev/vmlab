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
| guest agent | The in-guest vmlab-agent. `vm.is_ready()` / `vm.wait_ready()` test it; `vm.exec` / `copy_to` / `copy_from` run over it. | vmlab guest agent |
| vmlab-agent | vmlab's first-party in-guest agent on the `vmlab.agent.0` virtio-serial port (no guest network). Installed by the guest's own unattended-install hook from the auto-attached VMLAB bootstrap ISO at build time (meta `agent_version`); powers readiness, `vmlab shell`/`cp`/`tail`/`eventlog`/`ip`/`osinfo`, wscript `terminal()`/`stats()`, exec/copy, and graceful shutdown/reboot. Present in both full VMs and container micro-VMs. | agent channel |
| micro-VM | The tiny VM (pinned Alpine kernel + vmlab's purpose-built init) that each `container {}` runs inside, making an OCI container just another lab machine — same segments, DNS, snapshots and agent channel as full VMs (PRD §18). |  |
| playbook | A config-weave configuration folder bound to lab machines (or a template build) with a `playbook {}` block. Applied on `vmlab up` interleaved with provisions, re-run with `vmlab playbook check\|apply` or the web console's Playbook tab. | config-weave playbook |
| config-weave | The declarative guest-configuration system vmlab integrates for playbooks: plays converge package installs, files and services with drift detection, idempotent re-runs and automatic reboots. |  |
| web console | The browser UI served by `vmlab-web`: lab overview, visual designer, Files/Logs editors, per-machine consoles and terminals, template builds, playbooks, and proxied guest web pages. | vmlab-web, console UI |
| web page (guest) | An HTTP UI served inside a guest and declared with a `web {}` block; the web console proxies it into a same-origin iframe tab, injecting the guest app's login from the block's `auth {}`. |  |
| fast path | Optional eBPF network acceleration above the userspace switch: the afxdp tier (chosen by `auto`) and the explicit-only sockmap tier. Probed at daemon startup; any failure falls back to userspace. `vmlab fastpath` shows the active tier. | eBPF fast path |
| virtiofs | The shared-filesystem transport (vhost-user-fs) that `share {}` blocks and container volumes ride by default — no guest networking, snapshot-safe; SMB/CIFS is the fallback transport. |  |

[← Back to SKILL.md](../SKILL.md)
