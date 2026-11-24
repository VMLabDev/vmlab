# Second review: labd workspace sync

Card #946. A second bug-hunting pass over the workspace syncer. It lists faults
and makes no fix. Reviewed at `next` commit `fec7ca65` (2026-11-24). All paths
are from the repository root, and all line numbers are from that commit.

The first pass is `docs/reviews/labd-sync.md` (card #380, findings F1 to F11,
notes N1 to N13). This document continues its numbering, so a code names one
finding across both: F12 to F20 and N14 to N22 are new here. A finding of the
first pass is named again only where its fix is incomplete (F19), and the text
says so.

Severity words are those of the first pass: **security**, **data loss**,
**crash**, **wrong result**, **cosmetic**.

## What was read, and what was not

Read in full, tests excluded:

- `src/labd/workspace/`: `syncer.rs`, `plan.rs`, `apply.rs`, `scan.rs`,
  `ignore.rs`, `ledger.rs`, `watcher.rs`, `reseed.rs`, `locks.rs`, `halt.rs`,
  `windows.rs`, and `guest.rs` up to the fake guest (line 253).

Read in part:

- `src/labd/workspace/bracket.rs` and `diff.rs`: the code, without the doc
  comments. In `diff.rs` the guest half of `guest_side` was not read.
- `src/labd/workspace/guest.rs` from line 253 (the fake guest) and the test
  module of `syncer.rs`: the helpers the probes use, and four test bodies.
- `src/labd/lab.rs`: only the `dev sync` handlers (lines 3640 to 3730).
- `guest/agent/src/watch/mod.rs`: only how the prune list is applied (lines
  160 to 190). `guest/agent/src/fileops.rs`: only `attrs_of` (lines 682 to
  700).
- `src/labd/vm_agent.rs`: only how `pull_to` opens the local file.
- PRD §19.6: the size guard and the paragraph on stop and `destroy`.

Not read: the other test modules, the agent's watch backends (`inotify.rs`,
`rdcw.rs`), the agent's path resolver and Windows code, the `agent-proto`
codec, `src/cli/dev.rs`, and the wiring that builds a `Workspace` in `lab.rs`.
A fault in those is not in this document.

## How the findings were proved

F12 to F18 and F20 were run, not only read. Eleven temporary tests were added
to the test modules of `watcher.rs`, `ignore.rs`, `plan.rs` and `syncer.rs` in
the checkout of this card on helios, run, and removed. They are not committed.
All eleven passed, and each passes only while its fault is present. Their
source and the command are in `docs/reviews/labd-sync-2-probes.md`. Each finding
names its probe and proposes the test a fix should leave behind. No proposed
test depends on a wall-clock limit.

F19 was not run by a probe: a test that is already in the tree shows it.

The syncer probes use the in-memory guest (`FakeGuest`), not a VM. Where a
finding depends on what a real guest, a real agent or Windows does, the
finding says so.

## Findings, worst first

| Code | Fault | How bad |
|---|---|---|
| F12 | A guest deletion that passes see in parts is carried out on the host part by part, under the bulk-delete guard | Data loss (host) |
| F13 | An absence nothing saw is deleted on the host once a host edit of that path is taken back | Data loss (host) |
| F14 | A host directory that is deleted and made again before the next scan is not watched | Wrong result (host edits stop syncing, with no word) |
| F15 | A guest file made while a re-seed runs is never seen | Data loss (guest work), wrong result |
| F16 | A guest file the size guard refused is forgotten by the next pass, and no bracket refuses on it | Data loss (guest work) |
| F17 | A rule line that git accepts and the glob parser refuses fails every pass | Wrong result (sync stops until the host is cleaned by hand) |
| F18 | A rule file has no bound, and the prune list costs the square of its lines | Security against a hostile guest (host CPU) |
| F19 | F4's fix is incomplete: a refused symlink or a case collision still holds a re-seed for ever | Wrong result (sync stops for good after a restore) |
| F20 | A guest symlink whose `lstat` size is not its target length can never be replaced or removed | Wrong result (Windows guests, modelled) |

