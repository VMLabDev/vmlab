---
id: "07"
decision: D7
---

# Foundation

## Question
Build on rust-vmm crates, fork crosvm or cloud-hypervisor, or start from bare kvm-bindings?

## Answer
Build on rust-vmm crates (kvm-ioctls, kvm-bindings, vm-memory, virtio-queue, vhost and friends) and write our own VMM and devices, lifting BSD-licensed pieces from crosvm with attribution where useful.

## Why
Takes the well-tested KVM and virtio plumbing without inheriting another project's architecture. All rust-vmm licences are compatible with an MIT project as dependencies (`docs/research/qemu-viability/03-rust-prior-art.md` §6, corrected for the dual-licensed crates). Recommended option, accepted.

## Consequences
- crosvm code lifted into the tree keeps its BSD-3 notice; Apache-2.0-only code (cloud-hypervisor, `acpi_tables`, `kvm-bindings`) may be a dependency but not copied into MIT files without carrying its licence.
- A licence policy for vendored code is needed before any lift; recorded as an accepted unknown for the implementer, see MAP.md.
