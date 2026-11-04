---
id: "01"
kind: research
---
# The macOS machine

## Scope and evidence

Research on 2026-10-31 for phase-macos Q1, written read-only under the brief: this note plus a scratch dir, `target/research/macos-boot/` (captures cited as `research/macos-boot/<file>`). vmlab paths are under `/home/wil/dev/vmlab`. Oracle: QEMU 11.1.2. No QEMU C source was read; QEMU facts come from black-box runs, CLI introspection (`-device ...,help`) and public list/commit pages. One VM boot was made under `systemd-run --user --scope -p MemoryMax=24G -p MemorySwapMax=0`, one at a time, nothing left running; host devices untouched; an unrelated VM from another session was already running and not interfered with. Not checked: `system_profiler` SPPCIDataType/SPUSBDataType (both returned empty on this guest), a live virtio-net observation (the lab declared no NIC), and any macOS hwref capture (none exists).

## What vmlab asks QEMU to run

F1. The `macos` profile (`src/profiles/shipped.wcl:163-199`): q35, OVMF, secure boot and TPM off, `disk_bus = "sata"`, `nic_model = "virtio-net-pci"`, `display = "vmware"`, `cpu_model = "Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on"`, `hyperv = false`, `apple_smc = true`, `input = "usb"`, `tsc = "host"`, 4 CPUs, 8 GiB, `acpi_shutdown = false`, workspace `/Users/Shared/src` (PRD: `docs/vmlab-prd.md:334`).

F2. The exact argv is pinned by vmlab's test `macos_shape_is_phase_0s_command_line` (`src/qemu/cmdline.rs:1713-1750`) and re-measured on this host from the running process (`research/macos-boot/qemu-argv.txt`, OSK redacted). Builder sites: SMC/SMBIOS `cmdline.rs:499-506`, TSC `:473-482`, USB input `:839-865`, agent channel `:596-615`, disks `:646-662`.

```
-name vmlab:<lab>/<vm> -machine q35 -accel kvm
-cpu Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on,+invtsc,tsc-frequency=<Hz>
-smp 4 -m 8192M -device isa-applesmc,osk=<the OSK> -smbios type=2
-display none -vnc unix:<vnc.sock>,share=ignore -qmp <qmp.sock> -monitor none -serial file:<serial.log>
-drive if=pflash,format=raw,readonly=on,file=<vmlab-bundled OVMF_CODE_4M.fd> -drive if=pflash,format=raw,file=<per-VM OVMF_VARS.fd>
-chardev socket,id=vagent0,<agent.sock>,server=on,wait=off -device virtio-serial-pci -device virtserialport,chardev=vagent0,name=vmlab.agent.0
-blockdev qcow2 disk0 -device ide-hd,drive=disk0,bus=ide.0,unit=0
[-netdev stream,net0 + -device virtio-net-pci,netdev=net0,mac=<mac>  when the VM has NICs]
-device vmware-svga -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 -device usb-tablet,bus=xhci.0 -S [-nic none when NIC-less]
```

F3. Placement: unaddressed PCI devices land on q35's `pcie.0`, where QEMU keeps virtio transitional — "the only mode AppleVirtIO drives" (`shipped.wcl:174-177`; assert `cmdline.rs:1747-1749`). No `-usb` (no ICH9 UHCI/EHCI); the input xHCI is id `xhci` (`cmdline.rs:363-365`), separate from `usb = true`'s `usbx` (`src/usb/mod.rs:22`). No Hyper-V flags, TPM or SMM global.

F4. OpenCore is the template's, not vmlab's: it lives in the template disk's EFI system partition and OVMF boots it as a UEFI application (PRD `:334`). Local template `~/.local/share/vmlab/templates/x86_64/macos-tahoe/26.7.0/`: 80 GiB virtual / 17.7 GiB actual qcow2, built 2026-10-30, profile `macos`, first-boot script sets ComputerName/LocalHostName/HostName per clone. Firmware is vmlab's bundled edk2 `ovmf-generic=2026.08+ds-2` (`~/.local/share/vmlab/guest/firmware/x86_64/VERSION`). Agent and family: virtio-serial port `vmlab.agent.0` (`guest/agent-proto/src/lib.rs:44`), in-guest `/dev/cu.vmlab.agent.0`; agent cross-built for `x86_64-apple-darwin` (cargo-zigbuild), root under launchd; stop via agent only (macOS ignores the ACPI power button); shares SMB-only; boot identity `kern.bootsessionuuid` (PRD `:338-347`; `.claude/skills/vmlab/references/host-profiles.md:613-638`; `CLAUDE.md:97-103`).

## What the guest saw (measured, macOS 26.7.2)

