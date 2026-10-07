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

## End-to-end suite

`just e2e::run` builds this checkout into a Docker image and runs every
scenario in a privileged container with /dev/kvm (about 16 minutes; see
`e2e/README.md`). `just e2e::run --only core guest` runs named scenarios;
`--keep` leaves the labs up. Results land in `e2e/results/` (`results.json`,
`e2e.log` with every command and its output). When several worktrees run it at
once, give each its own image: `VMLAB_E2E_IMAGE=vmlab-e2e-<name>`. Scenarios
live in `e2e/scenarios/`, features in `e2e/features.py`; a feature no scenario
reports fails the run.

## Gotchas

- `just lab-up` defaults to `examples/mixed-lab`; pass `dir=` for another lab.
- Stopping a machine (or a partial `down`) keeps labd running (status still
  served, machines show stopped); a full `vmlab down`, `lab stop` or `destroy`
  reaps it (status says not running). A later `up` replaces a surviving daemon
  when `vmlab.wcl` has changed since it loaded it.
- `vmlab up` returns once machines have started, not once they are ready
  (a VM with shares holds it up to 30 s more, for its mounts). Poll
  `vmlab status -v` for `ready=yes` before `exec`/`cp`.
- Run the binary under the name `vmlab`. Under any other name the CLI starts
  the daemons from the `vmlab` on PATH instead, which is usually an older
  install. After rebuilding, `vmlab daemon stop` so the next command starts a
  supervisor from the new binary.
- Guest assets: `just guest-install` builds and installs them; a release's
  `install.sh` places the prebuilt bundle. A container lab needs them.
