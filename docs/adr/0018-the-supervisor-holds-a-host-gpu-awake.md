# ADR-0018: The supervisor holds a host GPU awake between labs

- **Status**: Accepted
- **Date**: 2026-10-25
- **Supersedes**: nothing
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0010](0010-the-supervisor-owns-the-template-store.md)

## Context

A `virgl` or `vulkan` VM renders on a host GPU (PRD §5.2). On a host with
runtime power management on (`power/control = auto`), the driver powers an
idle GPU down after a few seconds. Between one lab's `vmlab down` and the next
`vmlab up`, no process uses the GPU, so the driver suspends it. On NVIDIA the
resume boots the GSP firmware again. On at least one laptop that resume failed
(`Xid 119`, then `Xid 79`), and the GPU was gone until reboot. QEMU then
aborted inside `libnvidia-eglcore` about 40 seconds after vmlab reported the VM
up (VMLabDev/vmlab#170).

vmlab cannot stop the driver from suspending without root: `power/control` and
the `nvidia` module options are root-owned. It can make sure the GPU never has
an idle gap between labs, by keeping a client on it.

The prior art is `labs/zeus/keepawake/` in vmlab-win3d, measured on an RTX
4060 with fine-grained runtime D3. An EGL context with no work did not hold the
GPU: the driver suspended it after about 21 s with the context still open. The
same context clearing a 16x16 renderbuffer every 100 ms held it `active`
throughout, for a whole acceptance run.

## Decision

**The supervisor runs one keep-awake helper per held host GPU, started the
first time a `virgl`/`vulkan` VM starts on that GPU, and kept until the
supervisor stops.**

- The helper is `src/qemu/gpu_keepawake.c`: the vmlab-win3d loop (an OpenGL ES
  context on the GPU through EGL's device platform, a 16x16 clear and
  `glFinish` every 100 ms). It is C because it `dlopen`s libEGL, which the
  crate's `unsafe_code = "deny"` rules out in Rust. `build.rs` compiles it the
  way it compiles the Vulkan PCI-filter layer, and the binary embeds it. The
  supervisor writes it into a content-addressed directory under the data
  directory. It declares its own EGL types, so building vmlab needs no EGL
  headers, and a host with no libEGL gets a message naming it.
- The supervisor owns it, not the lab daemon. The gap to close lies between
  two lab daemons' lifetimes. A lab daemon exits with its lab.
- The lab daemon asks over `gpu.keepawake` before it spawns the emulator,
  because only the start knows which GPU a VM resolved to. The supervisor
  answers with the helper's pid and reuses a running helper. A helper that
  cannot open the GPU is a warning in the lab daemon's log, never a refused
  start, because the helper only narrows a window.
- The helper polls `getppid()` each interval and exits once the supervisor is
  no longer its parent. That way a killed supervisor does not leave the GPU
  held. `PR_SET_PDEATHSIG` fires when the spawning *thread* exits, and tokio
  can retire that thread, so it is not used.
- The host config decides it per GPU: `gpu "<name>" { keepawake = "auto" |
  "on" | "off" }`. `auto` (the default) holds an NVIDIA GPU that can
  runtime-suspend. That means `power/control` is `auto` and the driver's
  `/proc/driver/nvidia/gpus/<addr>/power` does not say `Runtime D3 status:
  Disabled`. NVIDIA leaves `power/control` at `auto` on desktop cards whose
  runtime D3 it has disabled, so `control` alone would hold every desktop
  card for nothing. The decision is computed in `hostgpu::render_plan` as a
  value (ADR-0003). `vmlab status -v` shows it as `keepawake=`.
- Two cheaper checks sit beside it in `hostgpu`. A GPU whose
  `power/runtime_status` is `error` is refused by validation and at start.
  An NVIDIA GPU that can runtime-suspend earns one warning naming the root
  fixes (a udev rule setting `power/control=on`, or
  `NVreg_DynamicPowerManagement=0x00`).

## Consequences

- `down` then `up` no longer crosses a suspend while the supervisor runs. That
  is the sequence that lost the GPU.
- A held GPU stays powered while the supervisor runs, labs or no labs. On a
  laptop that costs idle battery until `vmlab daemon stop`. `keepawake = "off"`
  opts out.
- A supervisor restart drops the hold until the next VM start on that GPU. A
  VM still running then shows `keepawake=lost`. The root fixes the warning
  names are the only way to cover a supervisor restart and a reboot.
- `auto` covers NVIDIA only, because the observed failure is the NVIDIA GSP
  resume. Other drivers can be held with `on`. The helper works on any EGL
  driver with the device platform.
- Template builds do not ask for a hold. A build VM's render GPU can still
  suspend between builds.
- The helper costs about 6 MiB of video memory and one `glFinish` every
  100 ms per held GPU, and it shows in `nvidia-smi` as a graphics process.