F5. Boot to agent ready in 37 s under KVM on the 9950X host (`research/macos-boot/status.txt`); first-boot provisioning ran; the daemon refreshed the agent (cebad00 → 684de81). Host had `/sys/module/kvm/parameters/ignore_msrs = Y`. Reports in `research/macos-boot/g-*.txt`:

- Platform: Model iMac20,1, board-id `Mac-CFF7D910A743CAAF`, serial and UUID present; SIP enabled; System Firmware `2094.80.5`, OS Loader `583~6005` — OpenCore's injection (F11), not QEMU's.
- CPU: GenuineIntel, family 6 model 94 stepping 3, brand "Intel Core Processor (Skylake, IBRS, no TSX)" — the `-hle,-rtm` effect is visible. `machdep.tsc.frequency = 4291934000`, exactly the argv's `tsc-frequency=4291934000` (pinned from KVM, `tsc-hz.txt`); `hw.busfrequency = 4000000000`; "Processor Speed: 5 MHz" is a cosmetic artifact of this synthetic frequency set.
- Graphics: Vendor `0x15ad` Device `0x0405` rev 0, "VRAM (Total): 3 MB", "No Kext Loaded"; IOKitDiagnostics `IONDRVFramebuffer=1`, `IOAccelerator=0` — generic framebuffer path, no accelerator instance.
- SMC: ioreg `AppleSMC` bound by macOS's own `com.apple.driver.AppleSMC` (3.1.9), `smc-count=1`, client class `AppleSMCClient`.
- Storage: "QEMU HARDDISK", SATA, AppleAPFSMedia; AppleAHCI stack loaded.
- USB: `AppleUSBXHCI`/`AppleUSBXHCIPCI` kexts loaded; kernel `IOUSBHostHIDDevice` instances 0 — HID rides DriverKit user space.

## What each piece means for vmlab-hv

### CPU, CPUID leaf 0x40000010, pinned TSC, host MSRs

F6. Leaf `0x40000010`: TSC kHz in EAX, bus/local-APIC kHz in EBX — VMware's generic hypervisor-CPUID proposal (https://lkml.org/lkml/2008/10/1/246). XNU's `cpuid_init_vmm_info()` (osfmk/i386/cpuid.c, https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/i386/cpuid.c) reads it for *any* detected VMM signature — KVM's included — when the max hypervisor leaf reaches it; hence `kvm=on` and `vmware-cpuid-freq=on` coexist (QEMU fills the leaf from the KVM TSC rate; added Jan 2017, https://lists.libreplanet.org/archive/html/qemu-commits/2017-01/msg00049.html; no man-page entry). Measured consumed in F5. vmlab-hv: expose a hypervisor CPUID with max leaf ≥ 0x40000010, filled consistently with the virtual TSC it actually runs.

F7. The invariant TSC: `+invtsc,tsc-frequency=<Hz>` — bare `+invtsc` makes QEMU refuse migration (an online snapshot is one); an explicit frequency lifts that (`cmdline.rs:473-482`, PRD `:216`). vmlab reads the frequency at the VM's first KVM start via a paused, device-less QEMU over QMP (`src/qemu/tsc.rs:1-9,54-65`), pins it in `.vmlab/vms/<vm>/tsc-hz` (`src/labd/vm.rs:214-220`) and carries it through freezes (`src/labd/plan.rs:180-188`), because the host recalibrates its TSC each boot. vmlab-hv probes TSC itself (phase-2 D10) — directly, since unlike vmlab it may ioctl `KVM_GET_TSC_KHZ` (`tsc.rs:3-4`) — and must keep snapshot restore pinned the same way.

F8. Host MSRs: XNU reads Intel MSRs KVM does not emulate and faults on each #GP unless `kvm`'s `ignore_msrs` is on; `up` warns for the macOS family when it is not `Y` (PRD `:347`, `host-profiles.md:385-400`, `cmdline.rs:224-249`). In vmlab-hv the same tolerance must come from the VMM's own MSR handling (swallow unknown RDMSR/WRMSR rather than inject #GP), or the host requirement stays and must be documented — open design point. Vendor/model: `vendor=GenuineIntel` and a supported Intel model are required (macOS checks the vendor); `-hle,-rtm` turns TSX off (PRD `:334`); KVM paravirtual extras stop macOS 13 booting on AMD hosts (`shipped.wcl:180-183`).

### The Apple SMC and the OSK

