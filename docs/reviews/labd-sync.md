# Review: labd workspace sync

Card #380. A bug-hunting review of the workspace syncer. It lists faults and
makes no fix. The base commit is `47dabad7` (`next` on 2026-11-21).

## What was read, and what was not

Read in full, tests excluded:

- `src/labd/workspace/`: `syncer.rs`, `plan.rs`, `apply.rs`, `scan.rs`,
  `ignore.rs`, `ledger.rs`, `guest.rs`, `watcher.rs`, `locks.rs`, `reseed.rs`,
  `bracket.rs`, `halt.rs`, `diff.rs`, `windows.rs`.
- `guest/agent/src/watch/`: `mod.rs`, `inotify.rs`, `rdcw.rs`, `records.rs`.
- The callers of the snapshot bracket in `src/labd/lab.rs` (`snapshot`,
  `restore`, the freeze clone).
- PRD §19.6, the part on snapshots and ignore changes.

Read in part:

- `src/labd/vm_agent.rs`: only `pull`, `pull_to` and `readdir` of the file
  client, and how `WatchSession` turns records into reports.
- `guest/agent/src/fileops.rs`: only the operation table (lines 277 to 490).
- The test modules: the names of the syncer tests, and five test bodies.

Not read: the file client's other calls, the agent's path resolver and handle
table, the agent's Windows and macOS platform code, the `agent-proto` codec, the
agent mux, `src/cli/dev.rs`, and the wiring that builds a `Workspace` in
`lab.rs`. A fault in those is not in this document.

## How the findings were proved

Every finding F1 to F11 was run, not only read. Eleven temporary tests were
added to the syncer's test module in the checkout on helios, run, and removed.
They are not committed. All eleven passed, and each passes only while its fault
is present. Their source is in `docs/reviews/labd-sync-probes.md`, with the
command. Each finding below names its probe and proposes the test a fix should
leave behind.

The probes use the in-memory guest (`FakeGuest`), not a VM. Where a finding
depends on what a real guest or agent does, the finding says so.

## Findings, worst first

