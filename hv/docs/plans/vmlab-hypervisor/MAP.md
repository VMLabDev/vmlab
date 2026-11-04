---
slug: vmlab-hypervisor
status: specified
created: 2026-10-30
---

# vmlab-hypervisor: a clean-room Rust VMM to replace QEMU under vmlab

## Destination
`vmlab-hypervisor` is a clean-room, MIT-licensed Rust VMM in its own repo that
vmlab selects per machine as a backend in place of QEMU. When finished it runs
every guest vmlab supports today on x86_64 Linux hosts — x86 guests under KVM
(modern and the legacy PC profiles: DOS, 9x, XP, TempleOS) and aarch64 and
riscv64 guests through its own TCG — with a control plane, snapshot format and
frame path designed around vmlab rather than QEMU's QMP, vmstate and RFB. QEMU
stays vmlab's default backend until the owner is satisfied with the new
hypervisor, and is removed after that.

## Grounding
Full reports: `docs/research/qemu-viability/` (why O3) and `docs/research/grounding/`
(`g1-seam.md`, `g2-constraints.md`). vmlab paths below are relative to `/home/wil/dev/vmlab`.
- vmlab already has a host seam, `src/labd/hypervisor/` (`Process`, `Control`, `Hypervisor` traits, `mod.rs:50-212`), with a QEMU adapter and a test fake that drives 38 lifecycle tests (`fake.rs`, `lifecycle_tests.rs`). ADR-0001 defines it; CONTEXT.md names it "Hypervisor" and says avoid "backend/driver/runtime" (`CONTEXT.md:363-372`).
- The backend is hard-wired: `Arc::new(Qemu)` at `labd/vm.rs:404` and `labd/container.rs:405`. The field is per machine, so per-machine selection is one constructor parameter away.
- The seam leaks QEMU: `LaunchSpec` is an argv (`mod.rs:137-155`), `Control::qmp()` hands the concrete `QmpClient` upward (`mod.rs:116`) for online snapshots (`vm.rs:1361-1447`), USB `device_add` (`vm.rs:1837-1862`) and screendump/input (`display/mod.rs:120-274`); `classify_exit` string-matches `"exit status: 0"` (`mod.rs:242`).
- RFB on `vnc.sock` carries the recorder (`record/executor.rs:69`) and GL-display capture, which has no QMP alternative (`vnc.rs:16-20`).
- qcow2 and `qemu-img` sit under templates, linked clones, internal snapshots and freeze chains, all above the seam (`template/qimg.rs:93-207`, `freeze/steps.rs:174-181`, `supervisor/backing.rs`); ADR-0017 and ADR-0019 fix qcow2 overlays and internal snapshots into the design.
- The network attachment is a unix stream socket with 4-byte big-endian length-prefixed Ethernet frames (`src/net/framing.rs:1-14`); the PRD calls it a substitution seam (`docs/vmlab-prd.md:600`).
- Agent and container control ride named virtio-serial ports `vmlab.agent.0` / `vmlab.ctl.0`; legacy guests use a 16550 on COM1 (`docs/vmlab-prd.md:489-498`). ADR-0013: the host opens channels, the guest only answers.
- The PRD lists non-QEMU hypervisors as a non-goal (`docs/vmlab-prd.md:31`), says vmlab is not a security boundary and has no sandboxing (`:29`), and sets no performance targets (`:27`).
- Hosts are Linux with WSL2 first-class (`docs/vmlab-prd.md:15, 896-898`), so nothing may require tap, bridges or CAP_NET_ADMIN by default.
- ADRs binding a backend: 0001, 0002, 0003, 0008, 0009, 0013, 0017, 0018, 0019.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | New clean-room MIT Rust hypervisor in its own repo, not a QEMU port | | `notes/01-clean-room-rewrite.md` |
| D2 | QEMU stays vmlab's default until the owner is satisfied, then is removed | | `notes/02-qemu-coexistence.md` |
| D3 | Hosts are x86_64 Linux only for now; no x86 TCG | | `notes/03-no-arm-hosts.md` |
| D4 | Phase order: containers, modern x86 VMs, TCG guests, legacy PC | | `notes/04-phase-order.md` |
| D5 | One hypervisor process per machine, spawned by vmlab | | `notes/05-process-model.md` |
| D6 | New typed control protocol; vmlab replaces `Control::qmp()` with typed seam operations | | `notes/06-control-protocol.md` |
| D7 | Build on rust-vmm crates; lift BSD crosvm pieces with attribution | | `notes/07-foundation.md` |
| D8 | qcow2 for now behind a disk-format seam; own format later | | `notes/08-disk-format.md` |
| D9 | NICs speak vmlab's length-prefixed stream socket; tap-fd later | | `notes/09-network.md` |
| D10 | Agent and ctl stay on virtio-serial named ports; legacy on COM1 | | `notes/10-agent-channel.md` |
| D11 | Per-machine hypervisor field with a host-config default | | `notes/11-backend-selection.md` |
| D12 | No sandboxing now; nothing may block per-process seccomp/Landlock later | | `notes/12-sandboxing.md` |
| D13 | Parity gate: e2e suite per hypervisor plus a differential suite vs QEMU | | `notes/13-parity-gate.md` |
| D14 | Snapshots are a disk-format-seam operation; qcow2 internal snapshots plus our own versioned device state | | `notes/15-snapshot-format.md` |
| D15 | A linked disk crate replaces qemu-img in vmlab once at parity | | `notes/16-image-tooling.md` |
| D16 | `qemu_args` on the new hypervisor is a validation error | | `notes/17-qemu-args.md` |
| D17 | swtpm stays external, same contract, from phase 2 | | `notes/18-swtpm.md` |
| D18 | Own repo `vmlab-hypervisor` as a cargo workspace; vmlab depends via git; may merge later | | `notes/19-repo-shape.md` |
| D19 | vmlab seam gets capability traits and a typed launch description, before phase 1 | | `notes/20-seam-refactor.md` |
| D20 | PRD §1.2 amended with an ADR when the selector lands | | `notes/21-prd-amendment.md` |
| D21 | Native shared-memory frame protocol primary; small RFB server for viewers | | `notes/22-display.md` |
| D22 | TCG: Sail-derived interpreter (riscv64, then aarch64), lockstep harness, then Cranelift JIT | | `notes/23-tcg.md` |
| D23 | Reuse SeaBIOS and OVMF unchanged; implement their platform interfaces | | `notes/24-firmware.md` |
| D24 | GPU: crosvm's virtio-gpu with rutabaga over virglrenderer | | `notes/25-gpu.md` |
| D25 | USB (crosvm xhci/usb-host) and vfio at the end of phase 2 | | `notes/26-usb-vfio.md` |
| D26 | Tiered differential suite: hardware dumps and kvm-unit-tests, lockstep for TCG, screenshots for legacy | | `notes/27-differential-suite.md` |
| D27 | Field `hypervisor = "qemu" | "vmlab"`, binary `vmlab-hv` | | `notes/28-naming.md` |
| D28 | No-regression performance bars against QEMU before a default flips (TCG figure raised to 100% by phase-3 D1) | | `notes/29-performance.md` |
| D29 | Cutover per phase; QEMU removed after phase 4 | | `notes/30-cutover.md` |
| D30 | QEMU-identical guest-visible hardware by default; breaks only with owner approval | | `notes/31-template-compat.md` |
| D31 | On a hypervisor switch, offline snapshots carry over; online ones refuse restore under the other hypervisor | | `notes/33-snapshot-switch.md` |
| D32 | Online snapshots restore within the same device-state major version; best effort before 1.0 | | `notes/34-device-state-versions.md` |
| D33 | File sharing: external virtiofsd in phase 1, own in-process virtio-fs device in phase 2; Windows virtiofs after a stable ≥mm325 soak; SMB stays | | `notes/35-file-sharing.md` |
| D34 | TCG sources: sail-riscv and sail-arm through our own Rust emitter; no ASL extraction | | `notes/36-tcg-sources.md` |

