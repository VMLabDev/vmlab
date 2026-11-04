---
id: "03"
kind: research
---
# Lifting from crosvm

## Scope and evidence

Researched 2026-10-30. Static inspection and measurements, plus one QEMU black-box PCI observation. No QEMU C source read. No code, interfaces, branches, or commits changed.

Source aliases below expand to absolute paths:
- `C` = `/tmp/claude-1000/-home-wil-dev-vmlab/eaa029fb-0559-42b8-8dec-30bd86055325/scratchpad/qv/crosvm`, clean checkout `ad804a6ecf1d3c4337f18b56092fc54d8ca574fa`.
- `H` = `/home/wil/dev/vmlab-hypervisor`.
- `V` = `/home/wil/dev/vmlab`.
- `R` = https://github.com/magma-gpu/rutabaga_gfx/blob/5bcb95f9cdb0ddb845a6081adc80828ed10fb258
- `F` = https://github.com/rust-vmm/vfio/blob/93bcf25f3f8196314a71173ae96749b269a65b19

The contract selects crosvm GPU/rutabaga under D24 and crosvm USB under D25, while D30 still requires QEMU-identical hardware. D21 requires native frames, RFB, and NVIDIA external-only dma-buf support. These requirements remain binding; a source lift does not waive them. `H/.scratch/plans/vmlab-hypervisor/SPEC.md:175-180`.

## Measured size

Physical Rust lines include comments, blanks, generated bindings, and inline tests. Counts are source size, not dependency closure or production-only code. Reproduction from `C`:

```sh
find devices/src/virtio/gpu -name '*.rs' -exec wc -l {} +
find gpu_display/src -name '*.rs' -exec wc -l {} +
find devices/src/usb/xhci -name '*.rs' -exec wc -l {} +
find devices/src/usb/backend -name '*.rs' -exec wc -l {} +
find usb_util/src usb_sys/src vfio_sys/src -name '*.rs' -exec wc -l {} +
wc -l devices/src/vfio.rs devices/src/pci/vfio_pci.rs
```

| Component | Rust files | Physical lines |
|---|---:|---:|
| virtio-gpu subtree | 6 | 5,768 |
| GPU wrapper `mod.rs` / resource manager `virtio_gpu.rs` | 2 | 2,194 / 1,814 |
| GPU protocol / EDID / parameters / snapshot helper | 4 | 1,003 / 499 / 168 / 90 |
| gpu_display/src, all platforms | 31 | 14,363 |
| display lib / stub, subsets of previous row | 2 | 731 / 184 |
| xHCI | 18 | 7,643 |
| USB backend, including FIDO | 21 | 4,690 |
| host_backend / fido_backend, subsets of previous row | 3 / 11 | 564 / 2,144 |
| usb_util / usb_sys | 5 / 1 | 2,057 / 253 |
| VFIO ioctl wrapper / PCI device | 1 / 1 | 2,000 / 2,240 |
| vfio_sys bindings and ioctl definitions | 3 | 1,029 |

Additional measured support: register_space 935 lines, devices/utils 385; GPU queue 623, descriptor_utils 1,664, resource_bridge 90. They are not all mandatory lifts. Display also has a 117-line build script, 1,322-line Wayland C file, and 2,062 protocol XML lines. Reproduce with the same recursive `find` for those directories and `wc -l` for `C/gpu_display/{build.rs,src/display_wl.c,protocol/*.xml}`.

## Shared integration constraints

F1. crosvm's `vm_memory` is its own crate. vmlab uses rust-vmm `GuestMemoryMmap<()>`; identical module names do not make these types compatible. `C/Cargo.toml:155-225`; `H/crates/vmlab-hv-devices/src/lib.rs:17-18`.

F2. vmlab's `BusDevice` only dispatches offset reads/writes; `PciDevice` supplies config/BAR access and descriptions. It has no crosvm allocator, fork, keep-fd, or VM-memory IPC lifecycle. IRQ adapters target `IrqLine` and `MsiSink`. `H/crates/vmlab-hv-devices/src/lib.rs:20-68`.

F3. vmlab's `VirtioDevice::activate` takes rust-vmm queues and a `VirtioInterrupt`; notifications are method calls. Its `Snapshot` saves bytes through `&self`. crosvm uses different queues/readers/writers, interrupt/event objects, worker lifecycle, and mutable snapshot methods. `H/crates/vmlab-hv-devices/src/lib.rs:70-114`; `C/devices/src/virtio/gpu/mod.rs:84-95,1856-1883,1939-1955`; `C/devices/src/suspendable.rs:20-49`.

