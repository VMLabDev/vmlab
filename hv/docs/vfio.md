# Test PCI passthrough

Use a spare PCI function that the workstation owner explicitly designates.
No device has been selected by this work package. Do not use a display GPU,
a storage controller carrying host filesystems, or the owner's keyboard or mouse
controller. The test never changes driver bindings.

## Prepare the designated function

Set `VMLAB_HV_TEST_VFIO_DEVICE` to its canonical, lowercase PCI address,
including the domain, for example `0000:65:00.0`. The example is a format
illustration, not a device recommendation.

Inspect the device and its IOMMU group before changing any binding:

```sh
BDF="$VMLAB_HV_TEST_VFIO_DEVICE"
[[ "$BDF" =~ ^[0-9a-f]{4}:[0-9a-f]{2}:[01][0-9a-f]\.[0-7]$ ]]
lspci -nnk -s "$BDF"
GROUP=$(readlink -f "/sys/bus/pci/devices/$BDF/iommu_group")
test -n "$GROUP"
ls -l "$GROUP/devices"
```

The host needs enabled IOMMU and interrupt remapping. Stop if the group contains
an essential host device. VFIO requires every group member to be detached from
its host driver or bound to VFIO. Obtain separate owner authorization before
changing any additional function, including a GPU's audio function. Do not use
ACS overrides or no-IOMMU mode.

After confirming that the selected function is idle and safe to detach, run
these commands on the host. Record the original driver first:

```sh
ORIGINAL_DRIVER=$(basename "$(readlink -f "/sys/bus/pci/devices/$BDF/driver")")
sudo modprobe vfio-pci
printf '%s' vfio-pci | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
printf '%s' "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind"
printf '%s' "$BDF" | sudo tee /sys/bus/pci/drivers_probe
readlink "/sys/bus/pci/devices/$BDF/driver"
ls -l "/dev/vfio/$(basename "$GROUP")"
```

The driver link must end in `vfio-pci`. Grant the test account access to the
group's VFIO node through the host's device-permission policy. The VMM refuses
other bindings and never unbinds, rebinds, or changes IOMMU groups.
Passthrough pins all guest RAM, and the kernel charges it to the account's
locked-memory limit (RLIMIT_MEMLOCK). The VMM raises its soft limit to the hard
limit by itself. When the hard limit is below the guest RAM it refuses to start
and names the limit. Check the hard limit in KiB with `ulimit -Hl`. This test
needs 4 GiB.

## Run the S1 acceptance test

Install matching 4 MiB OVMF CODE and VARS files in the system firmware
directory or set `VMLAB_HV_FIRMWARE_DIR` to the directory containing them.
Provide the Ubuntu 26.04 template under
`~/.local/share/vmlab/templates/x86_64/ubuntu-26.04/`, or set
`VMLAB_HV_TEST_VM_DISK` to its `disk.qcow2`. The test writes to a disk overlay
and makes a private copy of the VARS file for each VM start.

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env RUST_TEST_THREADS=1 cargo test -p vmlab-hv-tests --test vfio -- --nocapture
```

The test boots the designated function at guest `00:06.0`, checks `lspci -n`
against its host vendor and device IDs, and checks that the control socket
refuses snapshot save and load. It reboots the guest, checks the reset event,
the new boot ID, and the PCI identity, then powers off from the guest. A second
VM process starts with the same device and disk overlay, checks the PCI
identity again, and quits through the control socket. Both exits must be
successful and free of VFIO teardown errors. Without the device variable, it
prints a skip reason before accessing hardware. With a device specified,
missing KVM, firmware, or template assets fail the test.

For hardware acceptance, also exercise the function with its normal guest
driver. That step covers rendering, DMA, and interrupts used by the workload.

After every VM has exited, restore the original host driver if appropriate:

```sh
printf '%s' "$BDF" | sudo tee "/sys/bus/pci/devices/$BDF/driver/unbind"
printf '%s' "$ORIGINAL_DRIVER" | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
printf '%s' "$BDF" | sudo tee /sys/bus/pci/drivers_probe
printf '\n' | sudo tee "/sys/bus/pci/devices/$BDF/driver_override"
lspci -nnk -s "$BDF"
```

## Machine description and limits

```json
"vfio": [
  {"host": "0000:65:00.0", "pci": {"bus": 0, "device": 6, "function": 0}}
]
```

Several functions of one card are separate entries that share a guest slot.
The card goes at function 0, and each other function keeps its host function
number:

```json
"vfio": [
  {"host": "0000:05:00.0", "pci": {"bus": 0, "device": 4, "function": 0}},
  {"host": "0000:05:00.1", "pci": {"bus": 0, "device": 4, "function": 1}}
]
```

The guest then reads the multi-function bit at function 0 and finds the
card's HDMI/DP audio function beside it. vmlab writes these entries for a
`gpu { mode = "passthrough" }` VM from the functions of the host slot that are
bound to `vfio-pci`, or from the host GPU's `passthrough_functions`.

Assignment requires q35 and an explicit, unoccupied bus-zero guest address.
The VMM resets the function at start and VM exit when the kernel can reset it
by itself (`VFIO_DEVICE_FLAGS_RESET`). The audio function of a graphics card
usually cannot be: it has no function-level reset and shares its bus with the
GPU. Such a function is assigned without a reset and the log says so. The VMM
still clears its command register and interrupts. It maps guest RAM through a
VFIO type1 container, and directly maps mmap-capable BAR pages into both KVM
address spaces. Sparse mmap areas are respected. MSI-X table and PBA pages
remain trapped; other registers on those pages use VFIO region access.
Guest BAR and ROM addresses never reach host config space.

The expansion ROM is read on the guest's first access to it and kept for the
life of the machine. The kernel checks the ROM signature through the device on
every read, so the VMM turns memory decoding on for the length of the read.
A graphics card's flash is slow: the first read of an RTX 5090's 512 KiB ROM
takes about 14 seconds, once for each start.

Three extended capabilities are unlinked from the chain the guest sees, and
read as zeros: SR-IOV, ARI and Resizable BAR. The kernel presents SR-IOV's VF
BARs read-only, and EDK2 adds them to its resource plan. On an RTX 5090 that
asks the host bridge for a range it rejects, after which OVMF gives no device
a BAR and the guest finds no disk and no display. ARI names a next function
the guest does not have, and Resizable BAR would change a BAR size the VMM has
fixed.

PCI0's 64-bit window in the ACPI tables grows by the sum of the assigned
64-bit BARs, rounded up to 1 GiB, above its 32 GiB default. Firmware places
the largest BAR first, so a 32 GiB BAR would otherwise push every other
device's 64-bit BAR outside the declared window.

MSI advertises one maskable vector when hardware supports masking. MSI-X
preserves the hardware vector count. Both use eventfds and KVM MSI routing;
masked vectors retain pending events. INTx uses KVM irqfd resampling at the
q35 IOAPIC PIRQ GSI. Legacy PIC-only INTx delivery is not implemented.
Other capability accesses pass through the kernel VFIO config-space filters.
No vendor-specific GPU reset or NVIDIA workarounds are included. An RTX 5090
runs a Windows 11 guest with the NVIDIA driver without them (card 453).

Online snapshot save and load are refused while any VFIO function is assigned.
Disk-only image operations remain independent of assignment.

The implementation follows the Linux VFIO and KVM interfaces:
https://docs.kernel.org/driver-api/vfio.html
https://docs.kernel.org/virt/kvm/api.html