### F12. The bulk-delete guard counts one pass, so a deletion in slices gets through

**Where.** `src/labd/workspace/plan.rs:293-295` (`BulkDelete::triggered`),
`plan.rs:865-894` (`withhold_bulk_delete`),
`src/labd/workspace/syncer.rs:1854-1899` (`unsettled`).

**Why it is a fault.** The guard holds guest-to-host removals back when one
plan has more than 20 of them and they are more than half of the ledger. Both
numbers are of one pass. A pass decides only the deletions whose events have
settled (`unsettled` defers the rest for a quiet period), and a walk after an
overflow decides whatever is gone at that moment. So an `rm -rf` in the guest
that is still running when a pass looks is seen in parts. Each part that is
half of the ledger or less is carried out on the host, and the ledger shrinks
by that part, so the next part is measured against a smaller number. Under 21
paths nothing is held at all. The module text says the guard "exists for the
guest doing something catastrophic and the syncer faithfully replicating it
onto the copy nothing re-derives". A large tree is the case where the deletion
takes longer than one quiet period (250 ms), so the guard is weakest where it
is needed most.

**Proof.** Probe `review946_p6` (plan level): 100 agreed files, the guest
loses them in parts of 50, 25, 12 and 13. Result: 4 plans, no `bulk_delete`,
no conflict, 100 of 100 host files removed. Probe `review946_p6b` (the loop):
100 files, the guest deletes 45 and the watch reports them, then the other 55.
Result: the host is left with 55 files with no halt, and the halt comes only
for the second part.

**Test to keep.** `a_guest_deletion_that_spans_passes_is_held_as_one`: the
steps of `review946_p6b`, asserting that the host still holds 100 files and
the workspace is halted. A fix needs a count that outlives one pass, for
example removals carried out since the last pass that removed nothing.

**How bad.** Data loss on the canonical copy: up to half of the agreed paths
for each pass, with no halt.

### F13. An absence nothing saw is deleted on the host after a host edit is taken back

**Where.** `src/labd/workspace/plan.rs:752-771` (the branch ends at
`if !host_moved.changed()`), `plan.rs:773-781`,
`src/labd/workspace/syncer.rs:2499-2502` (`ledger.withheld = plan.withheld`).

**Why it is a fault.** The rule of #190 is that after a break in the watch a
path the guest lacks is never deleted on the host. The plan keeps that rule
across passes with the ledger's `withheld` set. A path enters `plan.withheld`
only as a `MissingUnseen` conflict, or when the pass could not decide it. When
the host also changed the path during the break, the plan calls it a
`ModifiedAndDeleted` conflict and does not put it in `withheld`. The syncer
then replaces the ledger's set with the plan's, so the mark is gone. The break
is over by the next pass. When the developer takes the host edit back (`git
checkout -- file`, `git stash`, an editor undo), the host copy is unchanged
again, the guest copy is absent, and the plan reads that as a deletion the
guest made: `Remove { direction: ToHost }`. Taking an edit back is the natural
answer to "one side changed it and the other deleted it", and it deletes the
file.

A pass that a verb asked for is protected: under a flush the walk holds every
missing guest path back, and that sets the mark (`plan.rs:682-687`). The first
run of the syncer-level probe showed this. The pass a start runs by itself is
not protected.

**Proof.** Probe `review946_p3` (plan level): pass 1 gives the conflict and an
empty `withheld`, pass 2 gives one `Remove` to the host. Probe `review946_p3b`
(the loop): the agreement is borne out by a second start, the syncer stops, the
guest loses `main.rs`, the host edits it, the syncer starts. Result: halt on
"one side changed it and the other deleted it", `withheld` empty on the ledger.
The host file is written back to the agreed content. Result: the host's
`main.rs` is deleted and the halt is gone.

