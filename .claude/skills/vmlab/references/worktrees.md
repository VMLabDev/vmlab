# Worktree labs

A **worktree lab** is a lab instance in a linked git worktree whose machines are
linked clones of the main checkout's **freeze** rather than of their templates.
The main checkout's lab is frozen once at an offline snapshot; every linked
worktree's `vmlab up` then clones each machine from the frozen disks and boots it
as if that snapshot had just been restored, so hours of provisioning are not
repeated. The main lab keeps running while worktree labs come and go, and no lab
of the family can reach another. The verbs are `vmlab worktree freeze`, `thaw`
and `status` (cli-lab.md). Every worktree lab is first a lab instance
(architecture.md, "Lab instances").

Nothing in `vmlab.wcl` declares a freeze. It is working data in the main
checkout's `.vmlab/`, so no branch commits or carries one. A lab run with
`--instance` or `VMLAB_INSTANCE` in an ordinary directory is not a worktree lab
and keeps the ordinary instance rules. To share a provisioned machine across
hosts, build a template from it instead (templates.md).

## Terms

| Term | Meaning |
| --- | --- |
| freeze | The main checkout's lab disks made read-only at a named offline snapshot, with each machine's firmware variables, TPM state, state record and the lab's named volumes copied beside them. Kept as numbered generations; undone by a thaw. |
| generation | One freeze, numbered from 1 under `.vmlab/freeze/<n>/`. Freezing again adds generation `n+1`, backed by `n`; a worktree lab keeps the generation it was cloned from. |
| freeze point | The snapshot a generation was frozen at. Every lab of the family holds it, and restoring it empties the lab's overlays onto the frozen files. |
| locked snapshot | A snapshot the main lab took before a freeze. It lives inside a frozen file, so restoring, deleting or taking it again is refused until `vmlab worktree thaw`. |
| worktree isolation | The rules a freeze turns on for the main lab and every worktree lab built on it: NAT refuses host addresses, forwards bind `127.0.0.1`, a worktree lab's global segments join `<segment>@<worktree>`. |

## Freeze the main lab

Run the freeze in the main checkout with the lab stopped and an **offline**
snapshot of the same name on every machine. An online snapshot is refused: its
memory holds the main lab's DHCP leases and agent sessions, and a clone booting
its disk alone would see a power cut.

```sh
cd ~/src/app
vmlab down
vmlab snapshot create provisioned
vmlab worktree freeze provisioned
```

```
froze generation 1 at "provisioned": dc01, web, db
snapshots taken before it are locked until `vmlab worktree thaw`; linked worktrees now clone these machines on `vmlab up`
```

Each disk is rewound to the snapshot, moved read-only into
`.vmlab/freeze/<n>/<machine>/`, and replaced by an overlay on it that holds the
state the lab had, so the main lab carries on unchanged. A failure on any disk
undoes every step before it. On a filesystem with reflinks (Btrfs, XFS) the
freeze copies nothing; on any other it copies every disk in full, keeps the
copies until the whole freeze has committed, and copies the named volumes for
good, so the lab needs that much free space while the freeze runs. cli-lab.md
(`vmlab worktree freeze`) lists what is copied and every refusal.

There is no live freeze: the main lab must be stopped.

## Worktree isolation

Cloned guests share hostnames, MAC addresses, machine-ids and Windows SIDs, so
two labs of a family must never share a wire or a route. While the main checkout
is frozen, the main lab and every worktree lab built on it run isolated:

- **NAT refuses every new flow to the host itself**: `127.0.0.0/8`,
  `0.0.0.0/8`, and every address and broadcast address on a host interface,
  re-read at most once a second so a VPN or a renewed lease is covered from the
  next flow. A refused TCP connect gets a reset; UDP and ICMP are dropped
  silently. The one exception is TCP to `127.0.0.1` on the lab's own smbd port,
  so shared folders keep working. The internet stays reachable. Without this
  rule a worktree guest could reach the main lab's forwards through the host's
  LAN address and land on its own clone.
- **Port forwards bind `127.0.0.1`** instead of every interface (segment
  `forward {}` and container `port {}` alike), and a fixed host port is shifted
  by the lab's port slot (networking.md, "Ports in a frozen family").
- **A worktree lab's global segments join `<segment>@<worktree>`**, named after
  the git worktree, so they are shared with no other lab of the family; two lab
  files in the same worktree still meet on it. A global segment with a
  `connect {}` peer refuses `up` in a worktree lab. The frozen main lab keeps the
  plain names.

A lab daemon decides isolation once, when it starts. The freeze ran in a daemon
that started before it, so the next `up`, `pull`, `vm start` or `vm restart` in
the main checkout replaces that daemon first and prints:

```
lab "app": the main checkout's freeze or a machine's disk changed — restarting the lab daemon to plan its clones again
```

The same replacement happens in a worktree lab after a refreeze or thaw, or when
a machine has lost its disk, with the same refusals as a lab-file edit while a
machine is running. `vmlab status` then shows the decision on the line after the
lab's name (cli-lab.md, `vmlab status`):

```
worktree isolation: worktree "feat-x", generation 1 at "provisioned"; NAT refuses host addresses, forwards bind 127.0.0.1
  global segment "corp" joins as "corp@feat-x"
```

