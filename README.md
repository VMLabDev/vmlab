# vmlab

A single-host virtual machine lab orchestrator. Define **labs** — named
groups of VMs, containers and virtual networks — declaratively in [WCL][wcl],
build and manage reusable **templates**, and drive automation through
[wscript][wscript] scripts that interact with guests at every level: power
state, snapshots, keystrokes and mouse input, screenshot capture with image
matching and OCR, and command execution and file transfer over vmlab's own
in-guest agent.

vmlab targets QEMU/KVM exclusively, driven directly over QMP — no libvirt.
Hosts are Linux, with **WSL2 supported as a first-class host environment**.
Guests can be x86_64, aarch64 or riscv64, natively accelerated or emulated.

See [`docs/vmlab-prd.md`](docs/vmlab-prd.md) for the full product
requirements; it is the source of truth for design and scope.

## Install

```sh
curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre   # vmlab is pre-release only for now
```

This drops `vmlab` into `~/.local/bin`. Or build from source (see
[Building](#building)).

## Architecture

Two-tier daemon system (PRD §3):

- **Supervisor (`vmlabd`)** — one per user, auto-started by the CLI. Owns lab
  lifecycle, the lab registry, global segments, template-store writes,
  host-level watchdogs, and an aggregated event stream.
- **Lab daemon** — one per running lab, owning that lab's QEMU processes,
  QMP/agent channels, network fabric (a complete userspace switching / DHCP /
  DNS / NAT / routing / filtering stack), snapshots, state, and the wscript
  runtime.

The CLI is a client of both tiers. wscript scripts are written against a clean
lab/VM API and are never aware of the daemons.

There are exactly two doors into a running guest: **QMP** (power, devices,
screen) and **vmlab-agent** (exec, files, terminals, readiness) — see
[The guest agent](#the-guest-agent).

## Quick start

```sh
# A minimal lab: one Linux VM with internet egress, pulled from a registry.
cat > vmlab.wcl <<'EOF'
import <vmlab.wcl>
lab "demo" {
  vm "box" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    arch     = "x86_64"        # registry refs are multi-arch — pick one
    memory   = 2GiB
    nic { nat = true }
  }
}
EOF

vmlab validate     # full schema + semantic validation, no side effects
vmlab up           # pull the template, create clones, boot, run the setup steps
vmlab status       # machine/segment state, IPs, ready flags
vmlab exec box -- uname -a
vmlab down         # graceful stop; clones retained
vmlab destroy      # stop + delete clones and lab-local state
```

Templates can equally be built locally and referenced from the store as
`<arch>/<name>[@<version>]` — see `vmlab template build`.

## Machines

A lab holds two kinds of machine, on the same segments and with the same DNS,
snapshots and agent channel:

```wcl
vm "dc01" {                                    # a full VM from a disk template
  template = "x86_64/windows-server-2025"
  nic { segment = "corp" ip = "10.50.0.10" }
}

container "web" {                              # an OCI image, run in a micro-VM
  image = "nginx:1.27"                         # PRD §18
  nic { segment = "corp" }
  port { host = 18080 container = 80 }
}
```

Containers are not namespaces on the host: each runs in its own micro-VM
(pinned Alpine kernel + vmlab's own init), so a container has a real kernel
boundary and snapshots like a VM does.

## Setting guests up

Setup steps are declared **inside** the machine they configure and applied in
declaration order on `vmlab up`:

```wcl
vm "srv01" {
  template = "x86_64/windows-server-2025"
  nic { segment = "corp" }

  playbook "playbooks/domain" {                # declarative (config-weave)
    play = "member"
    var "domain" { value = "corp.example.com" }
  }
  provision "scripts/finish.ws" { }            # imperative (wscript)
}
```

- **Playbooks** are [config-weave][config-weave] plays: desired state, with a
  real drift check (`vmlab playbook check`) and reboot-aware apply loops.
- **Provisions** are wscript: sequenced, imperative work with the full typed
  guest API.
- **Event handlers** (`on "vm.crashed" { … }`) react to lifecycle events.

## The guest agent

`vmlab-agent` is vmlab's own in-guest agent, reachable on a dedicated
`vmlab.agent.0` virtio-serial port. It never touches the guest's network, so
exec, file transfer, interactive terminals, log tailing, metrics, clipboard and
readiness all work on air-gapped machines and before a guest is configured.

It is baked into templates at build time from an auto-attached VMLAB bootstrap
ISO, and verified live on the channel before the image is sealed. Container
micro-VMs get it injected at boot. Guests that cannot run it (vintage OSes,
`agent = false`) are still fully scriptable through the screen — keystrokes,
mouse, image matching and OCR — they just never report ready.

## Examples

Worked examples under `examples/`, all built and run end-to-end:

| Example | What it shows |
|---|---|
| `templates/` | Eight template definitions built from installer media: `ubuntu-24.04`, `ubuntu-26.04`, `fedora-44`, `almalinux-10`, `arch`, `opensuse-leap-16.0`, `opensuse-tumbleweed` and `windows-server-2025` (fully unattended `autounattend.xml` with virtio drivers) |
| `mixed-lab/` | Windows + Linux + an nginx container on one segment: static IP, `depends_on` ordering, an SMB share onto `S:`, a host port-forward, a provision script and a crash handler |
| `ad-lab/` | A larger Active Directory lab definition (config + scripts reference) |
| `alpine-registry/` | The no-build-step path: a template referenced by OCI ref and pulled on first `up` (PRD §6.4) |
| `winsrv-desktop/` | The smallest useful lab — console access and `gui = true` (PRD §11) |
| `alpine-arm64/` | An emulated aarch64 guest on an x86 host, NAT + SSH forward |
| `riscv64-ubuntu/` | An emulated riscv64 guest (needs `qemu-system-riscv64` ≥ 8.1 and riscv64 UEFI firmware) |
| `peer-a/` + `peer-b/` | Cross-instance L2 peering: a `global` segment with `connect {}` bridging two supervisors over a PSK-authenticated trunk (PRD §9.2) — `just peer-demo` |

## CLI

| Verb | Action |
|---|---|
| `vmlab up [machine...]` | Create/start the lab (or a subset), run playbooks and provisions |
| `vmlab down [machine...] [--force]` | Graceful stop (`--force` hard-kills); clones retained |
| `vmlab pull [machine...]` | Download missing registry templates/images without starting anything |
| `vmlab destroy` | Stop + delete clones, lab-local state, dynamic net config |
| `vmlab status` | Lab/machine/segment state, IPs, ready flags |
| `vmlab validate` | Full validation, no side effects |
| `vmlab vm start / stop / restart / destroy <vm>` | Per-VM power operations |
| `vmlab vm screenshot / sendkeys / mouse-move / click / drag <vm>` | Screen capture and input injection |
| `vmlab vm ocr / find-image <vm>` | Read text off the screen; locate a template image |
| `vmlab container start / stop / restart / destroy <c>` | Per-container lifecycle |
| `vmlab container exec / shell / logs / ip <c>` | Container interaction |
| `vmlab lab list / info / stop / destroy` | Manage running labs host-wide |
| `vmlab snapshot create / restore / list / delete` | Lab-wide by default; `--vm` narrows to one machine |
| `vmlab playbook list / check / apply` | config-weave playbooks; `check` reports drift without changing anything |
| `vmlab console <vm>` | Attach a VNC viewer (`--tcp` forward for WSL2) |
| `vmlab exec [--timeout s] <vm> -- cmd` | Run a command through the guest agent |
| `vmlab shell <vm>` | Interactive root/SYSTEM shell over virtio-serial (Ctrl-] detaches) |
| `vmlab cp <src> <dst>` | Copy files host↔guest — either side may be `<vm>:<path>` |
| `vmlab tail <vm> <path>` | `tail -F` a guest file over the agent |
| `vmlab eventlog <vm> [--filter XPATH]` | Follow a Windows guest's event log |
| `vmlab osinfo <vm>` | Guest OS identification as JSON |
| `vmlab script <script.ws>` | Ad-hoc wscript against the current lab |
| `vmlab logs [lab/][vm] [-f] [-o jsonl]` | Tail/dump logs (pretty by default) |
| `vmlab fastpath` | Which network fast-path tier is active, and why |
| `vmlab template build / list / rm / clean / export / import` | Template store |
| `vmlab template push / pull / search / login` | OCI registry distribution |
| `vmlab template registry list / add / remove` | Shared registry namespaces |

The supervisor starts on demand; `vmlab daemon start / stop / status` exists as
a hidden escape hatch.

## Building

[`just`][just] is the command runner. [WCL][wcl] and [wscript][wscript] are git
dependencies pinned by rev in `Cargo.toml`, so no sibling checkouts are needed:

```sh
just build     # cargo build
just test      # cargo test
just ci::check # the merge bar: everything a change must pass before it can merge
just ci        # list the gate's parts — run one on its own with `just ci::lint`
```

`just ci::check` is self-sufficient from a clean checkout and reports a missing
tool as a missing tool. It covers clippy, `cargo fmt --check`, the test suite,
the standalone guest crates, and the committed BPF objects — the last of which
needs a one-time `just ebpf-tools`.

Runtime tools expected on the host: `qemu-system-<arch>`, `qemu-img`, `swtpm`,
`tesseract` (OCR), an ISO tool (`xorriso`/`genisoimage`), `mtools` +
`mkfs.vfat` (floppy building), `sqfstar` from `squashfs-tools` (required for
containers), and a VNC viewer (`remote-viewer` preferred) for `vmlab console`.
Shared folders use `virtiofsd` when host and guest both support it and fall
back to a bundled unprivileged `smbd`, so having both covers every guest.

## Networking

Each lab gets a complete userspace network fabric — L2 switching, DHCP, DNS,
NAT, routes, port forwards and L3 filtering — with no taps, bridges, or
`CAP_NET_ADMIN`. An optional eBPF fast path (`VMLAB_FASTPATH=auto`, AF_XDP
tier) accelerates it where the host allows; `vmlab fastpath` reports the
selected tier, and an unavailable fast path silently falls back to userspace.

## WSL2

vmlab is WSL2-clean by design (PRD §13): KVM requires nested virtualisation
enabled in `.wslconfig`; the userspace network fabric needs no tap/bridge/
macvlan and no privileges; host access from Windows rides port-forwards plus
WSL's localhost forwarding; `vmlab console --tcp` bridges the VNC display to a
localhost port for a Windows-side viewer; and `$XDG_RUNTIME_DIR` is created if
absent at daemon start.

[wcl]: https://github.com/wiltaylor/wcl
[wscript]: https://github.com/Configweave/wscript
[config-weave]: https://github.com/Configweave/config-weave
[just]: https://github.com/casey/just