F9. `isa-applesmc` is an ISA device at I/O base 0x300 (measured: `-device isa-applesmc,help` → `iobase=768`, required `osk=<str>`); data port 0x300, command/status port 0x304 — the Linux applesmc protocol QEMU's device was modeled on (https://raw.githubusercontent.com/torvalds/linux/master/drivers/hwmon/applesmc.c; 2010 patch https://patchwork.ozlabs.org/project/qemu-devel/patch/1277822152-6715-1-git-send-email-agraf@suse.de). Status bits at 0x304: 0x01 output-full, 0x02 input-accepting, 0x04 busy; commands 0x10 READ_KEY, 0x11 WRITE_KEY, 0x12 GET_KEY_BY_INDEX, 0x13 GET_KEY_TYPE, 4-char keys. The OSK is served as keys `OSK0`/`OSK1`, 32 bytes each, concatenated = the 64-char string. vmlab-hv needs only enough of this for AppleSMC.kext to bind and read OSK0/OSK1 (F5 shows it does).

F10. OSK legal position, recorded: upstream QEMU never shipped the real string — the 2010 device already used a dummy plus an "invalid key" warning; the citable rationale is Apple's copyright claim on the string, which made QEMU unwilling to distribute it and distros unwilling to package it (G. Somlo, LKML, https://lkml.org/lkml/2012/12/12/446). Apple failed to have the string treated as a trade secret (Jan 2012 court record, linked from https://raw.githubusercontent.com/kholia/OSX-KVM/master/README.md, which also publishes the string). The oft-told "Apple complained so QEMU removed it" is not corroborated by any citable record. **vmlab-hv must not embed the OSK** (phase-macos MAP destination): it arrives from vmlab in the machine description, as QEMU receives it as a caller-supplied property. Recorded honestly: vmlab itself embeds it today (`src/qemu/cmdline.rs:358-361`, `APPLE_SMC_OSK`) and puts it on QEMU's argv, where any process listing sees it — a vmlab-side decision this phase inherits and should revisit.

### SMBIOS type 2

