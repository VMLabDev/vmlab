# ADR-0014: The workspace is a guest-local copy of a canonical host tree

- **Status**: Accepted
- **Date**: 2026-08-05
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0012](0012-vmlab-terminates-ssh-on-the-host.md),
  [ADR-0013](0013-the-host-opens-channels-the-guest-answers.md)

## Context

PRD §19 has the editor attach *into* the guest, so the language server, the
build and the test watcher all run guest-side against the source tree. vmlab
already has a share mechanism (§7.5) that looks exactly right for putting a host
directory there.

It cannot carry a watched source tree. Host-side edits do not reach
`ReadDirectoryChangesW` over virtiofs — no released virtio-win pushes the
notification, and the merged one polls, watches only already-open handles and
reports one undifferentiated `MODIFIED` — and they do not reach a recursive
`SMB2_WATCH_TREE` over Samba, whose inotify backend registers a single
non-recursive watch. Linux guests fail identically: inotify does not fire for
host-side virtiofs changes, blocked at three independent kernel layers. **Both
fail silently.** The watcher stays armed and quiet and the language server simply
stops re-analysing.

The opposite placement — source guest-side and on its own — trades that for a
different loss: `destroy` is a first-class verb on disposable clones, and a
snapshot restore rolls uncommitted work back with the machine.

## Decision

**The workspace is a guest-local working copy on the machine's own disk; the
host directory is canonical; a vmlab-integrated syncer keeps them in step.**

The guest gets a real local filesystem, where `inotify` and
`ReadDirectoryChangesW` are native rather than simulated. The host holds
durability: restore re-converges from it, and `destroy` loses nothing.

Three properties make it a structural decision rather than a component choice:

- **The syncer is vmlab-integrated, not a generic tool wrapped.** vmlab performs
  snapshot capture and restore, so it can bracket them. An off-the-shelf syncer
  cannot know a rewind happened, and would propagate 500 rolled-back files onto
  the canonical copy as if the developer had edited them.
- **The agreement point is a host-side ledger** — digest per path, plus each
  side's own `(size, mtime)` as a pre-filter. Host-side because a guest-held copy
  is exactly the surviving guest-side state this decision removes, and because
  the two sides' clocks are not comparable: a restored guest resumes behind the
  host, which disqualifies any newest-wins rule outright.
- **Ignore semantics never enter the guest.** The guest is handed a coarse prune
  list of directory prefixes not to register a watcher under; it is never asked
  to decide, for a file it created itself, whether that file is in the synced
  set. That decision *is* the ignore set, and every partial version leaves build
  outputs in one tree and not the other.

Conflict policy is **halt and surface** — the whole workspace, both directions,
one machine, reporting every conflicting path in the batch. A conflict is an
anomaly, since authoring happens guest-side and the host-side writer set is
small and enumerable, which licenses an expensive loud policy over a winner rule
that must be right thousands of times a day.

Rejected en route: a **lab-scoped workspace disk** (retired — once the host is
canonical the guest copy need not survive anything; it survives only as a
possible performance option); a **vmlab-authored filesystem** on FUSE and WinFsp
(right mechanism, fatally asymmetric — the Linux FUSE notification path raises no
fsnotify events at all, so it buys one guest OS); and **agent-injected events on
a share** (works as a mechanism, and fixes one of the three findings above — the
path cap, the case sensitivity and the silently-failing `rm -rf` all stand).

## Consequences

**Gained**

- Native file-change notification, native filesystem semantics, and git's own
  fast path, on both guest families, with nothing to work around.
- `Rebuild Container`'s promise — resets everything except your local source —
  delivered rather than traded, with no rebuild verb and no durable guest-side
  state to drift.
- One mechanism for every machine kind. The entire OS difference is the
  profile-supplied default path.

**Given up**

- Conflict semantics exist at all, which the share route did not have. A halt is
  a stopped dev machine until someone resolves it.
- The syncer, its ledger, and a third agent capability (`watch`) are vmlab's to
  build and keep correct.
- Windows costs three active measures rather than none: the NTFS case-sensitive
  flag at every `mkdir`, attempted symlinks with a warning, and
  `core.autocrlf = false` guest-side.

**Watch for**

- Anything that puts sync state, ignore rules or an agreement record on the
  guest side. Each is the state this decision exists to eliminate.
- Comparing a host mtime to a guest mtime. It looks correct and silently keeps
  exactly the state a restore reconcile exists to destroy.