All adaptation ranges below are static estimates of new/rewritten Rust lines, not measured implementation effort or elapsed-time estimates. They exclude full S1 acceptance, QEMU-parity fixes, and accelerated/physical-device snapshot implementations unless explicitly included. Shared support overlaps; ranges must not be blindly summed.

## Virtio-gpu and rutabaga

F4. Internal dependencies are `base` events, descriptors, Tube IPC, WaitContext, WorkerThread, volatile memory, and serde helpers; `data_model`; `sync`; `gpu_display`; `hypervisor::MemCacheType`; `vm_control`; crosvm `vm_memory` and udmabuf; `snapshot`; and `serde_keyvalue`. Devices abstractions include Queue/DescriptorChain/Reader/Writer/Interrupt, resource_bridge, SharedMemoryMapper/Region/PrepareType, and PCI address. No direct `cros_async`, `resources`, or BusDevice import occurs in the six GPU files. `C/devices/src/virtio/gpu/mod.rs:21-96`; `virtio_gpu.rs:18-85`; `parameters.rs:15-21`; `protocol.rs:19-42`.

F5. Blob resources need host-visible shared-memory aperture mapping, cache/protection choices, and descriptor/Vulkan/external mapping callbacks. Current vmlab `VirtioDevice` lacks these hooks; this needs a transport/VMM seam, not just queue adaptation. `C/devices/src/virtio/gpu/mod.rs:1901-1927`; `virtio_gpu.rs:1188-1259`; `H/crates/vmlab-hv-devices/src/lib.rs:79-106`.

F6. Renderer initialization and command processing belong on the same worker thread. crosvm initializes before activation to avoid guest-driver timeouts. `C/devices/src/virtio/gpu/mod.rs:1843-1853`. Its snapshot path explicitly supports only 2D, assumes synchronous requests, and asserts that imported display resources are absent. `mod.rs:1930-1938`; `virtio_gpu.rs:153-155`.

Disposition: selectively lift protocol, EDID, command/resource handling; depend on rutabaga. This follows D24. Estimated adaptation: 2-4 kLOC for queues/memory/interrupts, worker/control simplification, descriptors, mapping, and lifecycle. Basis: the 4,008-line wrapper/resource-manager pair contains most host integration. Accelerated snapshot parity is additional, unresolved work. QEMU-compatible VGA, EDID, features, PCI layout, and aperture still require verification: vmlab selects `virtio-vga-gl` on q35 and Venus/blob/4G hostmem for Vulkan. `V/src/qemu/cmdline.rs:785-800`.

### Current rutabaga home, publication, and native libraries

F7. Rutabaga is now a separate Magma GPU repository, not a crosvm subtree. Registry API checks returned non-yanked `rutabaga_gfx` 0.1.85, BSD-3-Clause; crosvm pins `=0.1.80`. Published tarballs were inspected in memory without extraction. `C/Cargo.toml:225`; https://crates.io/api/v1/crates/rutabaga_gfx ; `R/Cargo.toml:10-18`; `R/LICENSE:3-27`.

F8. Current Rust dependencies include cfg-if, libc, log, remain, serde/json, thiserror, zerocopy, and `magma-gpu`; Vulkano is optional and marked prototype. There is no crosvm `base`/`vm_memory` dependency in this manifest. This is a usable crate boundary, not a reason to import the whole crosvm workspace. `R/Cargo.toml:20-42`.

F9. Virgl/virgl2 and Venus dispatch through VirglRenderer. Cross-domain is a separate Rust component for resource sharing across domains. gfxstream-vulkan uses Gfxstream; Venus and gfxstream Vulkan are different capsets/backends. `R/src/rutabaga_core.rs:330-355`; `R/src/cross_domain/mod.rs:5-15`.

F10. Published 0.1.80 probes `virglrenderer >=1.0.0`; published 0.1.85 and pinned upstream require `>=1.3.0`. Both are build-time pkg-config requirements, not bundled renderer builds. The README's 1.0.1 example is insufficient for 0.1.85. https://docs.rs/crate/rutabaga_gfx/0.1.80/source/build.rs:16-23 ; https://docs.rs/crate/rutabaga_gfx/0.1.85/source/build.rs:16-23 ; `R/README.md:47-64`.

