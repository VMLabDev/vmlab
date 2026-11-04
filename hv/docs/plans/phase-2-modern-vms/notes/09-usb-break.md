---
id: "09"
decision: D6
---

# Default USB (approved HV-D30 break)

## Question
ICH9 UHCI/EHCI as QEMU's -usb, or the tablet on xHCI?

## Answer
Approved break of HV-D30: vmlab-hv VMs have no ICH9 UHCI/EHCI; the absolute tablet sits on an xHCI controller (the same controller phase 2f extends for host passthrough).

## Why
USB is not boot-critical; Windows 8+ and modern Linux have inbox xHCI drivers and enumerate the new controller; windows-legacy (Windows 7) is phase 4. Saves an estimated 5-10 kLOC. Owner approved the recommended break.

## Consequences
- Existing templates see a one-time USB device change on first boot under vmlab-hv.
- The differential suite records the missing 1d.x functions and the xHCI as a known, approved difference.
- The tablet must be available from 2b (input), so a minimal xHCI with an HID tablet lands before 2f.
