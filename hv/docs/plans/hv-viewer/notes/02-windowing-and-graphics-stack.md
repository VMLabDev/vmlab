---
id: "02"
decision: D1
---

# Windowing and graphics stack

## Question
Which windowing and graphics stack displays frames and imports dma-bufs?

## Answer
winit for windows and input, Vulkan through `ash` for rendering, importing guest dma-bufs with VK_EXT_external_memory_dma_buf and VK_EXT_image_drm_format_modifier.

## Why
One path on Wayland and X11, and the most robust for NVIDIA. Handing the dma-buf straight to the Wayland compositor (linux-dmabuf subsurface) is a later optimisation; wgpu has no first-class dma-buf import.

## Consequences
The viewer needs a CPU-upload path for WSL2 (D6) through the same Vulkan renderer.