F11. The `gfxstream` feature links `gfxstream_backend`, discovered via pkg-config or GFXSTREAM_PATH variants. The stable pkg-config path also probes aemu_base/host_common/logging/snapshot and Linux libdrm, then selects a C++ runtime. The override/unstable paths differ. GBM is separately feature-gated. No minimum gfxstream version is asserted in this build script. `R/build.rs:11-118`. These are external native dependencies even though rutabaga itself is Rust.

F12. Current rutabaga's restore documentation still says VirglRenderer unsupported and Gfxstream work-in-progress. Depending on the newest crate does not close the snapshot gap. `R/src/rutabaga_core.rs:494-514`.

Disposition: depend on the published crate, initially matching crosvm's pin unless an upgrade is explicitly validated. Native renderer build/linking, host driver support, and licence packaging remain deployment dependencies. Native-library availability and accelerated rendering were not checked on this host.

## Headless display and dma-buf delivery

F13. `gpu_display` depends on internal `base`, `linux_input_sys`, `vm_control`, and `sync`, plus rutabaga. Windows adds other internal dependencies. The 184-line stub is not its dependency closure. On Linux the stock build always compiles Wayland C/protocols and links wayland-client, even when the runtime chooses stub. `C/gpu_display/Cargo.toml:20-47`; `build.rs:79-114`.

F14. Scanout flush calls `rutabaga.export_blob`, derives fd plus offset/stride/modifier/dimensions/fourcc, imports it, then flips. CPU readback occurs only if import fails. `C/devices/src/virtio/gpu/virtio_gpu.rs:403-517`. The display import carries a borrowed descriptor. `C/gpu_display/src/lib.rs:357-377`.

F15. Stub import discards that descriptor and all metadata, storing only an import ID. Stub flip returns an already-signaled Waitable. Thus it presents no frame to a consumer and its successful import bypasses CPU fallback. The stub's CPU allocation is not an exported screenshot/frame stream. `C/gpu_display/src/gpu_display_stub.rs:63-105,157-168`; `C/gpu_display/src/lib.rs:233-247`.

Disposition: write a fresh headless sink at the export/import/flip seam, selectively reusing metadata types. Estimated 0.8-1.5 kLOC for basic CPU and dma-buf delivery, owned fd lifetime, scanout/cursor notifications, and consumer completion. The existing private display traits prevent a drop-in external backend without modifying/extracting the seam. `C/gpu_display/src/lib.rs:211,295`. Full RFB, NVIDIA external-only sampling/readback, explicit synchronization correctness, and accelerated snapshots are excluded from that estimate and remain required by D21.

## xHCI

F16. Internal crates are `base`, `sync`, crosvm `vm_memory`, `resources`, and `bit_field`. Devices support includes PciConfiguration/PciDevice/BAR allocation, RegisterSpace/Register, IrqLevelEvent, EventLoop/FailHandle, hub/backend provider, rings, slots, and scatter/gather memory access. No direct cros_async or hypervisor import was found in xHCI. `C/devices/src/usb/xhci/xhci_controller.rs:10-40`; `mod.rs:31-54,109-163`; `scatter_gather_buffer.rs:5-17,40-47`; `xhci_abi.rs:9-17`.

F17. The controller exposes Fresco `1b73:1400`, a 64-KiB 32-bit non-prefetchable BAR, INTx trigger/resample interrupts, one interrupter, 16 slots, and eight USB2 plus eight USB3 ports. These are implementation choices, not generic xHCI requirements. `C/devices/src/usb/xhci/xhci_controller.rs:42,124-134,254-260`; `mod.rs:119-131`; `xhci_regs.rs:8-31`.

F18. A measured black-box mismatch already blocks an unchanged lift under D30. vmlab requests qemu-xhci (`V/src/qemu/cmdline.rs:839-849`). Local QEMU 11.1.2 reports `1b36:000d`, subsystem `1af4:1100`, and BAR0 64-bit. Reproduction, no guest disk or firmware boot:

```sh
qemu-system-x86_64 --version
printf 'info pci\nquit\n' | qemu-system-x86_64 -machine q35 -accel tcg -S -m 128 -nodefaults -display none -device qemu-xhci -monitor stdio
```

