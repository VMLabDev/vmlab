---
id: "01"
kind: research
---
# Guest audio research

## Question

What guest audio device(s) should vmlab-hv present (virtio-snd, Intel HDA, legacy
AC97/SB16), what host path should carry PCM to the host's PipeWire (in-process
client, vhost-user daemon, PulseAudio protocol for WSL2), what latency is
achievable, what must snapshots save, and can all of it be implemented clean-room
without QEMU source? Feeds Q2-Q7 of this map. Tags: **F** = fact with source,
**I** = interpretation/estimate, **NC** = not checked.

## Findings

### 1. Guest devices

**virtio-snd.** F: "Sound Device" is chapter 5.14 of the OASIS VIRTIO spec
(present in 1.2 csd01 and 1.3; conformance clause 7.3.17 in 1.3)
https://docs.oasis-open.org/virtio/virtio/v1.3/virtio-v1.3.html. F: virtio
device ID 25 (0x0019) — observed in a guest on QEMU 11.1.2
(`/sys/bus/virtio/devices/virtio0/device`); PCI 1af4:1059, subsystem 1af4:1100
(QEMU monitor qtree + guest sysfs, both observed 2026-10-30). F: Linux driver
since 5.13 (`CONFIG_SND_VIRTIO`, sound/virtio/; LKDDB "found in kernels
5.13–7.2" https://cateee.net/lkddb/web-lkddb/SND_VIRTIO.html). F: QEMU added
`virtio-sound-pci` in 8.2 (v13 series merged Nov 2023,
https://lists.nongnu.org/archive/html/qemu-devel/2023-11/msg00024.html). F:
QEMU's device is stereo-only, first stream playback / second capture, jacks and
chmaps unimplemented (docs/system/devices/virtio/virtio-snd.rst).
F: **no Windows driver ships today**: the virtio-win repo has no `viosnd`
directory (GitHub contents listing, 2026-10-30) and the request
virtio-win/kvm-guest-drivers-windows#929 is open since 2023-06; maintainer calls
it achievable, work is volunteer-only. I: do not plan Windows guests around
virtio-snd this decade; revisit if #929 lands.

**Intel HDA.** F: Windows has an inbox HD Audio stack — hdaudio.sys bus driver
(https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/hd-audio-bus-driver)
plus the UAA class driver since Vista — so Windows 7-11/Server need no guest
installs. F: Linux `snd_hda_intel` mature since 2.6 (2005).
F, observed as a black box on QEMU 11.1.2 (monitor + Alpine 6.18.58 guest):
`intel-hda` = PCI 8086:2668 (ICH6), `ich9-intel-hda` = PCI 8086:293e (ICH9,
matches real q35's 00:1f.3); card names itself "HDA Intel"; `hda-duplex` codec
registers as **"QEMU Generic", Vendor Id 0x1af40022, Subsystem Id 0x1af40022,
Revision Id 0x100101**: AFG 0x01, audio-out 0x02 (Master Playback Vol/Switch,
amp 0x4a steps), pin 0x03 line-out (default 0x00004010), audio-in 0x04
(Capture Vol/Switch), pin 0x05 line-in (0x00805020); stereo, 16-bit, rates
16/22.05/32/44.1/48/88.2/96 kHz. `hda-output` (no capture) and `hda-micro`
(speaker+mic) variants exist (`-device help`). This dump is the full
guest-visible codec contract D30 requires us to reproduce.

**Emulation cost.** I: HDA costs more than virtio-snd — MMIO register file,
per-stream CORB/RIRB + BDL walks, guest-polled LPIB/wall clock, per-period
IRQs, codec verb channel; virtio-snd is batched virtqueue messages. Both DMA
the PCM payload; the difference is trap/IRQ count. No measurements (NC).

**Legacy.** F: QEMU 11.1.2 also offers AC97 ("Intel 82801AA"), ES1370, SB16,
adlib, cs4231a, gus (`-device help`). I: AC97/ES1370 serve 98SE/XP-era guests
(inbox WDM drivers; Linux snd-intel8x0), SB16 serves DOS. Phase 4 concern.

### 2. Host paths to PipeWire

**(a) In-process `pipewire` crate (pipewire-rs).** F: MIT licence, hosted at
https://gitlab.freedesktop.org/pipewire/pipewire-rs; README self-describes as
"work-in-progress. Expect frequent breakage, bugs and missing features";
0.10.1 released 2026-08 (docs.rs); -sys layer bindgens libpipewire 0.3 headers
(needs PipeWire dev files + clang at build). F: playback/capture via
`pw_stream`; quantum chosen with `PW_KEY_NODE_LATENCY "node.latency"` as a
fraction "Ex: 128/48000" (src/pipewire/keys.h:167); per-machine node naming via
`PW_KEY_NODE_NAME`/description. F: threading is a C-API main loop run on its
own thread (crate docs; examples audio-capture, audio-playback). F: a complete
capture+playback reference in Rust already exists in rust-vmm's
vhost-device-sound pipewire backend (Apache-2.0, readable per D7).

**(b) vhost-device-sound** (github.com/rust-vmm/vhost-device). F: backends
`null|pipewire|alsa|gstreamer`; licence Apache-2.0 OR BSD-3-Clause; a separate
daemon per device speaking vhost-user over a unix socket (`--socket`, optional
`--socket-fd`); deps: `pipewire` 0.9.2, `alsa` 0.12, gstreamer 0.24
(Cargo.toml). F: **no snapshot/restore support** — zero occurrences of
snapshot/restore/migration machinery in vhost-device-sound/src (checked every
.rs, 2026-10-30), and no CLI flags. F: latency model is per-buffer kicks with
`latency_bytes` reported back to the guest (issue #501; semantics from the
2019 virtio-snd list post
https://lists.oasis-open.org/archives/virtio-dev/201912/msg00123.html); no
published latency numbers (NC).

**(c) PulseAudio protocol client.** F: crate `libpulse-binding` 2.30.1
(MIT OR Apache-2.0, binds LGPL libpulse), https://github.com/jnqnfe/pulse-binding-rust.
The WSLg path (§4) is its only justification.

**(d) Others.** F: PipeWire natively implements the JACK API (pipewire-jack),
so a JACK client reaches the same graph; adds nothing over (a) for us (I).
I: raw ALSA via the `alsa` crate means exclusive device access — wrong for a
multi-machine host.

### 3. Latency budget

F: PipeWire defaults on this host: 48 kHz, quantum 1024 (~21.3 ms), min 32, max
2048 (/usr/share/pipewire/pipewire.conf:44-48). F: each async link adds one
quantum; linked node latencies sum (PipeWire latency doc,
https://docs.pipewire.org/page_latency.html). F: QEMU gained a native
`pipewire` audiodev in 8.1 (Aug 2023; "Add Pipewire audio backend for QEMU",
qemu-devel, Apr 2023; press coverage Phoronix 2023-05-09; deep list URLs NC);
QEMU 11.1.2 lists it (`-audiodev help`, local); no pipewire-specific tuning
options surfaced in 11.1.2 (probed `latency-microsecs`, `in/out.buffer` — all
rejected; generic option set NC). F: no published end-to-end guest-to-speaker
found for QEMU, crosvm, cloud-hypervisor or libkrun (crosvm: only
null/file/CRAS backends, CRAS is ChromeOS-only — crosvm.dev cmdline docs;
cloud-hypervisor: virtio-snd request closed in 3 days, #6065, no audio; libkrun:
no sound device in its supported-devices list, README). I, estimates at 48 kHz:
guest ALSA/HDA period 10 ms + one xfer ≈ 0.1 ms + backend quantum 5.3 ms
(256) → **~16 ms tuned floor, ~30-45 ms with defaults**, HDA vs virtio-snd
differing by ~one guest period (10 ms) at most; a vhost-user hop adds <1 ms
(socket wake + shared-memory copy). I: WSLg audio rides its RDP transport, so
expect tens of ms minimum, no published figure (NC).

### 4. WSL2

F: WSLg runs PulseAudio in a system distro, projects its sockets into the user
distro and presets `PULSE_SERVER` (wslg README "WSLg System Distro"); the path
is `unix:/mnt/wslg/PulseServer` (wslg#29 and later issues, e.g. 2026-07). F:
audio out/in is an RDP sink/source pair (module-rdp-sink/source) merged onto
the RDP transport by Weston (README). F: the socket speaks PulseAudio, not
PipeWire; a distro-local PipeWire has no hardware sink, so the final hop is
still pulse (I). NC: whether a recent WSL swapped the server for pipewire-pulse.
I: vmlab-hv under WSL2 uses a libpulse client to that socket as a second
backend beside PipeWire.

### 5. Snapshots

**virtio-snd** (VIRTIO 1.3 §5.14; message names verified in spec text): save
virtqueue states (avail/used indices, enabled), per-substream PCM state machine
(SET_PARAMS format, PREPARE'd, START/STOP), in-flight tx/rx buffers with their
`virtio_snd_pcm_status` replies incl. `latency_bytes`, and the
jacks/streams/chmaps config. I: sane resume = restore queues + substream
states, drain one period on restore, accept one glitch; drop capture sampled
between snapshot and restore.

**Intel HDA** (register file defined by the public Intel spec 1.0a): controller
MMIO state (capabilities, CORB/RIRB bases + read/write pointers, immediate
command, INTCTL/STS), per-stream SD* registers with BDL base, stream positions
(LPIB/DMAPOS) and the wall-clock counter, stream-tag↔codec-routing, MSI state;
plus **codec state**: every verb side effect on the emulated QEMU Generic codec
(amp gains/mutes, pin controls, EAPD, power states, unsolicited-response
enables). I: LPIB and WALCLK are guest-polled and must restore monotonically
or playback skips; the codec's widget state must match what the guest last
read or Windows' mixer UI desyncs.

**vhost-user sound.** F: no save/restore support (§2b). I: therefore an online
snapshot of a machine with vhost-user audio must be refused — the same
behaviour SPEC D14/D33 accepted for external virtiofsd in phase 1 — or the
device must be in-process. This single fact nearly decides Q3.

### 6. Clean-room

F: everything above implements from public specs: VIRTIO 1.2/1.3 ch 5.14
(OASIS, public); Intel HDA Specification Rev 1.0a, public 225-page PDF (live,
verified 2026-10-30:
https://www.intel.com/content/dam/www/public/us/en/documents/product-specifications/high-definition-audio-specification.pdf).
F: the QEMU-parity facts we need (PCI IDs, codec IDs, pin defaults) are all
guest-observable; the ones in this note were captured black-box from QEMU
11.1.2 (monitor introspection + Alpine netboot guest), no QEMU source read.
NC: live URLs for the AC'97 2.3 spec and Creative's SB16 programming guide
(Intel's AC'97 link is dead; mirrors exist) — only phase 4 needs them.

## Options

- **O1 virtio-snd + in-process PipeWire client.** Simplest device (message
  queues, no MMIO), lowest guest CPU, Linux 5.13+ guests only; no Windows
  driver (F, §1).
- **O2 Intel HDA (ICH6/ICH9 + QEMU Generic codec) + in-process PipeWire
  client.** Every guest Windows 7+ and Linux without guest installs; D30
  parity is a matter of replaying the observed IDs; heaviest model to build
  (spec 1.0a is public and precise).
- **O3 vhost-user-sound daemon.** Backends for free, but no snapshot/restore
  (F), one more process per machine against D5, and no help for Windows guests.
- **O4 PulseAudio-protocol fallback backend** for WSL2 hosts; composes with
  O1/O2 behind a host-audio seam.
- **O5 viewer-process playback** (viewer plays PCM shipped by the hypervisor).
  Adds a hop and a second process; contradicts "straight into PipeWire" (I).

## Conclusion (ranked)

1. **O2, in-process, is the first device** — it alone covers Windows templates
   (D30) and every Linux guest, with zero guest-side installs; the codec
   contract is fully captured above.
2. **O1 as the second device and the Linux fast path** — spec-simple, add once
   HDA proves the pipeline; watch virtio-win#929 before ever promising Windows
   virtio-snd.
3. **Q3/Q4: the vmlab-hv process itself plays audio** via the `pipewire` crate
   behind a host-audio seam (playback + capture, quantum 256/48000, node name
   per machine); not vhost-user (snapshots, D5). No viewer in the path.
4. **O4 pulse fallback for WSL2** — same seam, second backend, WSL2-host
   default.
5. **Q6 microphone: in scope, capture included** — hda-duplex already carries
   line-in; keep host mic gated behind a per-machine flag until measured.

## Consequences

- Device-state format (D14) gains HDA + virtio-snd sections from the §5 lists;
  online snapshots with an external audio process are refused (D33 pattern).
- D30 hard facts to replicate: 8086:2668 (`intel-hda`), 8086:293e
  (`ich9-intel-hda`), virtio-snd PCI 1af4:1059 (virtio device 25), codec
  "QEMU Generic" 0x1af40022 rev 0x100101 with the §1 widget map. Which
  controller variant vmlab's QEMU templates request: not checked.
- New build/runtime dependencies: libpipewire + clang/bindgen; pipewire-rs is
  MIT but self-declared unstable — pin it, keep it behind the seam so a
  breaking release is a one-file fix.
- No published latency baselines exist anywhere (F, §3): if a number is
  wanted, we must build the measurement (S1-adjacent e2e harness) ourselves;
  target ≤16 ms guest-to-speaker tuned.
- Audio is phase 2 scope (modern VMs, alongside display); phase 1 containers
  are unaffected. Legacy AC97/SB16 decisions defer to phase 4.
- Clean-room holds: two public specs plus black-box observation; no QEMU
  source needed at any point.

DONE /home/wil/dev/vmlab-hypervisor/.scratch/plans/hv-audio/notes/01-audio-research.md