The main lab reads `frozen main lab` in place of the worktree. The daemon emits
`lab.isolation` once per start, before any machine starts (automation.md).

## Bring up a worktree lab

In a linked worktree, `vmlab up` reads the main checkout's freeze; nothing in the
worktree changes:

```sh
git worktree add ../feat-x
cd ../feat-x
vmlab up
```

Each machine with no disk yet is cloned from the newest generation when the
freeze holds it under the same `template` or `image` reference. The clone keeps
the frozen machine's MAC addresses, firmware variables and TPM state, and boots
as the main lab would right after restoring the freeze point, so its first-boot
provision does not run again. `machine.cloned_from_freeze` records each one. A
machine new in this branch, or one whose reference changed, builds from its
template as usual. A machine that already has a disk keeps it, including one
built before the freeze; `vmlab vm destroy <name>` then `vmlab up` rebuilds it
from the freeze. A dev machine cloned from the freeze re-seeds its workspace from
this worktree's tree before its watch opens (dev-machines.md).

When the freeze's files are missing, `up` refuses and names
`vmlab worktree freeze` as the fix. It never falls back to the templates, which
would silently skip the provisioning the freeze carries.

A script reads a forward's host port with `vmlab port <machine> <guest-port>`
rather than computing it from the slot.

## Reset a lab to the freeze point

Restoring the freeze point resets any lab of the family, the main lab included,
to the frozen state:

```sh
vmlab snapshot restore provisioned
```

The restore stops a running machine and replaces each disk with an empty overlay
on its frozen file, writing no frozen file. With no snapshot taken since the
freeze it is instant. Offline snapshots taken since then survive, each carried
into the new overlay at a cost in proportion to what it holds. An online one
cannot be carried, so the restore refuses, naming it, until it is deleted with
`vmlab snapshot delete`. A disk that is not an overlay on the frozen file, such
as one built from its template, refuses too. The freeze point's name cannot be
taken again by `vmlab snapshot create`. snapshots-vision.md ("Snapshots under a
freeze") gives the locked-snapshot rules.

## Freeze again

When the main lab has moved on, freeze it again at a new offline snapshot:

```sh
vmlab down
vmlab snapshot create upgraded
vmlab worktree freeze upgraded
```

This freezes the main lab's current overlays as generation 2, backed by
generation 1, and locks the old freeze point with the other earlier snapshots.
Worktree labs already built keep generation 1; only machines built from now on
start from generation 2. `vmlab worktree status` shows which worktree uses which
generation.

## Thaw

A thaw folds the freeze back into the main lab's disks and unlocks its earlier
snapshots. Destroy every worktree lab first, then thaw in the main checkout with
the lab stopped:

```sh
cd ../feat-x && vmlab destroy
cd ../app && vmlab down && vmlab worktree thaw
```

The main lab keeps its current state, and offline snapshots taken after a freeze
survive. The thaw refuses while any worktree disk backs onto the freeze, naming
each worktree, and on online snapshots taken since a freeze unless
`--drop-online-snapshots` gives them up. Isolation ends at the main lab's next
`up`, which replaces its daemon as after the freeze.

## Guards on a freeze

- `vmlab destroy` (and `vmlab lab destroy`) in the main checkout refuses while
  any worktree lab's disk backs onto the freeze, with or without a daemon
  running, naming each worktree and its generations (exit 5). With none, it
  deletes `.vmlab/freeze/` and `worktree.json` with the rest of `.vmlab/`, and
  the frozen state is gone for good.
- `vmlab destroy` in a worktree lab deletes only that worktree's overlays and
  copies, never a frozen file. It is how a worktree releases the freeze; the next
  `up` there clones from the newest generation. It also releases the worktree's
  port slot.
- `vmlab lab move` refuses a frozen lab, because every overlay of the family
  names the frozen files by absolute path. With worktree labs on the freeze the
  refusal names them; otherwise it asks for `vmlab worktree thaw` first.
- The template store counts a template as in use while any frozen file backs
  onto it, so `vmlab template rm` needs `--force`.
- Nothing in vmlab opens a frozen file read-write.

A freeze is not portable. Worktree overlays name the frozen files by absolute
path, so moving or renaming the main checkout's directory, or its
`VMLAB_WORK_DIR`, breaks every worktree lab built on it. Destroy the worktree
labs and thaw first.

## Files

| Path under the main checkout's `.vmlab/` | Holds |
| --- | --- |
| `worktree.json` | The newest generation and the snapshot it was frozen at. Written last by a freeze; its presence is what makes the lab frozen and turns on isolation for the family. |
| `freeze/<n>/<machine>/` | Generation `n`: the machine's frozen disks, read-only, with copies of its `OVMF_VARS.fd`, `tpm-state/`, `firstboot.done`, `clone-source` and `state.json` record. |
| `freeze/<n>/volumes/` | The lab's named volumes, copied at that generation. |
| `worktree-ports.json` | Each worktree lab's port slot, keyed by the git worktree's name, and every fixed port each lab of the family has bound, the main lab's declared ones as slot 0. Locked exclusively for every change; a thaw deletes it. |

In a worktree lab, `vms/<vm>/freeze-generation` (and the same file beside a
container's disk) records the generation the machine was cloned from.
