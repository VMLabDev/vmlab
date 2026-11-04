---
id: "04"
decision: D3
---

# Host path and who plays

## Question
In-process PipeWire client, vhost-user daemon, or the viewer?

## Answer
The vmlab-hv process plays and captures directly into the user's PipeWire session through the `pipewire` crate, behind a host-audio seam (a backend trait). One PipeWire node per machine, named from the lab and machine so `wpctl` and pavucontrol can control it. The viewer is not in the audio path.

## Why
Fewest hops and works with no viewer open; vhost-device-sound has no snapshot support and adds a process per machine (`notes/01`). The seam contains the crate's self-declared instability. Recommended option, accepted.

## Consequences
- The `pipewire` crate is pinned and confined to one backend module.
- libpipewire development files and clang become build dependencies; the backend is a cargo feature so builds without it still work.
- vmlab-hv must reach the user's PipeWire socket: it inherits XDG_RUNTIME_DIR from vmlab's lab daemon.
