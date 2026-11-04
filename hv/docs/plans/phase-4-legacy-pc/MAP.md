---
slug: phase-4-legacy-pc
status: specified
created: 2026-10-31
---

# Phase 4 of vmlab-hv: legacy PC guests

## Destination
The owner's legacy templates (Windows 7 and Vista under windows-legacy, TempleOS, and any XP, 9x or DOS templates) boot unchanged on vmlab-hv's i440fx machine with QEMU-identical hardware; the legacy agent tiers (the isa-serial C agent and the TempleOS HolyC agent) work; floppy and removable media work; the legacy e2e features pass on `--hypervisor vmlab`.

## Grounding
Landed: notes/01-machine-gap.md (i440fx/PIIX gap, E1-E11 estimates of 28-50 kLOC, the A20 defect, public datasheets per device) and notes/02-vmlab-legacy.md (four pc profiles; 17 x86_64 windows-legacy templates including windows-8/8.1 and the 2008/2012/2016 families, templeos, and the x86 store's freedos/95/98; no XP template exists locally; no e2e scenario boots an i440fx guest).
Binding: HV-D10 (legacy agent on COM1), HV-D30 (QEMU-identical hardware), phase-2 D2 (windows-legacy joins phase 4), D14 (no QEMU ROMs; a clean-room TPR ROM only if needed). Local templates: windows-7, windows-vista, templeos plus windows-8/8.1 and the 2008/2012/2016 families (all windows-legacy, note 02 F5), freedos-1.3, windows-95 x3, windows-98 x2.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | Gate guests are freedos-1.3, windows-98-se, windows-7 7600.0.1 and templeos. A new `legacy` e2e lab boots all four on vmlab-hv's pc machine. Every template is captured separately against QEMU (hwref) before its device work, because the Linux superset capture does not show driver-used Cirrus, PCnet, e1000 or IDE paths. XP joins the gate only if a template is built. | Q5 (owner, 2026-10-31) | MAP |
| D2 | A20 acceptance requires a CPU-visible wraparound probe through both i8042 and port 0x92, held across reset and snapshot load. A latched controller bit is not sufficient. | Q6 (owner) | MAP |
| D3 | pc omits the VMware backdoor/mouse and the kvmvapic ROM, as phase 2 does. Windows 7 and XP-class HALs are tested to catch a regression. | Q7 (owner) | MAP |
| D4 | On vmlab-hv, vmlab routes `input_transport = "vnc"` to the native input path. The 9x real-mode key-drop case is re-verified against vmlab-hv's own i8042 rather than reproduced. | Q8 (owner) | MAP |
| D6 | pc keeps the PIIX3 UHCI at 00:01.2 carrying the USB tablet, QEMU-identical. Phase 2's xHCI exception applies to q35 only, because 9x, XP and Windows 7 have no inbox xHCI driver. | Q3 (owner) | MAP |
| D7 | The Windows 95 templates are rebuilt on KVM with `cpu_model = "pentium3"`, applying the guest-side FIX95CPU patch during the template build, and drop `accel = "tcg"`. vmlab-hv gains no x86 TCG and no 95 special case. This is vmlab template work. | Q4 (owner) | MAP |
| D8 | Sub-phases land in order 4a machine core (i440fx/PIIX3/PIIX4 PM, pc firmware tables, IDE+ATAPI, std VGA, COM1 agent + COM2 log, PS/2, A20), 4b networking (e1000, PCnet), 4c floppy + ISA DMA + UHCI, 4d Cirrus, 4e vmlab side (pc description, refusals, D4 input routing, `legacy` e2e lab, D7 rebuilds). 4e may run in parallel from 4a. | Q11 (owner) | MAP |
| D5 | Corrected after the QEMU 11.1.2 socket oracle: TX is immediate while the backend accepts bytes. Match QEMU FIFO overflow, LSR/THRE interrupts, receive backpressure, and divisor-based RX timeouts. The earlier 11 KB/s TX premise was wrong. | Owner, 2026-10-31, after socket verification | notes/03-serial-parity.md |

## Frontier
- [x] Q1 (research) — The i440fx/PIIX machine gap. notes/01-machine-gap.md
- [x] Q2 (research) — vmlab's legacy profiles, templates, agents and e2e. notes/02-vmlab-legacy.md
- [x] Q3-Q9 (decision) — answered, D1-D7.
- [x] Q10 (research) [status: answered] — Per-template QEMU captures for the D1 gate guests. docs/hwref/pc-{freedos-1.3,windows-98-se,windows-7,templeos}/ and docs/hwref/template-captures.md
- [x] Q11 (decision) — Sub-phase order. D8

## Fog

## Accepted unknowns
- Windows 98 SE's PCnet data path and clean shutdown (D1): the QEMU capture never brought PCnet up inside the guest and the guest did not shut down cleanly, so no QEMU reference exists for either. The AMD Am79C970A datasheet defines PCnet, and S1 guest tests on vmlab-hv close both.
- FreeDOS live CD replacement (D1): the capture's media swap failed under QEMU too; the media-change path is specified from the ATAPI standard and tested on vmlab-hv.
- TempleOS pointer use and modes beyond 640x480 (D1): not observed in the capture; covered by the TempleOS S1 test if the guest uses them.

## Out of scope
