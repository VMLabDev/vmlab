---
id: "10"
decision: D10
---

# Agent channel

## Question
How do the agent and container control reach the guest?

## Answer
virtio-serial multiport with named ports (`vmlab.agent.0`, `vmlab.ctl.0`) exposed as host unix sockets; legacy guests keep a 16550 on COM1.

## Why
Every baked template's agent and cinit already speak it; changing transport belongs to the agent project. Recommended option, accepted.

## Consequences
- Socket semantics must match QEMU's `server=on,wait=off` chardev behaviour that vmlab and the test fake rely on (`fake.rs:485-505`).
- The hypervisor binds these sockets itself (`mod.rs:121-134`).
