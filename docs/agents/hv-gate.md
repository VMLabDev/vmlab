# Hypervisor release and nightly gate

The lead runs `just ci::hv-gate` on every `release/<version>` commit before
the release e2e. A nonzero exit blocks e2e and publication. This is separate
from the merge queue's `just ci::check`, whose command is unchanged.

In the vmlab-owned full copy of the pushed release commit on helios, run:

```sh
set -eu
. ~/.cargo/env
export PATH="$HOME/.local/bin:$PATH"
export VMLAB_SKIP_UPDATES=1
export VMLAB_TARGET_ROOT=/var/build/cache/vmlab/lead
cd "$VMLAB_TARGET_ROOT/src"
/var/build/cache/vmlab/bin/heavy just ci::hv-gate
```

Use `terrarium run vmlab-helios-vmlab --name release-hv-gate -- sh -c '<script above>'`
to retain the log. Record the release branch, exact commit, command, exit
code, passed/failed/ignored counts and run ID on the release card. Only then
start the release e2e. The wrapper waits for a shared heavy-run slot and
applies the target resource limits. Obey target GPU and
VFIO reservations. In particular, no GPU use before 14:15 UTC on 2026-11-10.

`hv-nightly.yml` runs the same gate on `next` at 02:30 UTC on GitHub-hosted
Ubuntu. We chose a scheduled workflow rather than a Terrarium schedule so
it outlives card agents, retains logs and does not contend for shared helios
hardware. It has no push/PR/manual trigger and explicitly checks out `next`,
never a card branch. GitHub activates schedules after this workflow reaches
the default branch. A missing KVM device fails the run, not a green skip.

The gate sets `VMLAB_REQUIRE_KVM=1`. Packages with KVM tests probe read/write
access during their Cargo build. Without KVM, ordinary tests are reported
as ignored with a reason by libtest. Required mode fails the build instead.
Cargo tracks `/dev/kvm` and the environment flag for rebuilds. Build and run
on the same host. Do not copy prebuilt test binaries between hosts. Existing
local-asset and GPU prerequisites still apply, so inspect ignored counts
alongside passes rather than treating them as hardware coverage.
