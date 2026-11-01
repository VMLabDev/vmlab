# Core and administration coverage

Before this change, `QEMU_SCENARIOS` pinned `core` and `admin` to QEMU.
`Harness._record` consequently skipped 33 delivered VM features, even when
their checks passed. `core` reports 25 of them and `admin` reports nine.
Both report `lab.destroy`.

## Original skip reasons

Every row below was skipped by the scenario pin, not by its feature phase.
These reasons distinguish the declarations behind that pin:

- C: `core-lab` mixes supported hardware with `vm01.qemu_args`, `vm02.floppy`,
  and `vm03` floppy media. `vmlab_refusals` rejects those declarations.
- A: `admin-lab` declares an aarch64 `arm01`, which `vmlab_refusals` rejects.
- P: The lab is already compatible. Only the enclosing scenario pin blocks it.

| Feature | Scenario | Original lab and machines | Reason |
| --- | --- | --- | --- |
| `lab.up` | core | core-lab, all four | C |
| `lab.up.partial` | core | core-lab, vm01-vm03; blank stays stopped | C |
| `lab.depends_on` | core | core-lab, vm01 and vm02 | C |
| `lab.depends_on_provision` | core | core-lab, vm03 and vm01 | C |
| `lab.provision.parallel` | core | core-lab, vm02 and vm03 | C |
| `lab.status` | core | core-lab, all four | C |
| `lab.down` | core | core-lab, all four | C |
| `lab.up.reload` | core | core-lab, vm01 and dependencies | C; also checks QEMU SMBIOS argv |
| `lab.reload.stopped` | core | core-lab, vm02 and vm01 | C; also checks QEMU memory argv and PID |
| `lab.reload.network` | core | core-lab, lan and vm01 | C; also checks QEMU PID |
| `lab.up.template-changed` | core | core-lab, vm03 | C |
| `lab.destroy` | core, admin | core-lab, admin-lab | C, A |
| `vm.start` | core | core-lab, vm03 | C |
| `vm.stop` | core | core-lab, vm02 and blank | C; also checks QEMU process exit |
| `vm.restart` | core | core-lab, vm03 | C |
| `vm.destroy` | core | core-lab, vm03 | C |
| `vm.ip` | core | core-lab, vm01 and vm02 | C |
| `vm.hw.cpus-memory` | core | core-lab, vm01 | C |
| `vm.hw.disk` | core | core-lab, vm01 | C |
| `vm.hw.disk-from` | core | core-lab, vm01 | C |
| `vm.hw.cdrom` | core | core-lab, vm01 | C |
| `vm.hw.nested` | core | core-lab, vm01 and vm02 | C |
| `vm.scratch` | core | core-lab, blank | C; blank itself is compatible |
| `vm.media.iso` | core | core-lab, vm01 | C |
| `share.unmountable` | core | core-unmountable, um | P |
| `lab.list` | admin | admin-lab, arm01 | A |
| `lab.info` | admin | admin-lab, arm01 | A |
| `lab.stop` | admin | admin-lab, arm01 | A; also checks QEMU process exit |
| `lab.restart` | admin | admin-lab, arm01 | A |
| `lab.destroy.named` | admin | admin-lab, arm01 | A |
| `lab.down.release` | admin | admin-roots, blank | P |
| `lab.move` | admin | admin-roots, blank | P; also identifies QEMU by name |
| `lab.status.conflict` | admin | admin-roots, blank | P; also identifies QEMU by name |

## Coverage after the split

QEMU keeps `core-lab`, `admin-lab`, and every original feature check.
On vmlab-hv, `core-hv` uses two machines for lifecycle, hardware and parallel
provisions. `core-hv-provision` uses two for the provision dependency, and
`core-hv-scratch` uses one for the scratch boot and forced stop. These labs
run in sequence and reuse the original media and provision script.
`admin-hv` uses one x86_64 scratch VM for the administration checks.
`core-unmountable` and `admin-roots` run unchanged on both hypervisors.

Four other features remain intentionally skipped on vmlab-hv:

| Feature | Scenario and QEMU lab | Skip reason |
| --- | --- | --- |
| `vm.hw.qemu_args` | core, core-lab/vm01 | QEMU-only command-line arguments |
| `vm.hw.floppy` | core, core-lab/vm02 | Phase 4 floppy controller |
| `vm.media.floppy` | core, core-lab/vm03 | Phase 4 floppy controller |
| `arch.emulated` | admin, admin-lab/arm01 | Phase 3 aarch64 TCG emulation |

Phase tags are unchanged. `lab.validate`, `lab.validate.reject`, and
`daemon.lifecycle` are host features that already ran on both hypervisors.
