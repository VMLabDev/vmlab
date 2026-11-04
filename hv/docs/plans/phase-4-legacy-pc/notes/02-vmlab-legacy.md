---
id: "02"
kind: research
---
# vmlab's legacy guests

## Evidence and contract

F1. Source inspection of /home/wil/dev/vmlab at HEAD (post PR #189, merge `b6761b9`, 2026-10-30) plus the local template store at ~/.local/share/vmlab/templates/. No VM was booted, no QEMU C source read, no measurements taken beyond file counts; nothing was written but this note. The phase-4 contract: "i440fx/PIIX, IDE, floppy, cirrus, pcnet, e1000, isa-serial agent on COM1, all QEMU-identical (D30, D10)" — /home/wil/dev/vmlab-hypervisor/.scratch/plans/vmlab-hypervisor/SPEC.md:179; phase-2 D2 put windows-legacy here; HV-D10 keeps the legacy agent on COM1; D29 removes QEMU after this phase. Phase-4 MAP: .scratch/plans/phase-4-legacy-pc/MAP.md:14.

## The legacy profiles

F2. Four shipped profiles set `machine = "pc"` (Machine::I440fx → argv `-machine pc`, src/profiles/mod.rs:24-30). All are SeaBIOS (QEMU's x86 default — nothing added, src/qemu/cmdline.rs:580), `secure_boot = false`, `tpm = false`:

- **windows-legacy** (shipped.wcl:63-76): IDE disk, e1000 NIC, `display = "std"`, 2 cpus, 2GiB, `hyperv = true`, default virtio-serial agent ("virtio-win covers this era"). Vista/7/2008-era: no virtio drivers at install time.
- **windows-xp** (78-94): same shape, `agent_transport = "isa-serial"` (COM1), `rtc = "localtime"` (NT4–2003 read the RTC as local time), no `hyperv` (predates the enlightenments, shipped.wcl:11-13).
- **windows-9x** (96-114): **pcnet** NIC, `display = "cirrus-vga"`, 1 cpu, 256MiB floor, isa-serial agent, `rtc = "localtime"`, and `input_transport = "vnc"` — real-mode DOS/9x TUIs (fdisk, setup) drop QMP send-key events between menu redraws, so scripted keyboard input rides the VNC socket instead. There is no separate DOS/3.x profile: DOS and Windows 3.x-ME-2000 ride this one (a local freedos-1.3 template proves it, F5).
- **templeos** (116-128): IDE, std VGA ("it drives VBE directly"), isa-serial agent for the HolyC agent, 2 cpus, 512MiB. `nic_model = "e1000"` is only a model choice; with no `nic` block the argv gets `-nic none` (cmdline.rs:746-750) — "no network by design".

F3. Field semantics (src/profiles/profile_schema.wcl:30-46): `agent_transport` is "virtio-serial" (default) | "isa-serial" (COM1, the legacy agent) | "none" (screen-driven, never ready); `input_transport` "qmp" (default) | "vnc"; `rtc` "utc" | "localtime". `disk_bus`, `nic_model`, `agent_transport`, `input_transport`, `rtc` and `machine` resolve from the profile only; `display`, `cpu_model`, `hyperv`, `accel` resolve VM > template > profile (src/qemu/resolve.rs:596-654). Arch `x86` is a display-only alias that runs on the x86_64 emulator and the same pc/q35 machines (cmdline.rs:108-114, resolve.rs:502-509). `cpu_model` is "named for guests the host CPU's CPUID breaks (Windows 9x Setup on a modern host)" (resolve.rs:73-78).

F4. The argv shape for a legacy VM (src/qemu/cmdline.rs:427-890). Measured against the builder, a windows-xp-shaped VM produces:

```
-name vmlab:<lab>/<vm> -machine pc -accel kvm -cpu host -smp 2 -m 2048M
[-rtc base=localtime]                                # profile rtc (cmdline.rs:486-491)
-display none -vnc unix:<vnc.sock>,share=ignore -qmp unix:<qmp.sock>,server=on,wait=off -monitor none
-chardev file,id=serlog,path=<serial.log>            # serial log moves to COM2
-device isa-serial,chardev=serlog,index=1            #   when agent owns COM1 (537-552)
-chardev socket,id=vagent0,path=<agent.sock>,server=on,wait=off
-device isa-serial,chardev=vagent0,index=0           # agent = 16550 on COM1 (588-615)
-blockdev driver=qcow2,node-name=disk0,file.driver=file,file.filename=<disk>
-device ide-hd,drive=disk0,bus=ide.0,unit=0,bootindex=50   # disks first, 50+i (633-665)
-blockdev driver=raw,node-name=cd0,read-only=on,...
-device ide-cd,drive=cd0,bus=ide.0,unit=1,bootindex=0[,id=install-cd]  # CDs bootindex i (680-707)
[-drive if=floppy,format=raw,file=<fd.img>]          # floppy (709-715)
-netdev stream,id=net0,server=off,addr.type=unix,addr.path=<switch.sock>
-device e1000,netdev=net0,mac=...                    # or pcnet; no host_mtu (719-750)
-device VGA                                          # display "std"; "cirrus-vga" passes through raw (resolve.rs:458-479)
-usb -device usb-tablet                              # tablet on pc's PIIX3 UHCI (821-865)
-S  [qemu_args... last]
```

Details that phase 4 must reproduce: IDE slots are addressed, never auto-placed — two units per bus, primary first, disks from the front, CD-ROMs continue, exactly four slots on `pc` (cmdline.rs:399-416); SeaBIOS gets an explicit boot order (CDs lowest, disks 50+, floppy never indexed, 619-623); `hyperv = true` adds the hv-* flag string under KVM (HYPERV_FLAGS, cmdline.rs:256-258), skipped under TCG or a named cpu_model (294-310); a named `cpu_model` replaces the accelerator default (467-472); `cirrus-vga` and `pcnet` are not in the schema's documented value lists — they ride `display_device_name`'s/`nic_model`'s "power users may name a QEMU device directly" pass-through (resolve.rs:477, schema comment profile_schema.wcl:20-21).

## Local templates, classified

F5. Measured from every `template.wcl` under the store (profile declared per template; 69 versions in x86_64/, 15 in x86/):

- **Phase 4, profile windows-legacy (pc/i440fx)** — 17 x86_64 versions: windows-7 ×2 (7600.0.0/.1), windows-vista, windows-8, windows-8.1, windows-server-2008 + -core, 2008-r2 + -core, 2012 + -core, 2012-r2 + -core, 2016, 2016-core ×2. All 2 cpus/4GiB/40GiB. So windows-8/8.1 and the 2008/2012/2016 families (which the MAP left "to classify") are all windows-legacy: i440fx, IDE, e1000 — phase 4, not phase 2.
- **Phase 4, profile templeos** — templeos 5.03.2 (2 cpus/512MiB/2GiB, `agent_version = "agent-templeos=f1e4515"`, origin https://templeos.org/Downloads/TempleOS.ISO).
- **Phase 4, profile windows-9x** — only in the `x86/` store (arch "x86", same x86_64 emulator and pc machine): freedos-1.3, windows-95/95-osr2/95-osr25, windows-98/98-se. 1 cpu, 64–128MiB, 2GiB disks, all stamped `agent-legacy=5692f58` (freedos `f1e4515`). **All three 95 templates pin `accel = "tcg"` and `cpu_model = "pentium3"`; 98/98-se/freedos pin only `cpu_model = "pentium3"`** and stay KVM. The 32-bit windows-legacy family is also there (vista, 7 ×3, 8 ×2, 8.1, 2008 + core).
- **Already phase 2 (q35)**: windows-10/11, windows-server profile (2019/2022/2025 ± core), linux-modern (steamos, debian-13, ubuntu-24.04/26.04, fedora-44, arch, alpine-3.23, rocky-9, almalinux-10, nixos-25.11, opensuse-leap/tumbleweed, parrot), kali (linux-generic — q35 + SeaBIOS, still q35). macos-tahoe is the macos profile (own later phase). riscv64/aarch64 stores are phase 3.
- Agent stamps: modern agent `agent=57fa802` on windows-7 7600.0.1, 2008-r2 ±core, 2012 ±core, 2012-r2 ±core, 2016 ±core; vista, 2008 ±core, 7 7600.0.0, 8, 8.1 carry no `agent_version` (what an unstamped legacy template does at `up` — agent.update, repair — was **not checked**).

## The legacy agent tier

F6. PRD §7.4 legacy tier (docs/vmlab-prd.md:521): `agent_transport = "isa-serial"` wires the same host socket to a 16550 on COM1, the serial console log moves to COM2, and the guest runs `vmlab-agent-legacy` — same wire (agent-proto v2), C89. The code advertises features `["exec","set_time"]` (guest/agent-legacy/src/agent.c:6-7,152) and refuses every other open by name so the host feature ladder degrades truthfully; readiness, `vmlab exec`, `os_info` and the stop ladder work. (guest/agent-legacy/README.md:11-15 still says "one feature, `exec`" — stale against the code.) Conformance: src/labd/legacy_agent_tests.rs compiles the C with the host cc and drives it with the daemon's own `AgentHandle` over a unix socket — the same shape as QEMU's chardev socket (legacy_agent_tests.rs:1-63).

What the hypervisor must supply:

- **A 16550 on COM1 wired to the agent socket** (QEMU: `-device isa-serial,chardev=vagent0,index=0`), **plus a second UART on COM2** for the serial log when one is configured (cmdline.rs:537-552). The host side is a chardev socket — no baud on the wire.
- **Baud 115200 8N1, programmed guest-side**: divisor 1 (plat_dos.c:60-64, VmlabAgt.HC:30-34), CBR_115200 (plat_win32.c:97-101), B115200 (plat_posix.c:45-50). QEMU times UART transmit to the programmed baud — roughly 11 KB/s at 115200, which is why the tier refuses file transfer (README.md:64-65) — and the DOS port polls relying on "the emulated 16550 stops feeding the UART while its FIFO is full" (plat_dos.c:5-6). vmlab-hv's 16550 therefore needs QEMU-identical FIFO and divisor-timed TX behaviour (D30), not just byte plumbing; the wire layer resynchronises on the magic because a serial line can drop or replay bytes across an online snapshot restore (README.md:39-41).
- **The TempleOS agent is installed by typing at the screen**: `vmlab::templeos_agent_script()` returns the HolyC source as `A("…")` statements and a template's provision types it with `vm.type_text_paced(script, 40)` — roughly 12,000 keystrokes, ~8 minutes at 40 ms each, once per template build (agent-templeos/README.md:41-56). This needs the std-VGA display path (VBE; the profile says TempleOS "drives VBE directly") and a **PS/2 keyboard path that lands every keystroke, shifts included** — input must be the QMP-style transport; over VNC "TempleOS sees every shift a keystroke late" (README.md:58-60). `$` is written `0x24` because DolDoc treats it as a command (README.md:62-64); power-off is a write to the PIIX4 sleep register (README.md:28-31), so PM ACPI matters here too. Whether VmlabAgt answers `set_time` was not checked.

## e2e features and scenarios tagged phase 4

F7. PR #189 tags every feature with the vmlab-hv phase that makes it applicable (e2e/features.py:214-233; `PHASE_ORDER` ends `"3", "4"`; `VMLAB_DELIVERED = "1"`). The phase-4 group is exactly three features (features.py:291-292): **`vm.hw.floppy`** (a `floppy` attachment), **`vm.media.floppy`** (media built from a folder as a floppy), **`share.smb1`** (SMB1 shares for legacy guests). `out_of_scope` skips features past the delivered phase on vmlab-hv runs (features.py:313-323).

F8. Where they run today — and the gap: the floppy features run in `core.py` on vm02/vm03 of labs/core-lab/vmlab.wcl, which are **e2e-alpine guests (linux-modern, q35, OVMF)** reading `/dev/fd0` (e2e/scenarios/core.py:155-162,522-532); `share.smb1` runs in `unsupported.py` on a Linux guest mounting with `vers=1.0` against smbd configured `server min protocol = NT1`, explicitly noting "an XP-era client itself was not exercised" (e2e/scenarios/unsupported.py:185-200). **No e2e scenario boots an i440fx guest, the legacy agent tier, TempleOS, 9x or XP at all** — the phase-4 tags cover devices and dialects, not legacy guests. SMB1 itself is hypervisor-independent: `smb1 = true` relaxes vmlab's own smbd to NT1 with NTLMv1 (src/smb/config.rs:18-21,180-223), checked against `smbd -b` for WITH_SMB1SERVER (config.rs:300-306); XP-era guests without an agent get their `net use` line typed at the console through the keystroke surface (src/smb/mount.rs:323-341). The hypervisor's part is only the NIC those guests drive (e1000/pcnet).

## Refusals today and the vmlab-side work

F9. `vmlab_refusals` is the one list (src/config/hypervisor.rs:88-153, mirrored by the daemon at src/labd/machine_build.rs:336-340): today every VM on vmlab-hv is refused whole — "runs container micro-VMs only so far" (hypervisor.rs:104-107) — plus per-VM reasons for `qemu_args`, `accel = "tcg"`, and non-x86_64 arch. PRD §22's phase table says phase 4 "Legacy PCs" is Refused, and "the refusals are one list in code, which each phase shrinks" (docs/vmlab-prd.md:2553-2569). No legacy-specific refusal exists yet; legacy profiles are covered by the blanket VM refusal. Note the `accel = "tcg"` refusal already collides with the windows-95 templates (F5) — those stay QEMU-only under D3 (no x86 TCG).

F10. vmlab-side work phase 4 implies (all in vmlab, none started):

1. **A VM machine description** beside the container-only one (src/labd/hypervisor/vmlab_hv/describe.rs:26 describes containers; the proto already has `SerialPort`): pc machine, IDE disks with slot addressing and SeaBIOS bootindex, e1000/pcnet NICs, VGA/cirrus-vga display, floppy, isa-serial agent on COM1 + log on COM2, PS/2 input, `-rtc base=localtime`, `cpu_model` (pentium3), Hyper-V flags.
2. **Shrink `vmlab_refusals`** for legacy VMs and update the doc wording that still says "refused on a VM until vmlab-hv's phase 2" (src/config/schema.wcl:297, src/config/host_schema.wcl:57).
3. **`input_transport = "vnc"` on vmlab-hv**: the display layer branches to a `VncClient` on the machine's VNC socket for keys, text and pointer (src/labd/display/mod.rs:163-226). vmlab-hv serves frames natively (D8/D21); either its viewer-facing RFB server also serves input, or the transport maps to the native input path — and the real-mode key-drop reason 9x chose VNC must be re-verified on vmlab-hv's i8042.
4. **e2e**: flip `VMLAB_DELIVERED` to `4` (features.py:233) and, if the gate is to prove legacy guests at all, add features/scenarios that boot them (F8's gap; S4 per SPEC.md:188).
5. SMB1 needs nothing hypervisor-side (vmlab's own smbd); the legacy Windows guests need only their NIC and, for XP-era share mounts, the keystroke surface.

## Open questions this raises

- Q1. The windows-95 templates pin `accel = "tcg"`; D3 forbids x86 TCG on vmlab-hv. Do they stay QEMU-only forever, or are they rebuilt KVM + pentium3 like the 98 ones? Evidence: F5, F9.
- Q2. Does phase 4's pc keep the PIIX3 UHCI that `-usb` gives the tablet (D30-identical), or take phase 2's approved D6 xHCI break? Evidence: F4, phase-2 MAP D6.
- Q3. Where does `input_transport = "vnc"` land on vmlab-hv — hv's RFB server, the native input path, or a refusal? Evidence: F10.3.
- Q4. Must vmlab-hv's 16550 time transmit to the programmed divisor (QEMU-identical, ~11 KB/s) or only get FIFO/overrun semantics right? What do the polling DOS/TempleOS agents actually require? Evidence: F6.
- Q5. Which legacy template is the canonical phase-4 gate guest, and does the e2e suite grow legacy-guest features (today none boot an i440fx guest)? Evidence: F7, F8.
- Q6. The unstamped windows-legacy templates (vista, 2008 ± core, 7 7600.0.0, 8, 8.1) — built without a sealed agent? Does phase 4 need them agent-ready for readiness/stop paths? Evidence: F5 (not checked).
- Q7. Does the TempleOS agent answer `set_time` (the C agent does), and does the stale README feature list matter for the gate? Evidence: F6.
