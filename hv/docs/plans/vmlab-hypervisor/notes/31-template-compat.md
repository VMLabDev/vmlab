---
id: "31"
decision: D30
---

# Template compatibility

## Question
Must templates built under QEMU boot unchanged on the new hypervisor?

## Answer
Yes as the default: machine models present QEMU-identical guest-visible hardware (PCI IDs and slots, ACPI, SMBIOS, firmware interfaces), reproduced from specs and observation, not QEMU code. The owner will consider a break case by case when there is a stated reason.

## Why
Windows installs are expensive and OCI-distributed, and a chipset or storage-controller mismatch is INACCESSIBLE_BOOT_DEVICE on legacy guests. Owner accepted with an explicit escape.

## Consequences
- Any proposed break is raised to the owner with its reason and the templates it affects; it is never made silently.
- D26's hardware dumps are the check that compatibility holds.
