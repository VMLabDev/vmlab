# vmlab

A single-host virtual machine lab orchestrator. Define **labs** — named
groups of VMs and virtual networks — declaratively in [WCL][wcl], build and
manage reusable **templates**, and drive automation through [wisp][wisp]
scripts that interact with guests at every level: power state, snapshots,
keystrokes and mouse input, screenshot capture with image matching and OCR,
and command execution and file transfer via the QEMU guest agent.

vmlab targets QEMU/KVM exclusively, driven directly over QMP — no libvirt.
Hosts are Linux, with **WSL2 supported as a first-class host environment**.

See [`docs/vmlab-prd.md`](docs/vmlab-prd.md) for the full product
requirements; it is the source of truth for design and scope.

## Architecture

Two-tier daemon system (PRD §3):

- **Supervisor (`vmlabd`)** — one per user, auto-started by the CLI. Owns lab
  lifecycle, the lab registry, global segments, template-store writes,
  host-level watchdogs, and an aggregated event stream.
- **Lab daemon** — one per running lab, owning that lab's QEMU processes,
  QMP/agent channels, network fabric (a complete userspace switching / DHCP /
  DNS / NAT / routing / filtering stack), snapshots, state, and the wisp
  runtime.

The CLI is a client of both tiers. wisp scripts are written against a clean
lab/VM API and are never aware of the daemons.

## Quick start

```sh
# A minimal lab: one Linux VM with internet egress.
cat > vmlab.wcl <<'EOF'
import <vmlab.wcl>
lab "demo" {
  vm "box" {
    template = "x86_64/linux-modern"
    memory   = "2G"
    nic { nat = true }
  }
}
EOF

vmlab validate     # full schema + semantic validation, no side effects
vmlab up           # create clones, boot, run provision scripts
vmlab status       # VM/segment state, IPs, ready flags
vmlab exec box -- uname -a
vmlab down         # graceful stop; clones retained
vmlab destroy      # stop + delete clones and lab-local state
```

## Examples

Worked examples under `examples/`, all built and run end-to-end:

- `templates/ubuntu-24.04/` — Ubuntu Server 24.04 template: ISO download +
  sha256 verify, cloud-init autoinstall via a CIDATA media block, OCR-driven
  confirmation in wisp.
- `templates/windows-server-2025/` — Windows Server 2025 (eval) template:
  fully unattended autounattend.xml install with virtio drivers, guest
  agent on first logon, boot-prompt handling in wisp.
- `mixed-lab/` — a two-VM Windows + Linux lab using both templates:
  static IP, boot ordering, SMB share onto `S:`, host port-forward, and a
  provision script driving both guests.
- `ad-lab/` — a larger Active Directory lab definition (config + scripts
  reference; templates for the client VM not included).

## CLI

| Verb | Action |
|---|---|
| `vmlab up [vm...]` | Create/start lab (or subset), run provision scripts |
| `vmlab down [vm...]` | Graceful stop; clones retained |
| `vmlab destroy` | Stop + delete clones, lab-local state, dynamic net config |
| `vmlab status` | Lab/VM/segment state, IPs, ready flags |
| `vmlab validate` | Full validation, no side effects |
| `vmlab start / stop / restart <vm>` | Per-VM power operations |
| `vmlab snapshot / restore / snapshots / snapshot-delete` | Snapshots |
| `vmlab console <vm>` | Attach a VNC viewer (TCP-forward fallback for WSL2) |
| `vmlab exec [--timeout s] <vm> -- cmd` | Guest-agent exec |
| `vmlab cp <src> <vm>:<dest>` | Copy a host file or directory tree into a guest |
| `vmlab osinfo <vm>` | Guest OS identification (guest-get-osinfo) as JSON |
| `vmlab run <script.wisp>` | Ad-hoc script against the current lab |
| `vmlab wispi` | Write the wisp interface file for LSP support |
| `vmlab logs [lab/][vm]` | Tail/dump JSON-line logs |
| `vmlab net rules / block / redirect / forward` | Inspect + mutate network rules |
| `vmlab template build / list / rm / export / import` | Template store |
| `vmlab template push / pull / login` | OCI registry distribution |
| `vmlab media build` | Folder → ISO/floppy |
| `vmlab daemon start / stop / status` | Supervisor control (normally automatic) |

## Building

This crate depends on the sibling [WCL][wcl] and [wisp][wisp] workspaces via
path dependencies (`../WCL`, `../wisp`). [`just`][just] is the command runner:

```sh
just build    # cargo build
just test     # cargo test
just check    # clippy (-D warnings) + fmt check + tests
```

Runtime tools expected on the host: `qemu-system-<arch>`, `qemu-img`,
`swtpm`, `tesseract`, an ISO tool (`xorriso`/`genisoimage`), `mtools` +
`mkfs.vfat` (floppy building), and `smbd` (shared folders). The official
container image (`Containerfile`) bundles them all.

## WSL2

vmlab is WSL2-clean by design (PRD §13): KVM requires nested virtualisation
enabled in `.wslconfig`; the userspace network fabric needs no tap/bridge/
macvlan and no privileges; host access from Windows rides port-forwards plus
WSL's localhost forwarding; `vmlab console --tcp` bridges the VNC display to a
localhost port for a Windows-side viewer; and `$XDG_RUNTIME_DIR` is created if
absent at daemon start.

## Container image

```sh
docker build -t vmlab -f Containerfile .
docker run --rm -it --device /dev/kvm \
  -v ~/.local/share/vmlab/templates:/root/.local/share/vmlab/templates \
  -v "$PWD":/lab -w /lab vmlab vmlab up
```

`--device /dev/kvm` is the only host grant required for acceleration; without
it vmlab falls back to TCG (slow but functional). No `--privileged`, no extra
capabilities, no host network mode.

[wcl]: https://github.com/wiltaylor/wcl
[wisp]: https://github.com/wiltaylor/wisp
[just]: https://github.com/casey/just
