---
slug: hv-audio
kind: spec
created: 2026-10-30
---

# Guest audio for vmlab-hv: implementation specification

Traces: `Dn` is a row in this plan's `MAP.md`. `HV-Dn` is a row in
`../vmlab-hypervisor/MAP.md`. Research is in `notes/01-audio-research.md`.

## Problem

A person using a `vmlab-hv` desktop guest hears nothing and cannot give the
guest a microphone. vmlab launches QEMU with no audio device and treats audio as
out of scope (`/home/wil/dev/vmlab/docs/vmlab-prd.md:850, 860`). The owner wants
guest audio with the lowest latency the stack allows, handed straight to the
host's PipeWire.

## Solution

`vmlab-hv` gains guest audio devices and plays their PCM streams directly into
the user's PipeWire session from the hypervisor process. There is no helper
daemon and no viewer in the path (D3). An Intel HDA controller covers Windows and
Linux guests with inbox drivers, and virtio-snd follows as the Linux fast path
(D2). A host-audio seam keeps the PipeWire crate in one place and adds a
PulseAudio backend for WSL2 (D3, D4). Audio and microphone are per-machine
opt-ins, both off by default (D1, D7). Delivery is in phase 2 of the hypervisor
(D5), against a latency target of 20 ms guest-to-speaker on a tuned host (D6).

## User stories

1. As a vmlab user, I set `audio = true` on a `vmlab-hv` machine, and the Windows or Linux guest's sound plays on my host with no driver installed in the guest.
2. As a vmlab user, each audio-enabled machine appears as its own PipeWire node named after its lab and machine, so I can set its volume, mute it or route it with `wpctl` or pavucontrol.
3. As a vmlab user, I set `microphone = true` as well, and the guest records from my host's default source. Without it, no capture stream exists in PipeWire for that machine.
4. As a vmlab user on WSL2, audio works through WSLg's PulseAudio server with no configuration.
5. As a vmlab user, an online snapshot of an audio-enabled machine restores with audio resuming. At most one period glitches.
6. As a vmlab user, `audio = true` on a QEMU machine fails validation with a message saying the setting needs `hypervisor = "vmlab"`.
7. As the owner, I can measure guest-to-speaker latency on my host and see it against the 20 ms target.

## Implementation decisions

### Machine description (`vmlab-hv-proto`, D7)

`MachineDescription` gains an optional `audio` section. When it is absent, no audio device exists. Its fields are:

- `device`: `hda` or `virtio_snd`.
- `microphone`: bool.
- `node_name`: the PipeWire and PulseAudio node name, which vmlab sets to `vmlab.<lab>.<machine>`.
- `node_description`: human-readable, for mixers.

The wire shape gets pinned by a test alongside the existing ones.

### Host-audio seam (`vmlab-hv-devices`, module `audio`; D3, D4)

A `HostAudio` trait opens streams for a device. `open_playback(format)` returns a sink the device pushes PCM periods into. `open_capture(format)` returns a source the device pulls periods from. Formats are interleaved S16LE or F32LE, 1 or 2 channels, at the rates the guest device offers. The trait reports host-side latency so devices can return it to the guest (virtio-snd `latency_bytes`, HDA position).

Backends, each in its own module:

- `pipewire`: the `pipewire` crate (MIT), pinned to one version behind the cargo feature `pipewire`. It runs one `pw_stream` per direction on the PipeWire main loop thread, with `node.name`, `node.description` and `node.latency = 256/48000`. PCM passes through a lock-free single-producer single-consumer ring between the device and the PipeWire thread, so the vCPU path never blocks on PipeWire. Underrun plays silence, and overrun drops the oldest period.
- `pulse`: `libpulse-binding` behind the cargo feature `pulse`, with the same ring design.
- `null`: discards playback and supplies silence for capture.
- `recording`: test only, behind `cfg(test)` or a `testing` feature. It records every period it receives with a monotonic timestamp, and plays a scripted source for capture (seam A1).

Selection at machine start, in this order (D4):

1. PipeWire, if its socket under `XDG_RUNTIME_DIR` is reachable.
2. PulseAudio, if `PULSE_SERVER` is set.
3. Otherwise null, with one warning line on stderr naming why.

The capture stream is opened only when `microphone` is true (D1).

`vmlab-hv` inherits `XDG_RUNTIME_DIR` and `PULSE_SERVER` from the vmlab lab daemon that spawns it (D3).

### Intel HDA (`vmlab-hv-devices`, module `hda`; D2)

The HDA model is a PCI function implementing `PciDevice`, built from the public Intel HDA specification 1.0a:

- **Controller:** an ICH9 HDA controller, PCI 8086:293e, at the slot q35 uses for it.
- **Global registers:** the global capability, control and status registers, and the interrupt control and status registers.
- **Command transport:** CORB and RIRB DMA with their pointers, plus the immediate command interface.
- **Streams:** one output and one input stream descriptor. Each walks its buffer descriptor list by DMA and reports position through LPIB, the DMA position buffer and the wall clock.
- **Interrupts:** MSI.