**Test to keep.**
`an_unwitnessed_absence_stays_withheld_when_the_host_also_changed_it`: the
steps of `review946_p3b`, asserting that the host file stays and the workspace
halts on `MissingUnseen`.

**How bad.** Data loss on the canonical copy. It needs a break, a lost guest
file and a host edit of the same path, so it is rare.

### F14. A host directory deleted and made again is not watched

**Where.** `src/labd/workspace/watcher.rs:160-194` (`register`, the test at
line 177), `watcher.rs:252-287` (`decode`).

**Why it is a fault.** The kernel removes an inotify watch when its directory
is deleted. `register` keeps the directory in its map until a scan no longer
lists it. When the directory exists again by the time the next scan runs, the
scan lists it, `register` finds the name in the map and adds no watch. The
descriptor in the map is dead, so nothing in that directory is reported from
then on. `decode` does not handle `IN_IGNORED`, which is the event that says a
watch is gone. The same holds for every directory below the one that was
deleted. The module text says a subtree that quietly stops being watched "is
the silent failure this design refuses".

Deleting and making a directory within one scan is ordinary: `rm -rf dist &&
mkdir dist` in a build script, `git stash -u` with `git stash pop`, a rebase
that removes and replays the commits which added a directory, a branch switch
away and back while a long pass runs. A host edit in such a directory is found
only when another event starts a pass, because a steady pass stats the whole
host tree. Until then it does not sync, and `dev sync status` shows nothing.

**Proof.** Probe `review946_p1` (watcher level): register the root and `d`,
remove `d`, create `d`, register the same list, write `d/new.rs` and `top.rs`.
Result: events for `top.rs` only. Probe `review946_p1b` (the loop): the same on
a seeded workspace, then a save of `d/a.rs`. Result: 3 s later the guest still
holds the old content, and it arrives only after a save of another file.

**Test to keep.** `a_directory_made_again_is_watched_again`: the steps of
`review946_p1`, asserting that `settle()` returns `Touched("d/new.rs")`. It
needs no sleep, because the kernel queues the event inside the write.

**How bad.** Wrong result. Host-to-guest sync of a subtree stops with no word
until an unrelated event.

### F15. A guest file made while a re-seed runs is never seen

**Where.** `src/labd/workspace/syncer.rs:1057-1099` (the re-seed runs with no
watch), `syncer.rs:1150-1159` (`rescan = !std::mem::take(&mut seeded)`),
`syncer.rs:1992-2005` (the probe set), `src/labd/workspace/reseed.rs:278`.

**Why it is a fault.** The re-seed walks the guest once, with the watch closed.
The watch that opens after it is the one open that is followed by no walk. A
steady pass asks the guest only about paths the host holds, the ledger holds or
the watch reported. A file the guest creates after the re-seed's walk and
before the watch opens is in none of those sets. It stays unknown until
something touches it again or the next walk. A restore of an online snapshot
resumes the guest's processes, so a build or an editor that was running at the
capture writes during exactly this time. The re-seed hashes both trees in
full, so the time is long on a large tree, and a failed watch open adds 5 s for
each retry. While the file is unknown, `unsynced` is empty, a capture passes
and `destroy` does not refuse, although the guest holds work the host has never
seen.

**Proof.** Probe `review946_p5`. A hook creates `/src/late.rs` in the guest at
the moment the watch opens after a restore bracket. Two flush passes follow.
Result: `reseed` is over, `rescans` is 0, `unsynced` is empty, the guest holds
`late.rs`, the host does not, and `before_capture` and `before_destroy` both
return `Ok`.

**Test to keep.**
`a_guest_file_made_during_a_re_seed_reaches_the_host`: the same steps,
asserting that the host receives `late.rs`. One fix is to open the watch before
the re-seed's guest walk and to drop what it reports for the paths the re-seed
wrote.