This observes identity/BAR type only. QEMU register layout, MSI/MSI-X, USB timing, hotplug, and guest-driver parity were not checked. Changing PCI IDs alone does not prove compatibility.

F19. xHCI has an empty Suspendable implementation; inherited snapshot, restore, sleep, and wake all return errors. A lift supplies neither working snapshots nor quiescing. `C/devices/src/usb/xhci/xhci_controller.rs:335`; `C/devices/src/suspendable.rs:20-49`.

Disposition: lift ring/TRB/slot core under D25; write vmlab PCI/IRQ/memory/event integration and QEMU-compatible presentation. Estimate 1.5-3 kLOC integration/support plus 0.8-2 kLOC controller snapshot/quiesce logic and tests. This excludes behavioural parity fixes and physical peripheral state rollback. A fresh complete controller would discard the measured 7.6-kLOC core; an unchanged lift violates the observed D30 identity/BAR contract.

## Host USB passthrough

F20. `usb_util` depends on internal base/data_model/usb_sys/sync and external libc, remain, static_assertions, thiserror, zerocopy. It uses Linux usbdevfs asynchronous URBs, interface claims, mappings, and fd polling, rather than a demonstrated libusb dependency. `C/usb_util/Cargo.toml:8-17`; `src/device.rs:21-38,269-366,460-473`; `C/usb_sys/Cargo.toml:8-9`.

F21. Host backend accepts an opened File and creates usb_util Device/HostDevice/event handling. Generic backend also depends on Tube, vm_control USB commands/results, event/job helpers, xHCI backend traits, and cros_tracing. It needs replacement control integration for vmlab's typed add/del path. `C/devices/src/usb/backend/host_backend/host_backend_device_provider.rs:19-32`; `host_device.rs:10-43,125`; `C/devices/src/usb/backend/device_provider.rs:13-67`; `utils.rs:67-106`.

F22. Excluding FIDO leaves at most 2,546 backend lines before pruning generic FIDO enum/dispatch/error references. The actual host-only candidate with xHCI/util/sys is 12,499 Rust lines, plus 1,320 shared register/event helpers if retained. This follows the measured table, not a 564-line host_backend-only estimate. FIDO references: `C/devices/src/usb/backend/device.rs:28-51`; `transfer.rs:12`.

F23. Claim failure releases claimed interfaces but does not rebind detached host drivers. The source explicitly documents that USBDEVFS_CONNECT is never issued. Lifting this behaviour carries an observable host-ownership issue. `C/devices/src/usb/backend/host_backend/host_device.rs:138-177`.

Disposition: selectively lift usb_util, usb_sys, host backend, and necessary generic endpoint/transfer handling; prune FIDO. Estimate 0.8-1.8 kLOC replacement/shims for fd/ioctl/mmap/mutex types and control/event plumbing, overlapping xHCI support. External-state snapshot semantics and host-driver recovery are additional work. Alternative USB crates and real device attach/detach were not checked.

## VFIO PCI

F24. `devices/src/vfio.rs` is ioctl/container/group/device plumbing, not the PCI model. The latter adds another 2,240 lines. Low-level dependencies include base ioctl/fd/Event/error types, data_model flexible arrays, hypervisor Vm/DeviceKind, resources address allocation, sync, vfio_sys, and IommuDevType. `C/devices/src/vfio.rs:22-55,146-151,1017-1043`; `C/vfio_sys/Cargo.toml:8-10`.

F25. PCI integration adds base memory mappings, WaitContext/worker/Tube, hypervisor cache types, resources SystemAllocator, vm_control memory/control/hotplug IPC, PCI config/MSI/MSI-X/PM/ACPI machinery, IrqLevelEvent, and Suspendable. No direct cros_async import appears in these two VFIO files. `C/devices/src/pci/vfio_pci.rs:14-87`. BAR accesses intercept MSI-X tables/PBA, so blindly forwarding BARs is insufficient. `:2031-2057`.

F26. vmlab PciDevice can dispatch config/BAR access, but direct BAR mapping and DMA setup need VMM ownership: crosvm commits mappings after guest BAR changes. Its low-level API takes a hypervisor VM to register VFIO groups. `C/devices/src/pci/vfio_pci.rs:1949-1956`; `C/devices/src/vfio.rs:1017-1028`. Keeping this host/KVM plumbing outside vmlab's KVM-agnostic device crate is consistent with `H/crates/vmlab-hv-devices/src/lib.rs:3-8`.