## Frontier
Ids Q1 and Q2 were asked before this map existed; Q1 (ARM hosts) settled D3.
Session boundary offered 2026-10-30 (split architecture from per-subsystem questions); declined, map in one session.
- [x] ~~Q3 (decision) — Phase order: which slice does the new hypervisor reach parity on first?~~ → D4
- [x] ~~Q4 (decision) — Process model: is the hypervisor a separate process per machine that vmlab spawns, or a library linked into the lab daemon?~~ → D5
- [x] ~~Q5 (decision) — Control protocol between vmlab and the hypervisor: a QMP-compatible subset, or a new typed protocol?~~ → D6
- [x] ~~Q6 (decision) — Foundation: build on rust-vmm crates, or fork crosvm or cloud-hypervisor?~~ → D7
- [x] ~~Q7 (decision) — Disk format: does the hypervisor keep qcow2 with backing chains as its disk format?~~ → D8
- [x] ~~Q8 (decision) [blocked_by: ~~Q7 (D8)~~] — Snapshot format: where do RAM and device state live, and do qcow2 internal snapshots stay?~~ → D14
- [x] ~~Q9 (decision) [blocked_by: ~~Q7 (D8)~~] — Image tooling: what replaces `qemu-img` for templates and freeze once QEMU is removed?~~ → D15
- [x] ~~Q10 (decision) — Network attachment: speak vmlab's existing length-prefixed stream socket, or something new?~~ → D9
- [x] ~~Q11 (decision) — Agent channel: keep virtio-serial named ports so the existing agent and cinit run unchanged?~~ → D10
- [x] ~~Q12 (decision) — Backend selection in vmlab: where does a lab choose the hypervisor, and at what granularity?~~ → D11
- [x] ~~Q13 (decision) [blocked_by: ~~Q12 (D11)~~] — `qemu_args`: what happens to a machine that sets it on the new backend?~~ → D16
- [x] ~~Q14 (decision) — Sandboxing: keep "not a security boundary", or confine each machine?~~ → D12
- [x] ~~Q15 (decision) — Parity gate: what proves a phase is done and lets vmlab move a profile over?~~ → D13
- [x] ~~Q16 (decision) — swtpm: keep it as an external process? (virtiofsd split out to Q25)~~ → D17
- [x] ~~Q17 (decision) [blocked_by: ~~Q4 (D5)~~, ~~Q5 (D6)~~] — Repo shape: crate layout, and where the shared protocol crate lives?~~ → D18
- [x] ~~Q18 (decision) [blocked_by: ~~Q5 (D6)~~] — vmlab seam refactor: widen `Control`, add capability traits, or accept degraded features until parity?~~ → D19
- [x] ~~Q19 (decision) [blocked_by: ~~Q12 (D11)~~] — PRD: when and how is the §1.2 non-goal amended?~~ → D20
- [x] ~~Q20 (decision) [blocked_by: ~~Q3 (D4)~~] — Display: an RFB-subset server on `vnc.sock`, or a native frame protocol?~~ → D21
- [x] ~~Q21 (decision) [blocked_by: ~~Q3 (D4)~~] — TCG approach: Sail-derived interpreter first, JIT substrate, and the differential harness.~~ → D22
- [x] ~~Q22 (decision) [blocked_by: ~~Q3 (D4)~~] — Legacy PC platform and firmware: which chipset, devices and firmware (SeaBIOS, OVMF) to reuse unchanged?~~ → D23
- [x] ~~Q23 (decision) [blocked_by: ~~Q3 (D4)~~] — GPU: virglrenderer bindings, rutabaga, or crosvm's GPU stack?~~ → D24
- [x] ~~Q24 (decision) [blocked_by: ~~Q3 (D4)~~] — USB and vfio passthrough: when and on what?~~ → D25
- [x] ~~Q25 (decision) [blocked_by: ~~Q26 (notes/14)~~] — File sharing, Windows especially: what transport does the new hypervisor offer, and does virtiofsd stay?~~ → D33
- [x] Q26 (research) [status: answered] — What are the options for fast, reliable host-to-guest file sharing into Windows guests when we control the hypervisor?
- [x] ~~Q27 (decision) — Differential suite: what is compared against QEMU, at what granularity, per phase?~~ → D26
- [x] ~~Q28 (decision) — Naming: what is the lab-file field and value that select the new hypervisor, and what is the project called in vmlab's vocabulary?~~ → D27
- [x] ~~Q29 (decision) — Performance: what bar, if any, does each phase have to meet beyond the parity gate?~~ → D28
- [x] ~~Q31 (decision) — Template compatibility: must templates built under QEMU boot unchanged on the new hypervisor?~~ → D30
- [x] ~~Q30 (decision) — Cutover: when a phase passes its gate, does vmlab flip the default for that phase's profiles, or wait and flip everything at once?~~ → D29
- [x] Q32 (research) [status: answered] — Do the licences of the Arm and RISC-V Sail models (and Arm's ASL spec) allow generating emulator code into an MIT project?
- [x] ~~Q33 (decision) — Existing snapshots: when a machine switches hypervisor, what happens to its online snapshots taken under the other one?~~ → D31
- [x] ~~Q34 (decision) — Device-state compatibility: which older hypervisor versions' online snapshots must a newer `vmlab-hv` restore?~~ → D32

- [x] ~~Q35 (decision) — TCG semantics sources and generator: which spec feeds each frontend, and how is Rust generated from it?~~ → D34

## Fog

## Accepted unknowns
- Licence policy for code lifted into the tree (D7): keep each lifted file's original notice; the implementer writes the policy before the first lift
- How validation words a refusal when a machine needs something the new hypervisor lacks (D11): any message naming the missing capability satisfies it
- Matching QEMU's `server=on,wait=off` socket semantics for agent and ctl (D10): behaviour is defined by the existing fake and tests
- Discovery of the hypervisor binary and orphan-sweep markers in vmlab (D5): mechanical, follows the QEMU pattern
- Fd-passing and up-front resource acquisition so sandboxing stays possible (D12): a design principle for implementers, not a question

- Protocol version handshake between vmlab and `vmlab-hv` (D18): any explicit version check at connect satisfies it
- ADR revising ADR-0001's argv-outside-the-seam rule (D19): written with the refactor
- SMM emulation for OVMF secure boot (D23): a requirement of phase 2, not a choice
- NVIDIA external-only dma-buf scanout in the frame export (D21): a requirement, behaviour defined by vmlab's existing QEMU patches
- Deterministic replay for the TCG harness (D22) and the benchmark set and host (D28): spec work

- Confirming Arm's position on MIT redistribution of sail-arm-derived code (D34, notes/32): an owner task with Arm or counsel before public aarch64 release; the fallback sources keep the design valid either way
- Phase-1 online snapshots of machines with virtiofs shares (D33): settled 2026-10-30 (Q44) as vhost-user device-state transfer, because a refusal would regress vmlab's container-with-volume snapshots and fail the D13 gate
- The VioFS soak test (D33) lives in the differential suite (D26)
- Converting QEMU-taken online snapshots: not needed, D31 keeps them and refuses cross-hypervisor restore

- Legacy x86 guests' real-mode and 16-bit paths under KVM (D3): a requirement on the x86 machine models, verified by the legacy differential tier (D26)
- When the tap-fd fast path lands (D9): after phase 1, phase unfixed; the stream socket satisfies parity
- The e2e harness's hypervisor parameter and per-phase scope (D13), the `qimg` trait and its parity run (D15): implementation of decided gates
- tpm-tis over swtpm's PTM protocol (D17), QEMU-adapter translation of each capability to QMP (D19), the frame-protocol client in vmlab's recorder and display and its wire layout (D21): spec work under decided designs
- OVMF_VARS validity across hypervisors (D23): follows from D30's identical pflash layout
- The host-config default keyed by machine kind or profile (D29): shape decided, field layout is spec work
- vmlab's Vulkan PCI filter and keep-awake helper apply unchanged to `vmlab-hv` (D24): no decision needed

## Out of scope
- macOS guests: their own phase after phase 2 (phase-2 map D2, 2026-10-30)
- A native viewer app and guest audio: raised separately as `.scratch/plans/hv-viewer/INTAKE.md` (2026-10-30)
- Correcting vmlab PRD §7.5's stale VioFS facts, re-validating virtiofs for Windows profiles under QEMU, and how vmlab learns a template's virtio-win version (a vmlab task, found by notes/14)
- A purpose-built disk format to replace qcow2 (D8): deferred by the owner; the disk-format seam keeps it possible
- Merging the workspace into the vmlab repo (D18): deferred by the owner until the hypervisor is mature
- vmlab's SMB server scope, including AES-128-GMAC signing for Win11 24H2+ guests (vmlab daemon, not the hypervisor)
- ARM Linux hosts, and therefore x86 guest emulation under TCG (deferred by the owner, 2026-10-30)
- Porting QEMU code; the project is clean-room