**How bad.** Data loss of guest work when a `destroy` or a restore follows, and
a wrong result until then.

### F16. An oversize guest file is forgotten, and no bracket refuses on it

**Where.** `src/labd/workspace/syncer.rs:2594-2611` (what stays owed),
`syncer.rs:1992-2005` (the probe set), `syncer.rs:2631`,
`src/labd/workspace/bracket.rs:113-136` (`Outstanding::of`).

**Why it is a fault.** A guest file over `workspace_max_file` that the host
does not hold is refused by the size guard, and that pass reports it. The path
is not kept: it is in no agreement, not on the host, and not in `undecided`, so
it is not owed. The next pass does not ask the guest about it, and the new
report has an empty `oversize`. From then on `dev sync status` does not name
the file. An oversize file on the host is found again by every host scan, so
this is the guest's side only. `Outstanding::of` does not read `oversize`
either, not even in the pass that reported it. So a capture, a restore and a
`destroy` all pass while the guest holds a file the host has never taken, and
restore and `destroy` delete it with no `--discard-guest-changes`. PRD §19.6
says `destroy` refuses "where guest work the host has not taken is left, or
could be".

**Proof.** Probe `review946_p8`. The cap is 64 bytes. The guest creates
`dump.sql` with 128 bytes and the watch reports it. Result: one report names
the refusal. After one more pass `oversize`, `unsynced` and `skipped` are all
empty. `before_capture`, `before_restore` without the flag and `before_destroy`
without the flag return `Ok`. The host never held the file.

**Test to keep.** `a_refused_guest_file_stays_reported` (the report still names
it after a second pass) and
`a_restore_and_a_destroy_refuse_on_a_guest_file_the_size_guard_refused`. The
lead may decide that a refused file is not work to keep. Then the first test
stands alone, and the refusal texts should say that such files are lost.

**How bad.** Data loss of guest work. The file is large, so it is costly to
make again.

### F17. A rule line the glob parser refuses fails every pass

**Where.** `src/labd/workspace/ignore.rs:296-301` (`add_line` with `?`),
`src/labd/workspace/scan.rs:127`.

