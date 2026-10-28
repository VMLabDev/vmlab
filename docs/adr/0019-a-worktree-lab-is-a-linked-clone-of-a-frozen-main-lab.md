# ADR-0019: A worktree lab is a linked clone of a frozen main lab

- **Status**: Proposed
- **Date**: 2026-10-28
- **Supersedes**: nothing
- **Amends**: [ADR-0011](0011-a-lab-name-is-its-host-global-runtime-identity.md)
  (global segments, for labs built on a freeze)
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0010](0010-the-supervisor-owns-the-template-store.md),
  [ADR-0014](0014-the-workspace-is-a-guest-local-copy-of-a-canonical-host-tree.md),
  [ADR-0017](0017-a-template-version-is-a-base-plus-sealed-variants.md)

## Context

A linked git worktree already runs its lab as an instance under
`<declared>-<worktree>` (ADR-0011's amendment), with its own `.vmlab/`. Each of
its machines is cloned from the template and provisioned from scratch. For a
Windows domain or any lab with a long provision, that costs as much as the
first build of the main lab, every time a worktree is created.

The main checkout's lab already holds a provisioned state, usually captured as
a snapshot. The worktree labs should start from that state, while the main lab
keeps running and changing.

qcow2 limits how that can be done:

- A linked clone backs onto a file's **current** state.
  `qemu-img create -b <file>` cannot name an internal snapshot, and vmlab's
  snapshots are internal (§7.3, `src/labd/vm.rs:1364-1371`). ADR-0017 met the
  same limit for template variants.
- A running QEMU holds a write lock on its disk. Another process can only read
  that disk by turning locking off.

Three ways to give worktrees a stable file were considered:

- **Export.** `qemu-img convert -l snapshot.name=<s> -B <template>` writes the
  snapshot out as a new read-only file. Rejected. It is safe and leaves the main
  lab untouched, but it copies every byte provisioning wrote, which is several
  GB for a Windows guest, once per snapshot used.
- **Serve the snapshot.** `qemu-nbd -l snapshot.name=<s> -r` exports an internal
  snapshot read-only, and a worktree disk backs onto the NBD socket. Rejected.
  The main lab's QEMU holds the write lock, so `qemu-nbd` has to run with
  `file.locking=off`. That relies on qcow2 never rewriting a cluster a snapshot
  references, which is true of the implementation but not something QEMU
  promises for a file another process is writing. One daemon per disk would also
  have to outlive every worktree that uses it.
- **Freeze.** Stop writing the main lab's disk file at all. The main lab and each
  worktree get a new overlay backed by it. Chosen.

## Decision

**We will freeze the main lab's disks at a named offline snapshot. The frozen
files become read-only, and the main lab and every worktree lab run on qcow2
overlays backed by them.** `vmlab worktree freeze <snapshot>` does this in the
main checkout. `vmlab worktree thaw` undoes it.

- **The frozen file's current state is the snapshot.** Freezing needs the lab
  stopped and an offline snapshot `<snapshot>` on every machine. For each disk,
  the freeze takes a temporary internal snapshot of the current state, applies
  `<snapshot>` (`qemu-img snapshot -a`), and moves the file to
  `.vmlab/freeze/<n>/<machine>/`. It then writes the main lab's new overlay
  from the temporary snapshot with
  `qemu-img convert -l snapshot.name=<tmp> -B <frozen>`, and deletes the
  temporary snapshot. The main lab keeps exactly the state it had, and nothing
  is rewound. A plain backing file then gives every worktree the snapshot's
  state.
- **The freeze is a generation.** `.vmlab/worktree.json` names the newest
  generation and its snapshot. Freezing again freezes the main lab's current
  overlay as generation `n+1`, backed by generation `n`. A worktree clone keeps
  the generation it was made from.
- **A worktree lab clones from the freeze.** On `up` in a linked worktree, vmlab
  reads the main checkout's `worktree.json`. Each machine that has no disk yet,
  and whose template or image reference matches the one it was frozen under, is
  cloned from the freeze instead of the template. The clone is an overlay in the
  worktree's own `.vmlab/`. The machine's UEFI variables, TPM state, named
  volumes and state record (MACs, the freeze snapshot's record, first-boot
  marker, container image pin) are copied. The machine then behaves as if it had
  been restored from that snapshot. An `@dev` machine's workspace ledger starts
  with `reseed_owed = true`, so ADR-0014's re-seed carries the worktree's own
  tree into the guest.
- **MACs are kept.** A worktree machine has the same MACs as its frozen machine.
  Guest software keys on them, and worktree isolation guarantees the two never
  share a wire.
- **Snapshots before the freeze are locked.** They live in the frozen file, and
  restoring one would write to it. They stay listed and refuse to restore until
  `thaw`. The freeze snapshot is the exception. Restoring it in any lab of the
  family replaces that lab's overlay with an empty one.
- **Thaw needs no dependants.** `thaw` refuses while any worktree disk backs onto
  any generation. Otherwise it commits the main lab's overlays down the chain
  (`qemu-img commit`), moves the files back, deletes `worktree.json`, and
  unlocks the earlier snapshots.
- **A freeze turns on worktree isolation** for the main lab and every worktree
  lab built on it. NAT refuses connections to the host's own addresses. Port
  forwards bind `127.0.0.1`, not `0.0.0.0`. A worktree lab's global segments
  join `<segment>@<instance>` rather than `<segment>`, and `connect {}` is
  refused. That last rule amends ADR-0011, which says both instances join the
  same global segment.

## Consequences

**Gained**

- A worktree lab starts provisioned in the time it takes to create its overlays
  and copy a few small files. Disk use grows only by what each lab writes after
  the freeze.
- The main lab keeps running and changing, with its state unchanged by the
  freeze.
- No daemon serves a disk, and no lock is bypassed. Every chain is ordinary
  qcow2: worktree overlay, then the frozen generations, then the template.
- Worktree labs and the main lab cannot reach each other over any segment, NAT
  path or forward.

**Given up**

- The main lab's snapshots taken before the freeze cannot be restored until
  `thaw`, and `thaw` waits until no worktree uses the freeze.
- The main lab's `.vmlab/` is no longer disposable while a worktree depends on
  it. `vmlab destroy` and `vmlab lab move` in the main checkout refuse until no
  worktree uses the freeze, because the worktree overlays name the frozen files
  by absolute path.
- Each refreeze adds a file to the chain, which costs a little read speed per
  layer. A generation can only be removed once nothing backs onto it.
- Cloned guests share the main lab's hostname, machine-id and Windows SID.
  Isolation makes that harmless, and it is the reason isolation is not optional.
- An application in a worktree guest cannot reach a service on the host, a cost
  of the NAT rule.

**Watch for**

- Anything that writes a frozen file: `qemu-img snapshot -d` or `-a`,
  `qemu-img amend`, a repair, or a QEMU opening it read-write. Each one corrupts
  every worktree on that generation.
- The template store's in-use check (`src/supervisor/store.rs:400`) reads only
  the direct backing file of each lab's `disk0.qcow2`. It has to walk whole
  chains and include frozen files, or a template prune breaks every lab of the
  family.
- New host-facing listeners. Each one is a path between labs unless worktree
  isolation covers it.