**Codec:** a duplex codec with an audio function group:

- an output converter with volume and mute amplifiers, and a line-out pin
- an input converter with gain and mute, and a line-in pin
- stereo, 16-bit, at 44.1, 48 and 96 kHz

The codec answers the parameter, connection, amplifier, pin-control, power-state, EAPD and unsolicited-response verbs the HDA spec defines. Volume and mute apply in the device before PCM reaches the host seam.

The codec's vendor and widget layout may follow the black-box observation in `notes/01` (vendor 0x1af40022, the widget map listed there). D30's identity rule does not bind it, because vmlab's QEMU templates carry no audio device, but matching costs nothing.

Stream timing comes from the host seam: the device advances the guest-visible position as the host consumes periods, so position never runs ahead of real playback. LPIB and the wall clock stay monotonic across pause and restore.

### virtio-snd (`vmlab-hv-devices`, module `virtio/sound`; D2)

virtio-snd follows VIRTIO 1.3 chapter 5.14, as a `VirtioDevice` behind the existing virtio-pci transport:

- **Device:** device type 25, PCI 1af4:1059.
- **Queues:** control, event, tx and rx.
- **Streams:** one playback stream, plus one capture stream only when `microphone` is true. It offers no jacks and no channel maps beyond stereo.
- **PCM:** the full substream state machine (SET_PARAMS, PREPARE, START, STOP, RELEASE). Each tx buffer completes with `latency_bytes` from the host seam.

### Snapshots (HV-D14, HV-D32)

Both devices implement `Snapshot`.

| Device | State saved |
|---|---|
| HDA | All controller and stream registers, CORB/RIRB pointers, BDL position and wall clock, codec widget state (amplifiers, pin controls, power states, unsolicited-response enables) |
| virtio-snd | Substream states and parameters, queue state, in-flight buffers |

Host-side state is not saved. On restore the device reopens its host streams, discards one period of capture, and resumes playback at the saved position. One period may glitch.

### vmlab-side interface (D7; vmlab PR)

- **Lab file:** a per-machine `audio = true` and `microphone = true`, both default off. `microphone` requires `audio`.
- **Validation:** refuses both on QEMU machines.
- **Machine description:** the resolver fills the audio section with `device = "hda"` for Windows profiles. Linux profiles also get `hda` until virtio-snd ships, and `virtio_snd` after that.
- **Environment:** the lab daemon passes `XDG_RUNTIME_DIR` and `PULSE_SERVER` to `vmlab-hv`.
- **PRD:** §11's audio non-goal is amended.

## Testing decisions

- **A1, host-audio seam, `recording` backend (new).** Device and integration tests assert exactly which PCM periods the guest produced, in order, with timing. No sound server is involved, and the tests run in the merge bar.
- **A2, device tests at the guest interface.** HDA is driven through its PCI config space and MMIO registers: CORB/RIRB verb round trips, codec parameter reads, a BDL DMA playback that lands in the `recording` backend, capture from a scripted source, position monotonicity across pause, and a snapshot round trip. virtio-snd is driven through its control and PCM queues, covering the full substream state machine, `latency_bytes` and a snapshot round trip. These tests are part of the merge bar.
- **A3, S1 with a real Linux guest.** The harness boots a Linux guest with `audio` set, runs `aplay` with a known tone file, and asserts that the `recording` backend received it intact. It runs `arecord` against a scripted source and asserts that the guest captured it. With `microphone` false it asserts that no capture stream was opened. It needs `/dev/kvm` and skips with a reason when that is missing.
- **A4, loopback latency harness against real PipeWire (new, outside the merge bar).** The guest plays a click at a time it records. The host captures the machine's PipeWire node output with a monitor stream and timestamps the click. The harness reports guest-to-speaker latency against D6: at most 20 ms with quantum 256 at 48 kHz, and at most one quantum over PipeWire's graph latency at any quantum. Guest and host timestamps are related through the hypervisor's clock.

## Out of scope

- Legacy AC97, ES1370 and SB16 devices (phase 4).
- virtio-snd for Windows guests until virtio-win ships a driver.
- Audio through the viewer process, and audio over a network.
- Surround, jacks and channel maps beyond stereo.
- QEMU-side audio for vmlab's QEMU machines.

## Open questions

- Whether `pipewire` 0.10 exposes everything the ring design needs, such as buffer requests and real-time thread priority, or whether the backend drops to `pipewire-sys` for some calls. The first implementation milestone should answer it.
- How A4 relates the guest's play timestamp to host capture time with sub-millisecond accuracy (candidates: a kvmclock read from the guest, or a marker written to a virtio-serial port). This is spec work for the harness.
