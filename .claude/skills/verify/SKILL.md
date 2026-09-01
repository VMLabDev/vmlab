---
name: verify
description: How to build, launch, and drive vmlab surfaces for runtime verification — the CLI and scratch labs.
---

# Verifying vmlab changes at runtime

## CLI

`cargo build`, then run `target/debug/vmlab` from a lab directory. The
supervisor is shared and long-lived: `vmlab daemon status` lists every lab
ever registered on this host.

## Scratch labs

Make a lab dir in the scratchpad with a `vmlab.wcl`, then `vmlab up` from it —
it auto-registers with the supervisor. The cheapest guest is an OCI container
(e.g. `nginx:1.27` micro-VM, ~15 s to ready, no template build); needs the
guest assets in `~/.local/share/vmlab/guest/x86_64/` (usually already
installed). When done: `vmlab destroy` from the lab dir, otherwise the lab
lingers in the supervisor registry forever.

## Gotchas

- `just lab-up` defaults to `examples/mixed-lab`; pass `dir=` for another lab.
- Stopping a machine keeps labd running (status still served, machines show
  stopped); `vmlab down` kills labd (status goes null).
