# The macOS phase of vmlab-hv

## Problem

vmlab's `macos` profile runs macOS Tahoe on QEMU. The machine is q35 with
OVMF, and OpenCore sits in the template's EFI partition. It has these parts:
- A named Intel CPU model with vendor `GenuineIntel` on any host, an invariant
  TSC with a pinned frequency, and CPUID leaf `0x40000010`.
- An `isa-applesmc` at port 0x300 serving the OSK.
- An SMBIOS type 2 table.
- VMware SVGA II as the display.
- A USB keyboard and tablet on an xHCI.
- AHCI disks, and transitional virtio for the network and the agent.

vmlab-hv refuses this machine today. Until it runs it, macOS is the one modern
guest family that still needs QEMU.

The capture (`docs/hwref/q35-macos-tahoe/`) narrows the work further. macOS
never touched the VMware SVGA registers or FIFO. It drew on the firmware's VBE
linear framebuffer, at 640x480 and then 1280x800, both 32-bit.

## Solution

Extend vmlab-hv's q35 machine with what macOS needs, reusing phase 2's AHCI,
transitional virtio, xHCI with the HID tablet, fw_cfg/SMBIOS, OVMF pflash,
snapshot and display paths. The additions are:
- A named CPU model, with vendor and feature masking.
- CPUID leaf `0x40000010` filled from the virtual TSC vmlab-hv runs, with
  `invtsc` and the TSC frequency pinned per machine.
- Handling of unknown MSRs in userspace, on macOS machines only (D2).
- An applesmc device that serves a caller-supplied OSK (D1).
- An SMBIOS type 2 table.
- A VMware SVGA II PCI identity carried by the VGA/VBE core and framebuffer
  (D3).
- A USB HID keyboard beside the tablet.

On the vmlab side, describe macOS machines to vmlab-hv, pass the OSK from the
host config, and lift the refusal.

## User stories

1. As a vmlab user, my `macos-tahoe` template boots unchanged on
   `hypervisor = "vmlab"` to agent-ready, with display, keyboard and pointer,
   networking, and stop through the agent.
2. As a vmlab user, online snapshots and resets of a macOS machine round-trip
   on vmlab-hv, with the TSC frequency held across them.
3. As a vmlab user, a macOS machine on vmlab-hv needs no host-wide
   `kvm.ignore_msrs=Y`.
4. As the owner, neither vmlab nor vmlab-hv ever contains the OSK. vmlab
   refuses an `apple_smc` machine until `host.apple_smc_osk` is set (D1).
5. As a vmlab user, a `macos` e2e scenario proves the family on vmlab-hv,
   and macOS no longer needs QEMU or any host `kvm` module option (D4).

## Implementation decisions

### Machine description (`vmlab-hv-proto`, additive)

- **CPU model.** The machine description gains a named CPU model with feature
  edits. It covers the macOS profile's `Skylake-Client-v4` with `-hle,-rtm`,
  `vendor=GenuineIntel` and `kvm=on`. It reuses the field 4a adds for pc
  (`cpu_model`) rather than adding a second one. The supported names are a
  closed list, and an unknown name is refused.
- **TSC.** It gains `invtsc` and a pinned TSC frequency in kHz. When the
  frequency is absent, vmlab-hv probes it itself (phase-2 D10) and reports it,
  so vmlab can pin it as it does for QEMU today.
- **Hypervisor CPUID frequency leaf.** A flag for leaf `0x40000010`, the
  equivalent of QEMU's `vmware-cpuid-freq`, makes the max hypervisor leaf
  reach it.
- **applesmc.** An `applesmc` section carries the OSK as a string. The OSK is
  never logged, never echoed in errors or in the description dump, and never
  stored in snapshot device state. Restore takes it from the description
  again.
- **USB keyboard.** A flag places a USB HID keyboard on the xHCI beside the
  tablet.
