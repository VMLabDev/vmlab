---
id: "27"
decision: D26
---

# Differential suite

## Question
What is compared against QEMU, at what granularity, per phase?

## Answer
Tiered. Every phase: guest-visible hardware dumps (lspci, ACPI tables, dmidecode, CPUID) diffed against QEMU, plus kvm-unit-tests. TCG: per-instruction lockstep (D22). Legacy: boot each template to agent-ready plus screenshot and OCR comparison.

## Why
Hardware dumps are what make D30's compatibility checkable. Recommended option, accepted.

## Consequences
None beyond those already on the map.
