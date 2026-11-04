---
slug: hv-audio
status: specified
created: 2026-10-30
---

# Guest audio for vmlab-hv, played straight into PipeWire

## Destination
A `vmlab-hv` machine's guest audio plays on the host with the lowest latency the
stack allows, by handing the guest's PCM stream to PipeWire on the host without
an intermediate transcoding or network hop. Linux and Windows guests are
covered, and the hypervisor's snapshots, phases and clean-room rule still hold.

## Grounding
- vmlab treats audio as out of scope today and QEMU is launched with no audio device (`/home/wil/dev/vmlab/docs/vmlab-prd.md:850, 860`; inventory "Audio: not found").
- Raised as part of the native viewer: `.scratch/plans/hv-viewer/INTAKE.md`.
- Decisions from the vmlab-hypervisor map (its ids, not this map's) that bind this work: one process per machine (D5), rust-vmm foundation (D7), own device-state snapshots (D14), QEMU-identical guest-visible hardware for templates (D30), phase order (D4), WSL2 first-class.

## Decisions
| ID | Decision | Struck by | Recorded in |
|----|----------|-----------|-------------|
| D1 | Microphone capture in, opt-in per machine, default off | | `notes/02-microphone.md` |
| D2 | Guest audio device | | `notes/03-guest-device.md` |
| D3 | Host path and who plays | | `notes/04-host-path.md` |
| D4 | WSL2 audio | | `notes/05-wsl2.md` |
| D5 | Phase | | `notes/06-phase.md` |
| D6 | Latency target | | `notes/07-latency.md` |
| D7 | vmlab surface | | `notes/08-vmlab-surface.md` |

## Frontier
- [x] Q1 (research) [status: answered] — Guest devices, host paths to PipeWire, latency and licences for guest audio.
- [x] ~~Q2 (decision) [blocked_by: ~~Q1 (notes/01)~~] — Guest audio device(s) for Linux and Windows guests.~~ → D2
- [x] ~~Q3 (decision) [blocked_by: ~~Q1 (notes/01)~~] — Host path: in-process PipeWire client, or vhost-user sound backend?~~ → D3
- [x] ~~Q4 (decision) [blocked_by: ~~Q1 (notes/01)~~] — Who plays the audio: the hypervisor directly into the user's PipeWire session, or the viewer?~~ → D3
- [x] ~~Q5 (decision) [blocked_by: ~~Q1 (notes/01)~~] — WSL2 path, where WSLg serves PulseAudio rather than PipeWire.~~ → D4
- [x] ~~Q6 (decision) — Microphone (host to guest) in or out?~~ → D1
- [x] ~~Q7 (decision) [blocked_by: Q2] — Which phase delivers audio?~~ → D5

- [x] ~~Q8 (decision) — Latency target and the guest/host buffer sizes that meet it.~~ → D6
- [x] ~~Q9 (decision) — vmlab surface: how a machine turns audio on, and the default.~~ → D7

## Fog

## Accepted unknowns
- The capture stream's absence from PipeWire when the microphone is off (D1): a requirement, checked by test
- HDA model internals (D2) and the device-state sections for HDA and virtio-snd: spec work from the public specs
- Pinning and feature-gating the `pipewire` crate, and inheriting XDG_RUNTIME_DIR from the lab daemon (D3): implementation
- Backend selection order PipeWire, then PulseAudio, then null with a warning (D4): recorded here, needs no question
- The loopback harness's design (D6): spec work
- Machine-description fields and the vmlab PRD amendment (D7): delivered with the vmlab-side PR

## Out of scope
- Legacy AC97 and SB16 devices (phase 4 of vmlab-hypervisor)
- virtio-snd for Windows guests until a virtio-win driver exists
- Audio through the viewer process
