---
id: "25"
decision: D24
---

# GPU

## Question
virglrenderer bindings, rutabaga, or crosvm's stack?

## Answer
Lift crosvm's virtio-gpu device and use rutabaga_gfx over virglrenderer for virgl and Venus, with GL scanout exported as dma-buf into the frame protocol (D21).

## Why
The only working Rust Venus path. Recommended option, accepted.

## Consequences
- vmlab's Vulkan PCI filter layer and keep-awake helper (ADR-0018) apply unchanged to the hypervisor process.
