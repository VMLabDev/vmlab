---
id: "08"
decision: D7
---

# How vmlab chooses and finds the viewer

## Question
How is the viewer shipped and launched?

## Answer
A `vmlab-hv-viewer` binary in the vmlab-hypervisor workspace, installed beside vmlab-hv; vmlab finds it on PATH or through a host-config key, adds the machine's hypervisor to its status, and `vmlab console` and `gui = true` launch it for vmlab-hv machines (QEMU machines keep VNC viewers).

## Why
Keeps GPU and windowing dependencies out of the vmlab CLI.

## Consequences
vmlab-hypervisor has no release job yet; shipping the viewer in a release is accepted as later work (manual install for now).