**Why it is a fault.** Each line of a rule file goes to
`GitignoreBuilder::add_line`, and an error ends `read_dir_rules`, the host scan
and the pass. The parser refuses lines that git accepts: a trailing backslash
(`obj\`, which a developer on Windows writes), an unclosed or unopened brace
(`x{y`, `x}y`, which git matches as plain text), and a reversed range
(`[z-a]`). Git 2.56 on helios reads a `.gitignore` with all four, exits 0, and
ignores the files named `x{y` and `x}y`. So a repository that git handles
cannot sync at all, and the reason is in `trouble` only.

A guest reaches this too. A `.gitignore` the guest writes is installed on the
host before its siblings (the fix of F8), and from the next pass the host scan
fails before it looks at the guest. Correcting the file in the guest does not
help, because no pass gets far enough to carry the correction. Somebody has to
edit the file on the host. This is the class of F9. The guest's route was read,
not run.

**Proof.** Probe `review946_p2`. `read_dir_rules` on a directory whose
`.gitignore` holds one line. Result: `obj\` fails with `dangling '\'`, `x{y`
with "unclosed alternate group", `x}y` with "unopened alternate group", `[z-a]`
with "invalid range". `a[b` and `ok/` are read.

**Test to keep.** `a_rule_line_the_parser_refuses_is_a_named_skip`: the same
lines, asserting that the scan completes, the other lines of the file apply,
and the pass names the file and the line.

**How bad.** Wrong result: the workspace stops until the host is cleaned by
hand.

### F18. A rule file has no bound, and the prune list is quadratic in its lines

**Where.** `src/labd/workspace/ignore.rs:204-206` (`read_to_end`),
`ignore.rs:296-302`, `ignore.rs:411-444` (`prune_list`, the nested loop at
lines 435-443), `src/labd/workspace/syncer.rs:2475`.

**Why it is a fault.** A rule file is read whole, and every literal line
becomes a prune prefix. The only limit is the size guard on the file itself,
256 MiB by default. `prune_list` runs every pass. It asks `verdict` for each
prefix, and its last step compares every prefix with every other. It is called
from the pass directly, not from `spawn_blocking`, so it holds a runtime worker
of the lab daemon, and the pass holds the reconciliation lock of the host root,
which a second machine on the same workspace waits for. The list is also saved
in the ledger and sent to the guest each time the watch opens. A guest writes
the file, and the syncer installs it on the host (F8's fix puts it first).

**Proof.** Probe `review946_p9`. A `.gitignore` of 2 000 lines `genN/` (17 kB)
gives 2 000 prefixes after 141 ms. One of 8 000 lines (71 kB) gives 8 000
prefixes after 2.1 s. Four times the lines cost fifteen times the time. By the
same growth a file of 1 MiB costs minutes for each pass. The times are from a
debug build on helios and are evidence of the growth, not limits.

**Test to keep.** `a_rule_file_over_the_bound_is_refused_by_name` (a byte and
line bound in `read_rule_file`, reported like F2's refusal), and
`the_prune_list_is_bounded` (more prefixes than the bound gives a shorter list,
which is safe: pruning less only costs watch descriptors).

**How bad.** Security against a hostile guest: CPU of the host process, and
every machine that shares the workspace root waits.

### F19. F4's fix is incomplete: a refused symlink or a case collision holds a re-seed for ever

**Where.** `src/labd/workspace/reseed.rs:224-226` (`Reconverged::complete`),
`reseed.rs:344-348` (`symlinks_refused` joins `failures`),
`src/labd/workspace/syncer.rs:1070-1082`.

**Why it is a fault.** F4 was that conditions the steady state accepts for
ever also held the re-seed for ever. The fix took skips and size refusals out
of `complete()`. Two such conditions are still in it. Both are the documented
degradations of a Windows guest whose default login has `elevated = false`
(`windows.rs`, `Preconditions::degradations`):

- **A symlink the guest refuses.** In the steady state it is a warning: the
  module text of `syncer.rs` lists "a refused symlink" among the things that
  "warn and carry on". In a re-seed it is a failure.
- **A case collision.** In the steady state the colliding paths are refused by
  name and everything else syncs. In a re-seed `complete()` is false while one
  exists.

On such a guest one symlink in the repository, or two paths that differ only in
case, means that after any snapshot restore the re-seed never completes. The
loop runs it again every 5 s, hashing both trees in full, the watch never
opens, and nothing syncs in either direction.

**Proof.** The test in the tree
`an_incomplete_reseed_keeps_the_loop_and_restart_behind_the_barrier`
(`syncer.rs:5667`) uses `guest.refuse_symlinks()` as its failure and asserts
that the barrier stays, across passes and a restart. It was written for a
failure that can end. It pins the state for one that cannot. The collision case
was read, not run.

**Test to keep.**
`a_re_seed_completes_beside_a_refused_symlink_and_a_case_collision`: a guest
that refuses symlinks and folds case, a host tree with one link and one
colliding pair, a restore bracket. Assert that the re-seed completes, the watch
opens and both are reported. The existing test needs another failure, for
example `fail_push`.

**How bad.** Wrong result: sync stops for good after a restore on a degraded
Windows guest.

### F20. A guest symlink whose `lstat` size is not its target length cannot be replaced

**Where.** `src/labd/workspace/scan.rs:534-536` (the probe sets a link's size
to the length of its target), `src/labd/workspace/apply.rs:612-616` and
`apply.rs:654-657` (`verify_copy` compares that with the size `lstat` gives),
`guest/agent/src/fileops.rs:694` (`size: meta.len()`).

**Why it is a fault.** Before an action overwrites or removes a guest path,
`verify_copy` checks that the guest still holds what the scan saw. For a
symlink the scan's size is the length of the target string, and the current
size is what the agent's `lstat` reports. On Linux the two are equal. The
agent reports `Metadata::len()` of the link, and for a symlink on NTFS that is
0. Then the check fails with "destination changed after scanning; rescan
required" for every host change or removal of a synced link, on every pass.
`trouble` stays set, the loop runs a pass every 5 s, and a capture is refused.
The first placement of a link works, because nothing is there to verify.

**Proof.** Probe `review946_p10`. The fake guest reports size 0 for
`/src/link`. The seed places the link with no trouble. The host then points the
link at another target. Result after four passes: the guest link is unchanged
and `trouble` is "1 workspace actions failed: link: link: destination changed
after scanning; rescan required". That NTFS reports 0 is from the Rust standard
library's behaviour as I know it. It was not run on a Windows guest.

**Test to keep.** `a_guest_link_is_compared_by_its_target_not_its_lstat_size`:
the same steps, asserting that the guest link gets the new target.

**How bad.** Wrong result on Windows guests, if the size is 0 there as
modelled.

## Notes

These have no test proposed, so they are not in the list.

- **N14. A pulled file gets the guest's permission bits, with no host umask**
  (`src/labd/workspace/guest.rs:113-117`, `guest.rs:174-180`). A guest file
  with mode 666 or 777 is world-writable on the host. A guest file whose owner
  cannot read it cannot be verified after the pull. Mode-only changes never
  sync, so a later `chmod` on one side does not repair the other. Not run: the
  fake guest does not model modes.
- **N15. The prune list also covers a file.** The agent drops every report at
  or below a prefix (`guest/agent/src/watch/mod.rs:177-183`). A prefix comes
  from the rule text and is kept when the name is ignored as a directory
  (`ignore.rs:422`). With the rule `build/` and a synced file named `build`,
  guest changes of that file are never reported. A later pass finds them by
  `stat`.
- **N16. `dev sync resolve` with a path that matches nothing is dropped with
  no error** (`src/labd/lab.rs:3670-3693`, `syncer.rs:428-430`). `./a.rs` or a
  typed mistake leaves the halt as it was, and the verb says nothing about the
  path.
- **N17. The limit F6's fix left is easy to reach.** A walk carries changed
  guest files with no quiet period. The syncer's own `mkdir` on the host is a
  directory event, and every directory event is a walk
  (`watcher.rs:268-270`). So a guest that makes a directory and then writes a
  file slowly gets that file carried in the middle of the write. Not run.
- **N18. `create_dir_all` in `place_on_host` does nothing**
  (`apply.rs:837-840`). The path is `/proc/self/fd/<n>/<name>`, so the parent
  always exists, and a missing parent fails earlier in `host_path`. A file
  whose directory is still in its quiet period fails the pass once and is
  retried 5 s later.
- **N19. A lock that is never released costs a pass every second**
  (`syncer.rs:1326-1328`). Each pass scans the host tree and asks the guest
  about every agreed path. A stale `.git/index.lock` after a crashed git does
  this until somebody removes it.
- **N20. `dev sync diff` reads a named host path with `std::fs::read` when it
  is not a directory or a link** (`diff.rs:211`). A FIFO at that path blocks
  the verb. The caller is the host user.
- **N21. After a re-seed, a rolled-back guest file over the size cap stays in
  the guest** (`reseed.rs:101-111`). When the cap is raised later and the host
  does not hold that path, the file is carried to the host as a new guest
  file. That is content of the rolled-back tree on the canonical copy. Not run.
- **N22. The host scan hashes with a plain `File::open`** (`scan.rs:575`),
  where the apply side uses `O_NOFOLLOW`. The passes of every machine on one
  root are serialised, so the guest cannot change the entry between the `lstat`
  and the open. Another host process can.