- **Display.** `DisplayDevice` gains `VmwareSvga`.
- **SMBIOS type 2.** The description requests a type 2 table.
- **Unknown MSRs.** A per-machine policy chooses whether unknown MSRs `fault`
  (today's behaviour) or are `ignore`d and logged (D2).

### CPU and MSRs (`vmlab-hv-vmm`)

- **The named model.** It is a CPUID table defined from Intel's published
  CPUID documentation for that microarchitecture, with the masks applied. The
  exact leaf values QEMU presents are pinned by a capture (see Open
  questions), and the model is checked against it.
- **Leaf `0x40000010`.** EAX is the TSC frequency in kHz, from the rate KVM
  is actually running (`KVM_GET_TSC_KHZ`), the same value the TSC pin carries.
  EBX is the local APIC bus rate in kHz that KVM emulates, 1,000,000 as the
  capture shows, not the TSC rate.
- **Unknown MSRs (D2).** KVM's userspace MSR exits handle MSRs that KVM
  rejects (`KVM_CAP_X86_USER_SPACE_MSR` with the unknown-MSR reason). Reads
  return 0 and writes are dropped, and each MSR index is logged once per
  machine. This applies only where the machine's MSR policy says `ignore`, so
  every other machine keeps faulting.
- **Snapshots.** The CPU model, the TSC pin and the logged-MSR set ride the
  snapshot. A restore under a different TSC frequency is refused, as phase 2
  already does.

### Devices (`vmlab-hv-devices`) and firmware

- **applesmc** on ISA:
  - **Ports:** data at 0x300, command and status at 0x304.
  - **Status bits:** 0x01 output-full, 0x02 input-accepting, 0x04 busy.
  - **Commands:** `READ_KEY` 0x10, `WRITE_KEY` 0x11, `GET_KEY_BY_INDEX`
    0x12 and `GET_KEY_TYPE` 0x13, with four-character keys.
  - **The key set:** `OSK0` and `OSK1` (32 bytes each, the two halves of the
    OSK), plus whatever the capture's `applesmc.md` records macOS reading.
    Values come only from the description.
  - **Source:** the public Linux applesmc driver's protocol.
- **VMware SVGA II (D3).**
  - **What it has:** PCI identity 15ad:0405 with BAR layout and the VGA/VBE
    core from phase 2, plus a linear framebuffer delivered through the native
    display protocol.
  - **Registers:** the SVGA register window answers the ID and capability
    reads a driver would make, with no FIFO command processing.
  - **Blitting:** none, unless the P1 boot shows macOS issuing FIFO commands.
    If it does, it stops with the evidence.
- **USB HID keyboard** on the existing xHCI and HID code. It's a boot-protocol
  keyboard with the report descriptor the capture's `hid-exercise.json`
  shows. vmlab-hv's input path targets it directly.
- **SMBIOS type 2.** The phase-2 SMBIOS builder emits a baseboard table with
  QEMU's default strings, which come from the capture's fw_cfg SMBIOS blobs.
  OpenCore replaces the Mac identity regardless.

### vmlab side

- `describe.rs` describes a resolved macOS machine to vmlab-hv:
  - The CPU model, `invtsc`, the TSC pin and the frequency leaf.
  - applesmc with the OSK from `host.apple_smc_osk`.
  - The USB keyboard, VMware SVGA, SMBIOS type 2, and the MSR `ignore`
    policy.
- `vmlab_refusals` lifts the macOS refusal for vmlab-hv and adds one for macOS
  on QEMU (D4): the `macos` profile defaults to `hypervisor = "vmlab"`, a
  macOS machine on QEMU is refused with a message naming vmlab-hv, and the
  `ignore_msrs` warning and the QEMU `isa-applesmc` argv path are removed.
- `labd` takes the TSC frequency vmlab-hv reports when the VM has no pin yet,
  instead of launching the QEMU probe.
- Every place vmlab logs or records a machine description redacts the OSK, as
  #195 did for the QEMU argv.

### Clean room

- **Sources.**
  - Intel's CPUID documentation.
  - VMware's public `svga_reg.h` (Linux vmwgfx, Mesa).
  - The Linux applesmc driver's protocol.
  - The USB HID specification.
  - KVM's API documentation.
- **QEMU.** The captures fix what QEMU presents. QEMU C source is never read.

## Testing decisions

- **P1, S1 in macOS mode.** `vmlab-hv-tests` boots a fresh overlay of
  `x86_64/macos-tahoe` and checks, in order:
  1. The agent becomes ready and `exec` works.
  2. A native frame arrives at 1280x800.
  3. USB keyboard input lands.
  4. The network pings.
  5. A reset and an online snapshot load both round-trip, with the TSC
     frequency unchanged.
  6. The machine stops through the agent.

  The run also confirms that no host `ignore_msrs` is needed, and records the
  MSRs it logged. It skips with a reason when the template, `/dev/kvm` or the
  host key is missing.
- **P2, the hwref differential.** The static state vmlab-hv presents is diffed
  against `docs/hwref/q35-macos-tahoe/`: PCI, ACPI, SMBIOS including type 2,
  fw_cfg and e820. Every difference is either fixed or put on the allow-list
  with a reason.
- **P4, device tests at the guest interface.**
  - **applesmc:** status sequencing, `READ_KEY` of the OSK halves from a fake
    key, key-by-index and key types. The OSK never appears in any log or
    error.
  - **CPUID:** the named model's leaves against the CPUID capture, and leaf
    `0x40000010` against the pinned TSC.
  - **MSR exits:** read 0, write dropped, one log line per index, and the
    `fault` policy unchanged.
  - **VMware SVGA:** identity and BARs, and VBE mode set with framebuffer
    scanout.
  - **USB keyboard:** HID reports.
- **P5, a new `macos` e2e scenario in vmlab.** It runs on `--hypervisor
  vmlab` only (D4):
  - The agent becomes ready.
  - `exec` works, and `cp` works both ways.
  - A screenshot is taken.
  - A snapshot round-trips.
  - The machine stops.

  It skips cleanly without the template or `host.apple_smc_osk`. It's tagged
  to the macOS phase so it runs on vmlab-hv once that phase is delivered.
- **Merge bar.** `just ci::check` stays green, with P1 and P2 skipping cleanly
  where templates or the key are absent.

## Out of scope

- **Apple Silicon and arm64 macOS.** vmlab's macOS support is Intel only.
- **Graphics acceleration and a VMware SVGA blit engine.** These come back
  only if P1 shows macOS issuing FIFO commands.
- **Shipping Apple software.** No macOS, OpenCore or OSK is ever distributed.
  The user supplies all three.
- **A PS/2 path for macOS.** macOS has no PS/2 driver.

## Open questions

- **The exact CPUID leaves QEMU presents for the macOS profile's model on this
  AMD host.** The macOS capture did not inventory them. The build's first step
  captures them black-box by booting a Linux guest under QEMU with the same
  `-cpu` string and dumping every leaf. The named model is then pinned to that
  capture.
- **The MSR list (D2).** It comes from vmlab-hv's own logging during the first
  P1 boot. Whether the catch-all policy or an enumerated list ships is
  decided then, as D2 says.
- **Snapshot restore when the host TSC moved beyond KVM's tolerance.** Note 01
  left this unmeasured. **Deferred by the owner on 2026-11-01 to an
  owner-coordinated host reboot.** The owner is away, and a reboot would end
  every active session. This measurement is not a merge blocker for this
  delivery. P1's passing same-host snapshot round-trips cover the normal case.
  The passing S1 regression
  `macos_snapshot_preserves_cpuid_msr_log_and_refuses_changed_tsc_frequency`
  sets `IA32_TSC` to `1 << 60` and verifies rollback on restore, restoration
  of CPUID and the pinned frequency, and preservation of the union of logged
  MSR indices. It also verifies that a changed destination TSC frequency is
  rejected before guest RAM changes. This synthetic evidence does not
  establish cross-reboot behavior. Measure and record that behavior during
  the future owner-coordinated reboot.
- **The macOS shutdown vocabulary.** macOS ignores the ACPI power button, so
  vmlab already stops it through the agent. Whether vmlab-hv's control
  protocol needs anything beyond `quit` for this family is checked in P1.