F27. The VFIO PCI Suspendable implementation only implements worker sleep/wake; snapshot/restore inherit errors. It does not save physical device state. `C/devices/src/pci/vfio_pci.rs:2119-2138`; `C/devices/src/suspendable.rs:20-33`.

F28. Registry checks returned vfio-ioctls 0.10.0 and vfio-bindings 0.7.0, both non-yanked and `Apache-2.0 OR BSD-3-Clause`. They provide ioctl wrappers/generated bindings without crosvm base/resources/vm_control dependencies. `F/vfio-ioctls/Cargo.toml:1-40`; `F/vfio-bindings/Cargo.toml:1-20`; https://crates.io/api/v1/crates/vfio-ioctls ; https://crates.io/api/v1/crates/vfio-bindings .

F29. rust-vmm supplies container/group/device handling, guest-memory DMA maps, region access, eventfd IRQ/MSI/MSI-X operations, and optional iommufd/cdev support. Current source also exposes migration/dirty-logging APIs; hardware support is not implied. `F/vfio-ioctls/src/vfio_device.rs:288,421-520,1336,1564-1721,1781-1807,1855-2167`. It is not a complete guest PCI device or QEMU compatibility layer.

F30. vm-memory 0.18 matches vmlab, but default KVM dependencies are kvm-ioctls 0.25 and kvm-bindings 0.14 versus vmlab's 0.21/0.11. Passing VmFd/DeviceFd across these crate versions requires explicit integration work or version alignment, not a type cast. `F/vfio-ioctls/Cargo.toml:18-33`; `H/Cargo.toml:21-23`.

Disposition: depend on rust-vmm vfio-ioctls/bindings and write the vmlab PCI/VMM adapter, rather than lifting crosvm's 5,269-line VFIO stack and dependency closure. Estimate 2-4 kLOC fresh integration for config/BAR masking/mapping, IRQ routing, resource lifetime, and DMA setup, excluding GPU quirks, migration, and full hardware acceptance. A crosvm lift would still need an estimated 2-3.5 kLOC rewritten host/PCI integration. Basis: F24-F30 expose those couplings and the reusable crate APIs. Compile compatibility and actual passthrough were not checked.

## Licence constraints and verification limits

GPU files, stub, xHCI, USB backend/util, VFIO wrapper/PCI, and vfio_sys carry ChromiumOS copyright plus BSD-style LICENSE pointers at lines 1-3. Root `C/LICENSE:3-27` supplies BSD-3 conditions; manifests explicitly identify BSD-3-Clause, including `C/gpu_display/Cargo.toml:6`, `usb_util/Cargo.toml:6`, and `vfio_sys/Cargo.toml:6`. Retain notices and list every lifted file in THIRD_PARTY_NOTICES under D7. Dependencies with their own licences remain dependencies; renderer/transitive licence packaging was not audited. `H/.scratch/plans/vmlab-hypervisor/SPEC.md:72-79`.

Verified: source counts, cited integration points, published crate metadata/build scripts, and QEMU xHCI PCI identity/BAR type. Not checked: builds, just ci::check, S1/S2, native-library linking, guest boots, rendering, USB hardware, VFIO hardware, performance, and snapshot execution. This is research only; no implementation or shared-interface change was made.

## Open questions this raises

- Q1. What exact QEMU PCI/config/register/interrupt and VGA/EDID/capset contract must the lifts reproduce for existing templates? The observed xHCI mismatch needs more than a source-selection decision.
- Q2. What shared VMM callbacks expose GPU blob apertures and VFIO BAR mappings while keeping device models KVM-agnostic?
- Q3. What are the D21 frame fd ownership, synchronization, release, CPU readback, and external-only NVIDIA requirements? The stub answers none of them.
- Q4. Which pinned rutabaga/virglrenderer pair and host GPU matrix will be supported, and how will accelerated snapshots work given explicit upstream restore limits?
- Q5. How do snapshots handle attached physical USB/VFIO state, in-flight requests, and unsupported migration without silently weakening existing snapshot behaviour?
- Q6. What host-driver rebind/reconnect semantics apply after USB claim failures and detach, and which hardware proves them?
- Q7. Should VFIO align workspace KVM crate versions or use a narrower fd bridge/version pin? No compile probe was performed.