F11. vmlab adds bare `-smbios type=2` only beside the SMC: "the baseboard table macOS looks for, which QEMU only emits when asked" (`cmdline.rs:496-506`, PRD `:214`). Measured: the guest's Mac identity (iMac20,1, board-id, serial, UUID) is OpenCore's own — PlatformInfo generates and installs SMBIOS regardless of firmware content (Configuration.pdf §10: `Automatic`, `UpdateSMBIOSMode` Try/Create/Custom; sets SMBIOS 3.2; https://github.com/acidanthera/OpenCorePkg/blob/master/Docs/Configuration.pdf). So phase-2a's SMBIOS-via-fw_cfg builder needs only to include a type 2 table on macOS machines; OpenCore replaces the content. QEMU's own type 2 default strings were not measured — not checked.

### VMware SVGA II

F12. The public interface is the VMware DDK header shipped in Linux vmwgfx and Mesa (`svga_reg.h`, "Virtual hardware definitions for the VMware SVGA II device", copyright 1998-2009 VMware): https://raw.githubusercontent.com/torvalds/linux/v3.18/drivers/gpu/drm/vmwgfx/svga_reg.h (identical base set in Mesa today, https://elixir.bootlin.com/mesa/mesa-26.2.6/source/src/gallium/drivers/svga/include/svga_reg.h; no live VMware-hosted spec was found). Shape: PCI 15ad:0405; BAR0 register window (SVGA_INDEX_PORT offset 0, SVGA_VALUE_PORT 1, SVGA_BIOS_PORT 2, SVGA_IRQSTATUS_PORT 8); BAR1 guest framebuffer; command FIFO in BAR2, size from SVGA_REG_MEM_SIZE. Registers: ID=0, ENABLE=1, WIDTH/HEIGHT=2/3, MAX_WIDTH/HEIGHT=4/5, DEPTH=6, BITS_PER_PIXEL=7, PSEUDOCOLOR=8, RGB masks 9-11, BYTES_PER_LINE=12, FB_OFFSET=14, VRAM_SIZE=15, FB_SIZE=16, CAPABILITIES=17, MEM_SIZE=19, CONFIG_DONE=20, SYNC/BUSY=21/22, GUEST_ID=23, SCRATCH_SIZE=29, NUM_DISPLAYS=31, PITCHLOCK=32, IRQMASK=33; handshake by writing SVGA_ID_2 (0x90000002) to SVGA_REG_ID. FIFO words: MIN/MAX/NEXT_CMD/STOP=0-3, CAPABILITIES=4, FENCE=6, CURSOR_* 9-13; drain by writing SYNC and polling BUSY. 2D commands: UPDATE=1, RECT_COPY=3, DEFINE_CURSOR=19, DEFINE_ALPHA_CURSOR=22, UPDATE_VERBOSE=25, FRONT_ROP_FILL=29, ESCAPE=33 (id 2 is a deprecated hole). Measured: QEMU defaults to 16 MiB VRAM (`vgamem_mb`).

F13. What macOS does with it: register-driven mode-sets plus framebuffer writes, no acceleration. Measured `IOAccelerator=0`, `IONDRVFramebuffer=1`, "No Kext Loaded" (F5); community/vendor sources agree macOS guests get no SVGA acceleration (VMware Communities Nov 2024, https://community.broadcom.com/vmware-cloud-foundation/discussion/sequoia-graphics-configuration-not-supported-again; OSX-KVM: resolution changes work while its README says such systems "lack graphical acceleration", https://raw.githubusercontent.com/kholia/OSX-KVM/master/notes.md). Whether Apple's closed driver ever issues 2D FIFO commands is unverified — no public trace exists; the observed boot exercises none. Scope for vmlab-hv: registers + linear framebuffer + enough FIFO for the firmware's GOP driver (OVMF's QemuVideoDxe programs the device; OpenCore then sets its GOP mode) and SYNC/BUSY; a full 2D blit engine is unproven need. The 3 MB "VRAM" in system_profiler is macOS's own accounting, not the BAR size. The frame feeds the phase-2b native frame protocol like any other display.

### USB HID keyboard and tablet

F14. macOS has no PS/2 keyboard driver: Apple's ApplePS2Controller open-source releases end in 2004 (https://api.github.com/repos/apple-oss-distributions/ApplePS2Controller/tags), Hackintosh PS/2 needs third-party VoodooPS2 (https://dortania.github.io/OpenCore-Install-Guide/kext.html), and macOS-on-QEMU setups attach USB input only (https://raw.githubusercontent.com/kholia/OSX-KVM/master/OpenCore-Boot.sh). Hence `input = "usb"`: `usb-kbd` + `usb-tablet` on `qemu-xhci,id=xhci`, no `-usb` (PRD `:215`). Phase 2f's xHCI (phase-2 D6) plus the HID keyboard covers it; measured: the xHCI stack binds (F5). QEMU's unbound-keyboard rule (why scripted keys reach the USB keyboard, `cmdline.rs:832-838`) matters less here: vmlab-hv's input path targets its keyboard device directly (phase-2 SPEC 2b).

### The rest

F15. Disks: AHCI at 00:1f.2 as phase 2a already builds; macOS's own driver runs it (F5) — "OpenCore does not boot from AppleVirtIO's block personality" (PRD `:334`). Transitional virtio on pcie.0 for virtio-net and the agent's virtio-serial is phase-2 D5's shape; AppleVirtIO drives legacy 0.9.5 only. virtio-net was not observed live (no NIC in the lab) — its shape comes from the unit test and docs; verify at capture time. `hyperv = false`: a named CPU model goes without enlightenments anyway (`cmdline.rs:294-310`).

## Licensing and legal position

F16. Apple's SLA for macOS Tahoe 26 grants use "on a single Apple-branded computer at any one time" (§2A), allows up to two additional VM instances but only "on each Apple-branded computer you own or control" (§2B(iii)), and forbids installing or running it "on any non-Apple-branded computer, or to enable others to do so" (§2J): https://www.apple.com/legal/sla/docs/macOSTahoe.pdf. Running macOS guests on a Linux host therefore breaches the EULA's grant terms — a contract issue for the user, not something the VMM infringes (Somlo's analysis: https://www.contrib.andrew.cmu.edu/~somlo/OSXKVM/). For vmlab-hv the position is QEMU's: a general hypervisor; the user supplies macOS, OpenCore and the OSK; the project ships none of Apple's software and embeds no Apple-claimed secret (F10), and says so. The macOS template is the user's own build artefact, as under QEMU today.

## Open questions this raises

- Which exact MSRs does XNU read that KVM #GPs on (F8)? Needs a boot trace before deciding swallow-unknown vs an enumerated list.
- Does AppleVMwareSVGA2 ever push 2D FIFO commands (F13)? A hwref-style macOS capture with FIFO tracing would pin the device model's scope.
- No macOS hwref capture exists (`docs/hwref/` has none); one is needed to fix BDF placement, config-space values and QEMU's type 2 SMBIOS defaults under OVMF+OpenCore.
- Does the OSK belong in vmlab at all (F10)? The phase should propose the QEMU-like caller-supplied shape in vmlab too, for the owner to decide.
- On snapshot restore with the TSC pinned per-VM (F7), what happens when the host TSC moved beyond KVM's tolerance? vmlab reuses the pin; the failure mode is unmeasured.
- macOS ignores the ACPI power button (F4); vmlab-hv's control protocol must mirror vmlab's agent-only stop for this family — what does that imply for its shutdown vocabulary?
