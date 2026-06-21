# riscv64-ubuntu

A minimal RISC-V (riscv64) lab: one Ubuntu guest on a NAT'd segment with a
host port-forward to SSH. Demonstrates running an emulated riscv64 VM under
TCG on an x86 host.

## Prerequisites

Host needs `qemu-system-riscv64` (QEMU ≥ 8.1) and the riscv64 UEFI firmware
(`qemu-efi-riscv64` on Debian/Ubuntu, or edk2's `RISCV_VIRT_CODE.fd` /
`RISCV_VIRT_VARS.fd`).

Build the riscv64 Ubuntu template first (from the
[vmlab-templates](https://github.com/wiltaylor/vmlab) repo):

```sh
cd vmlab-templates/ubuntu-riscv64
vmlab template build          # downloads the cloud image, slow under TCG
vmlab template list           # confirm riscv64/ubuntu-24.04 landed in the store
```

## Run

```sh
vmlab up                          # boots the guest (TCG — give it a minute or two)
ssh vmlab@localhost -p 12322      # password: vmlab
vmlab down
```

The provision script (`scripts/setup.wisp`) waits for the guest agent and
logs the guest's `uname -m` (should print `riscv64`) and os-release. Watch it
with `vmlab logs`.

> Note: on x86 hosts there is no KVM for riscv64, so the VM runs under TCG
> emulation and is slow. That's expected.