| Code | Fault | How bad |
|---|---|---|
| F1 | A guest directory that cannot be listed during a walk reads as deleted files, and the host copies are removed (fixed in #384) | Data loss (host) |
| F2 (fixed, card #385) | The host follows a guest-planted symlink named like a rule file, and sends the lines of the target back to the guest | Security against a hostile guest, and crash of the host process |
| F3 | A restore does not flush and does not refuse on unsynced guest work | Data loss (guest work) |
| F4 | A re-seed never completes while the tree holds a socket, an unreadable path or an oversize file | Wrong result (sync stops for good) |
| F5 | A halted workspace keeps every path the guest reports, without a bound and without checking it is a path | Security against a hostile guest (host memory and CPU) |
| F6 | A guest or host file that changed without a settled event is carried anyway, so the quiet period and the git lock deferral are bypassed | Wrong result (torn copy on the host) |
| F7 | A guest delete made while the watch reopens for a new prune list is undone | Wrong result |
| F8 | An ignore rule written in the guest applies one pass late, so the output it ignores is copied to the host and stays | Wrong result |
| F9 | A directory named `.gitignore` or `.vmlabignore` fails every pass | Wrong result (sync stops) |
| F10 | A host file name that is not UTF-8 is trouble on every pass, and refuses every capture | Wrong result |
| F11 | A directory one side deleted comes back when the other side keeps ignored content in it | Cosmetic |

### F1. A failed guest `readdir` during a walk deletes host files

**Fixed in #384.** Reconciliation defers the descendants of a guest
directory it could not list. A failed root listing or a channel failure
aborts the walk.

**Where.** `src/labd/workspace/scan.rs:362-379` (`guest_walk`),
`src/labd/workspace/syncer.rs:1758-1764`, `src/labd/workspace/plan.rs:750-767`.

**Why it is a fault.** When `readdir` of one guest directory fails, the walk
records one skip for that directory and goes on. The pass then puts only the
directory itself into `undecided`. The agreed files below it are not in the
guest tree and not in `undecided`, so `reconcile` reads each as "the guest
deleted it" and plans `Remove { direction: ToHost }`. A host removal needs no
guest call (`apply.rs:472-484`), so it succeeds. The comment at `scan.rs:370`
says an unreadable directory must not read as "the guest having deleted
everything under it". That is what happens one layer up.

The walk that follows a break in the watch is protected: there the absence is a
`MissingUnseen` conflict. A flush is protected by `syncer.rs:1737-1751`. The
unprotected walks are the ones with `unwitnessed == false`: the walk after a
guest watch overflow, and the walk after a host-side `HostEvent::Rescan`. The
host watcher sends that for every new host directory (`watcher.rs:276-278`), and
the agent overflows on every guest directory rename (`inotify.rs:143-145`), so
these walks are common.

Two ways to get there:

- The login cannot list a directory: `chmod 000`, or a directory a `sudo` build
  step left owned by root.
- Any other `readdir` error. The match arm at `scan.rs:373` takes every error
  that is not a `GuestListingError`. A file session that dies in the middle of
  the walk (agent restart, guest reboot) fails every later `readdir` the same
  way, so every directory not yet listed is skipped and the walk still returns
  `Ok`. This second way was read, not run.

The bulk-delete guard is the only limit. It holds back more than 20 removals
that are also more than half of the ledger (`plan.rs:293-295`). Up to 20 files,
or up to half of a larger workspace, are removed from the host with no halt. A
skipped root (`"."`) empties a workspace of 20 paths or fewer.

**Proof.** Probe `review380_f1`. Seed `lib/a.rs` and `main.rs`, make
`/src/lib` unreadable in the guest, overflow the watch. Result: host
`lib/a.rs` deleted, guest still holds it, no halt.

**Test to keep.** `an_unreadable_guest_directory_never_deletes_host_files`: the
same steps, asserting the host file stays and the skip is reported. A second
test at `scan.rs` level: a `readdir` that fails with a channel error fails the
walk.

**How bad.** Data loss on the canonical copy.

### F2. A guest-planted rule-file symlink discloses host files to the guest

**Status.** Fixed by card #385. `read_dir_rules` opens a rule file with
`O_NOFOLLOW` and never reads one that is a symlink. The decision on a
legitimate in-tree link: it is refused too, because its target can be a
guest-owned path (`.env`) the guest was never given, and a rule about which
targets are safe would need the rules that are being read. The link still syncs
as a link. Its rules do not apply, and every pass names it in the skipped list
and in a `workspace.skipped` event, without holding the pass or a re-seed open.
The tests are `a_symlinked_rule_file_is_never_read_through` (syncer),
`a_symlinked_rule_file_is_refused_by_name_and_never_read` and
`a_rule_file_linked_to_an_endless_file_is_not_opened` (ignore rules). F9 is
not fixed by this change: a directory named like a rule file still fails the
scan. The text below describes the code before the fix.

**Where.** `src/labd/workspace/ignore.rs:157-192` (`read_dir_rules`, the read at
line 169), `ignore.rs:354-396` (`read_line`), `ignore.rs:289-322`
(`prune_list`), `src/labd/workspace/syncer.rs:961`.

**Why it is a fault.** `read_dir_rules` reads `.vmlabignore` and `.gitignore`
with `std::fs::read`, which follows symlinks. A symlink is synced guest to host
verbatim (`apply.rs:517-540`). So a guest creates `.vmlabignore` as a symlink
to any absolute host path, the syncer places that link in the canonical tree,
and the next host scan reads the target as rules. Every line of it with no
glob character becomes a prune prefix, and the prune list is sent to the
guest's agent when the watch reopens. The guest reads lines of a host file that
the lab daemon's user can read: `/etc/passwd`, or the body of a private key,
whose base64 lines have no glob character.

Two more effects of the same read, read and not run:

- A link to `/dev/zero` makes `std::fs::read` allocate until the lab daemon is
  killed. That is a crash of the host process, on every start of that syncer.
- A link to a large or binary file makes every pass fail or stall.

A fix has to keep a legitimate in-tree `.gitignore` symlink in mind. That is a
decision for the fix card.

**Proof.** Probe `review380_f2`. A host file outside the workspace holds
`hunter2-token` and `second/line`. The guest gets `/src/.vmlabignore` as a
symlink to it. Result: the second watch opens with the prune list
`["hunter2-token", "second/line"]`.

**Test to keep.** `a_symlinked_rule_file_is_never_read_through`: the same
steps, asserting no watch is opened with a line of the outside file, and that
the pass names the refused rule file.

**How bad.** Security against a hostile guest (host file disclosure), and
crash of the host process.

### F3. A restore neither flushes nor refuses on unsynced guest work

**Where.** `src/labd/workspace/syncer.rs:571-591` (`before_restore`),
`src/labd/lab.rs:3519-3521`.

**Why it is a fault.** PRD §19.6 says: "A pre-flight flush brackets capture as
well as restore. If the guest has unsynced work — channel down, sync paused,
guest writing faster than it drains — halt and surface."
`before_capture` does this: it runs a pass and refuses on `unsynced`,
`trouble`, `rescan`, `reseed` and `unanswered`. `before_restore` runs no pass.
It reads the last published report and refuses only on a halt. Guest work that
is inside its quiet period, owed after a failed pass, waiting on a git lock, or
behind a rescan is discarded by the re-seed, and `--discard-guest-changes` was
never asked for. A conflict that arose after the last pass is not seen either,
because the report is stale.

**Proof.** Probe `review380_f3`. After a clean pass the guest edits `main.rs`
and creates `new.rs`, and the watch reports both. `before_restore` without the
flag returns `Ok`. After the bracket, the guest's `main.rs` is the host content
again, the guest's `new.rs` is gone, and the host never received either.

**Test to keep.** `a_restore_flushes_first_and_refuses_on_unsynced_guest_work`,
the mirror of the existing
`a_capture_flushes_first_and_refuses_on_unsynced_guest_work`, plus the case
where the flag is given.

**How bad.** Data loss (guest work that never reached the host).

### F4. A re-seed never completes while the tree holds a path it skips

**Where.** `src/labd/workspace/reseed.rs:193-199` (`Reconverged::complete`),
`reseed.rs:326`, `src/labd/workspace/syncer.rs:884-899`.

**Why it is a fault.** A re-seed is complete only with no skipped path, no
oversize file, no collision and no failure. A socket or FIFO in the tree is a
skip (`scan.rs:146-151`), and so is a file the login cannot read. The steady
state treats all of these as a warning: the module text says a `.sock` in the
tree "must not stop a dev machine". After a restore the same tree keeps
`reseed_owed` set. The loop sleeps 5 s and runs the whole re-seed again, which
hashes both trees in full, and it does that until the socket is gone. The watch
never reopens and nothing syncs in either direction. A packfile over
`workspace_max_file` (256 MiB by default) has the same effect, and so does git's
`fsmonitor--daemon.ipc` socket under `.git`.

The incomplete state is deliberate (the test
`an_incomplete_reseed_keeps_the_loop_and_restart_behind_the_barrier` holds it).
The fault is that conditions the steady state accepts for ever also hold it for
ever.

**Proof.** Probe `review380_f4`. A Unix socket sits beside `main.rs` in the
host tree. Before the restore the report shows no halt and no trouble, with the
socket as a named skip. Eleven seconds after the bracket: 3 passes, `reseed`
still set, the watch not reopened, `reseed_owed` still on the ledger.

**Test to keep.** `a_re_seed_completes_beside_a_socket_and_an_oversize_file`:
the same tree, asserting the re-seed completes, the watch reopens, and both
paths are reported.

**How bad.** Wrong result. Sync stops for good after a restore, and the host
and the guest hash the whole tree every 5 s.

### F5. A halted workspace keeps every path the guest reports

**Where.** `src/labd/workspace/syncer.rs:1262-1270` (a `Batch` goes into the
debounce set unchecked), `syncer.rs:2120-2126` (`owed` while halted),
`syncer.rs:1137-1150` (`unsynced`).

**Why it is a fault.** A batch entry's path is taken as it comes. Nothing
checks that it is a relative path with valid components. That check happens
only at apply time (`apply.rs:589-591`). While the workspace is halted, the
pass hands back every drained path as owed, so a path that exists on neither
side is never dropped. Each pass then probes every one of them in the guest,
and each report clones the whole list. A guest that has a conflict (which it
can make at will) and then reports unique names grows the lab daemon's memory
and the cost of each pass without a bound, up to 32 MiB of names for each batch
(`MAX_RECORD`). One lab daemon serves every machine of the lab.

Outside a halt, one invalid path (`""`, `/etc/passwd`, `../x`) fails at apply
on every pass. That keeps `trouble` set, runs a full pass every 5 s, and
refuses every capture. This part was read, not run.

**Proof.** Probe `review380_f9`. A workspace halts on a seed conflict. The
watch reports 3000 names that exist nowhere, and `../../outside`,
`/etc/passwd` and `""`. Two passes later the report still holds 3004 unsynced
paths.

**Test to keep.** `a_reported_path_that_is_not_a_path_is_dropped_on_receipt`
and `a_halt_does_not_keep_paths_that_exist_on_neither_side`.

**How bad.** Security against a hostile guest: memory and CPU of the host
process.

### F6. A change with no settled event is carried, so the debounce is bypassed

**Where.** `src/labd/workspace/syncer.rs:1666-1678` (the probe set is the whole
host tree and the whole ledger), `syncer.rs:1211-1217` (the `select!`),
`syncer.rs:1779-1791` (guest locks are known only from the watch).

**Why it is a fault.** `watcher.rs` states the rule: "a path is only reconciled
once it has stopped moving". The pass does not hold it. A steady-state pass
stats every agreed path on both sides, and it hashes and carries any path whose
size or mtime moved. Only paths already in a debounce set are held back as
`in_flight`. A file whose event has not reached the loop yet is not in that
set. That is the normal state for a moment: the loop reads one event for each
turn of the `select!`, and a ready timer can start a pass while events wait in
the channel. So a pass that another path asked for carries a file in the middle
of being written. The transfer checks the bytes against the digest taken at
scan time, so what lands is a consistent copy of a half-written file. The next
event repairs it, unless the guest stops first.

The same gap bypasses the `.git` lock deferral. A guest-side lock is known only
after the watch has named it. A pass that runs while guest git holds
`index.lock`, and before that name has been drained, carries `HEAD`, `index`
and `refs/` in the middle of the operation.

**Proof.** Probe `review380_f5`. The guest rewrites `main.rs` and the watch
reports nothing. An unrelated host edit of `other.rs` triggers a pass. Result:
the host's `main.rs` holds the guest's new bytes, and the guest watch was
drained 0 times.

**Test to keep.**
`a_guest_change_found_by_the_probe_waits_out_a_quiet_period` and
`a_git_lock_found_by_the_probe_defers_the_mutable_set`.

**How bad.** Wrong result: a torn file, or a `.git` state that never existed,
on the canonical copy for a time.

### F7. A guest delete across a prune-list reopen is undone

**Where.** `src/labd/workspace/syncer.rs:1152-1161` and `syncer.rs:966-975`
(`unwitnessed = rescan` on every open), `src/labd/workspace/plan.rs:716-735`.

**Why it is a fault.** When the prune list changes, the syncer closes the
guest watch and opens a new one. It marks that reopen `same_guest`, so the walk
bears no agreement out, but it still sets `unwitnessed`. The plan then treats
an agreed path the guest lacks as an absence nothing saw. The guest was up and
the developer deleted the file. If the agreement was made in this run, the
file is copied back into the guest. If it was confirmed by an earlier start,
the workspace halts with "nothing saw it deleted". A delete that the old watch
had already reported, and that waits in the debounce set, gets the same
treatment. A branch switch in the guest that changes `.gitignore` and deletes
files does both things at once.

**Proof.** Probe `review380_f8`. The host adds `.vmlabignore` with `build/`.
The guest deletes `old.rs` at the moment the watch reopens. Result: the guest
holds `old.rs` again, the host still holds it, and there is no halt.

**Test to keep.** `a_guest_delete_across_a_prune_reopen_reaches_the_host`.

**How bad.** Wrong result. The delete is lost without a word, or the workspace
halts on a file the developer removed.

### F8. An ignore rule written in the guest applies one pass late

**Where.** `src/labd/workspace/syncer.rs:1637-1640` (rules come from the host
scan only), `syncer.rs:1672-1676`, `syncer.rs:1815-1824`.

**Why it is a fault.** The rules are read from the host tree at the start of a
pass. A `.gitignore` the guest has just written is not on the host yet, so the
pass that carries it also carries everything it ignores. The next pass reads
the rule, drops those paths from the ledger, and leaves the copies on the host
for ever. Authoring happens in the guest, so `cargo new` then `cargo build`, or
a guest-side branch switch, reaches this. The host ends with guest build output
that nothing removes, and the transfer can be the large one the rule was
written to avoid.

**Proof.** Probe `review380_f6`. The guest writes `.gitignore` with `target/`,
and `target/out.bin`, in one batch. Result: the host holds `target/out.bin`,
and the ledger no longer has it.

**Test to keep.**
`a_rule_file_the_guest_wrote_is_applied_before_its_siblings_are_carried`.

**How bad.** Wrong result.

### F9. A directory named like a rule file fails every pass

**Where.** `src/labd/workspace/ignore.rs:169-173`,
`src/labd/workspace/scan.rs:115`.

**Why it is a fault.** `read_dir_rules` treats every read error but `NotFound`
as fatal. A directory named `.gitignore` or `.vmlabignore` answers
`Is a directory`, the host scan fails, and the pass fails before it looks at
the guest. A guest can make one with `mkdir`: it syncs to the host as a
directory, and from then on every pass fails. Removing it in the guest does not
help, because no pass gets far enough to see that. Somebody has to remove it on
the host.

**Proof.** Probe `review380_f2b`. `host_scan` of a tree with a directory
`.gitignore` returns `reading …/.gitignore: Is a directory (os error 21)`.

**Test to keep.** `a_rule_file_that_is_not_a_file_is_a_named_skip`.

**How bad.** Wrong result: the workspace stops until the host is cleaned by
hand.

### F10. A host file name that is not UTF-8 is trouble on every pass

**Where.** `src/labd/workspace/scan.rs:125`.

**Why it is a fault.** The scan turns each name into a `String` with
`to_string_lossy`. A name that is not UTF-8 enters the tree under a name with
`U+FFFD` in it, which no file has. The plan carries it to the guest, the apply
cannot open it, and it fails on every pass. `trouble` stays set, the loop runs
a full pass every 5 s, and every capture is refused. Two such names can also
fold onto one key. The module's rule for a path it cannot handle is a named
skip.

**Proof.** Probe `review380_f10`. The host holds `caf\xe9.txt`. Two passes in
a row report `1 workspace actions failed: caf�.txt: No such file or directory`,
`skipped` is empty, and `before_capture` refuses.

**Test to keep.** `a_host_name_that_is_not_utf8_is_a_named_skip`.

**How bad.** Wrong result.

### F11. A directory left standing comes back on the side that deleted it

**Where.** `src/labd/workspace/apply.rs:278-281`.

**Why it is a fault.** When one side deletes a directory and the other still
holds ignored content in it, the removal is "left standing" and the agreement
is dropped. On the next pass the directory exists on one side with no
agreement, so it reads as created there and is made again on the side that
deleted it. The comment says a removal that cannot succeed must not be retried
on every pass. It is not retried. It is reversed.

**Proof.** Probe `review380_f7`. The host holds `crate/a.rs` and an ignored
`crate/target/host.o`. The guest deletes `crate/a.rs` and `crate`. Result: the
host's `a.rs` is removed, and the guest's `crate` directory is back.

**Test to keep.** `a_directory_left_standing_is_not_made_again`.

**How bad.** Cosmetic: an empty directory returns.

## Notes

These have no test proposed, so they are not in the list. Some are design
consequences that the lead may want stated.

- **N1. `.git` syncs both ways, so the guest can run code on the host.** A
  guest can write `.git/config` (`core.fsmonitor`, `core.sshCommand`) or
  `.git/hooks/*`. The next host-side git command runs it as the developer. This
  follows from the design in `locks.rs`. For a guest that is not trusted it is
  the largest exposure in the subsystem, larger than F2.
- **N2. Symlinks from the guest land on the host with any target**
  (`apply.rs:517-540`). The syncer never follows one. F2 is the one place it
  did. Host tools will follow them.
- **N3. The check before a rename is not atomic** (`apply.rs:580-581` says so).
  A write to the destination between `verify_copy` and the rename is
  overwritten. The window is one round trip.
- **N4. The loop ends without a word when the host watcher thread ends**
  (`syncer.rs:1232`, `watcher.rs:235-241`). No event is emitted, the syncer
  stays in the running map, and a `dev sync` verb then waits 120 s. A `poll`
  or `read` error other than `EINTR` and `EAGAIN` gets there.
- **N5. Every walk hashes both trees in full** (`syncer.rs:1628-1629` clears
  the scan cache). With F1's triggers (each new host directory, each guest
  directory rename) that is a full hash for a `mkdir`.
- **N6. The ledger is saved with no `fsync`** (`ledger.rs:308-321`). After a
  host crash it can be empty. An empty ledger loads safely, but `reseed_owed`
  and `halted` are lost with it. The paths that differ then halt as conflicts
  instead of being overwritten, so this is not a loss.
- **N7. Host names a Windows guest cannot hold are not refused before
  transfer.** `validate_entry_name` runs with `windows = false` on host paths
  (`apply.rs:78`). A host name with `\` reaches the agent as a separator, one
  with `:` as an alternate data stream. Not run: the fake guest does not model
  either.
- **N8. A filesystem unmounted inside the guest workspace** makes the agent
  forget the watch with no mark and no overflow (`inotify.rs:110-121`). The
  next pass reads the files as deletes that were seen.
- **N9. A resolution `--guest` cannot land** where the host copy is oversize
  (no digest, `apply.rs:652-655`) or changed kind since the agreement
  (`plan.rs:761-765` removes by the agreed kind). It fails on every pass.
- **N10. `dev sync diff` joins the caller's path onto both roots with no
  check** (`diff.rs:145-146`). The caller is the host user, so this is not a
  boundary.
- **N11. The agent's `Digest` follows a final symlink**
  (`guest/agent/src/fileops.rs:471-475`), where every other call of the syncer
  does not. The effect stays in the guest.
- **N12. The `(size, mtime)` pre-filter has git's "racily clean" gap.** A
  second write of the same size in the same timestamp tick as the hash is not
  seen until the file changes again. The quiet period makes this unlikely.
- **N13. The flush path sleeps `QUIET` without watching `stop`**
  (`syncer.rs:1024`). A stop waits up to 250 ms more.
