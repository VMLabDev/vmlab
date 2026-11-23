# Review: dev machines

Card #672. A bug-hunting review of dev machines: the `@dev` declaration, the
`vmlab dev sync` verbs, and the lab daemon paths that start, stop, capture,
restore and destroy a machine with a workspace. It lists faults and makes no
fix. The base commit is `6eaa1a92` (`next` on 2026-11-23).

The workspace syncer's pass was reviewed before, in `docs/reviews/labd-sync.md`
(card #380, base `47dabad7`). This review does not repeat it. It covers what
that review named as not read: `src/dev`, `src/cli/dev.rs`, and the wiring in
`src/labd/lab.rs` that builds a `Workspace` and brackets a snapshot.

## What was read, and what was not

Read in full, tests excluded:

- `src/dev/mod.rs` and `src/dev/select.rs` (with their tests).
- `src/cli/dev.rs`.
- `src/labd/workspace/diff.rs`, `bracket.rs`, and `guest.rs` up to the fake
  guest.
- In `src/labd/lab.rs`: `WorkspaceSessions` (172-270), `make_clone`
  (1470-1515), `stop_machine` and `destroy_machine` (1545-1610), the stop of a
  lab (3030-3160), `dev_machines`, `start_workspaces`, `workspace_of` and the
  three `workspace_*` verbs (3170-3380), `snapshot`, `restore_all`, `restore`
  and `check_restore` (3550-3895), `start_machine` (1366-1408).
- In `src/labd/lab/reload.rs`: `settle_pending`, `rebind_workspace`, `pending`.
- In `src/labd/workspace/syncer.rs`: the lock, `Syncer`, `WorkspaceSyncers`
  and the loop `run` up to the end of its `select!` (100-230, 340-1260).
- `check_dev_arguments` and `check_dev_defaults` in `src/config/validate.rs`.
- PRD §19.6 and §19.7, and the paragraph of §7.5 on a share and the lab's
  state.

Read in part:

- `src/labd/workspace/apply.rs`: the host-side half (`host_path`, `onto_host`,
  `verify_copy`, `place_on_host`). It confines every host write with `openat2`
  under `BENEATH | NO_SYMLINKS`. I found no fault there.
- `src/labd/workspace/plan.rs`: `reconcile` from its start to the one-sided
  case, `settle_both`, and the bulk-delete guard.
- `src/labd/workspace/ledger.rs`: the fields, `load`, `save`, `mark_rewound`,
  `rebase`. `scan.rs`: `host_scan`. `halt.rs`. `ignore.rs`: the floor and
  `validate_entry_name`.
- `src/labd/vm_agent.rs`: `pull_to`, `FileSink`, `readdir`, `call_within`.
- `src/cli/lab_move.rs`: the ledger rebase.

Not read: the body of `pass` after its first 50 lines, `reseed.rs`,
`watcher.rs`, `locks.rs`, `windows.rs`, the rest of `scan.rs` and `ignore.rs`,
the guest half of `apply.rs`, the agent (`guest/agent`), `labd/identity.rs`,
`labd/freeze_clone.rs`, the wscript rung (`as_login`), the rendering of
`vmlab status`, the e2e suite and the manual. A fault in those is not in this
document.

## How the findings were proved

F1 to F9 were run. Ten temporary tests were committed on the card's branch
(`1e15e53b`), run on helios, and removed by a later commit of the branch. All ten passed,
and each passes only while its fault is present. Their source and the command
are in `docs/reviews/dev-machines-probes.md`. F10 was read, not run.

The probes use the in-memory guest (`FakeGuest`) and the machine double
(`FakeMachine`), not a VM. Where a finding says more than its probe showed, it
says "read, not run".

## Findings, worst first

| Code | Fault | How bad |
|---|---|---|
| F1 | A workspace may hold the lab's state directory, and a rule the guest writes brings it into the sync (fixed, card #690) | Security against a hostile guest |
| F2 | A restore while the host workspace directory is away skips the halt check and leaves no re-seed note | Data loss (canonical copy) |
| F3 | A running dev machine with no syncer is treated as stopped, so capture and restore pass without a flush | Data loss (guest work) |
| F4 | A stop takes no last pass and records nothing about owed work, so an offline restore discards it without the flag | Data loss (guest work) |
| F5 | `destroy` does not ask about a halted or unsynced workspace | Data loss (guest work) |
| F6 | One machine's pass holds a host-wide lock across its guest I/O | Security against a hostile guest or peer, and wrong result |
| F7 | `dev sync diff` reads a guest file of any size into the lab daemon's memory (fixed, card #696) | Security against a hostile guest, and crash |
| F8 | Guest file names and guest file text reach the developer's terminal with their escape sequences | Security against a hostile guest |
| F9 | `dev sync diff` calls a symlink and a file identical | Wrong result |
| F10 | The refusal for a stopped machine names two verbs that cannot answer for a stopped machine | Cosmetic |

F1 is first because it needs only a common declaration (`workspace = "."`).
F2 loses more, the canonical copy, but needs the host directory to be away at
the moment of a restore.

### F1. A workspace may hold the lab's state directory

**Where.** `src/config/validate.rs:2174-2181` (`check_dev_arguments`),
`src/config/validate.rs:3351` (a test that holds `workspace = "."` to be
valid), `src/labd/lab.rs:3267-3300` (`workspace_of`),
`src/labd/workspace/ignore.rs:61` (the floor).

**Why it is a fault.** PRD §7.5 refuses a share whose host directory holds the
lab's state directory (`.vmlab`) or lies inside it: "a guest that reached it
would not be confined to the declared directory". The comment on
`check_dev_arguments` says a workspace is "held to what a share is held to".
It is not. The check asks only that the directory exists. `workspace = "."` in
a lab root is accepted, and so is a parent of the lab root or an absolute
path, because `root.join(workspace)` is not held under the root.

The syncer has no rule of its own for the state directory. The floor is
`.vmlab-sync*` and `.git/**/*.lock`. So with `workspace = "."`:

- With no ignore rule, the first pass carries `.vmlab/` into the guest: the
  lab's `state.json`, every machine's sync ledger, the SMB credentials of
  other machines (`.vmlab/smb/machines/<machine>/creds`), and every disk
  under `workspace_max_file`.
- With `.vmlab/` in `.gitignore`, which the PRD recommends, the directory is
  guest-owned and stays out. The guest can undo that. `.vmlabignore` syncs
  both ways and beats `.gitignore`. A guest writes `.vmlabignore` with
  `!.vmlab/`, the next host scan reads it, and the state directory is in
  scope.
- The sync is bidirectional. Once a state file is agreed, a guest edit of its
  copy is "guest changed, host unchanged" and is written over the host file.
  That reaches `state.json`, the ledgers, and the `smb.conf` that `smbd` reads
  again when it changes (§7.5).

A lab that sets `VMLAB_WORK_DIR` keeps its state outside the root and is not
affected.

**Proof.** Probe `review672_p2`. The host tree holds `.gitignore` with
`.vmlab/`, and `.vmlab/smb/creds` with `host-secret`. After the seed the guest
does not hold the file. The guest writes `.vmlabignore` with `!.vmlab/` and
`!.vmlab/**`. Result: the guest holds `host-secret`. The guest then rewrites
its copy, and the host file holds the guest's bytes. No halt. The probe uses a
stand-in file under `.vmlab/`, with the real ledger in another directory.

**Test to keep.** In `validate.rs`:
`a_dev_workspace_over_the_lab_state_is_refused`, with `workspace = "."`, a
parent of the root, and a path inside `.vmlab`. In the syncer:
`the_lab_state_directory_never_syncs_whatever_the_rules_say`, the steps of the
probe with the assertions turned round.

**How bad.** Security against a hostile guest: it reads host files outside
what it was given, and writes the lab's own state.

**Fixed by card #690.** Three changes. `check_dev_arguments` holds the host
`workspace` to the share rule (`share_over_lab_state`): a workspace that holds
the lab's state directory or lies inside it is a validation error, compared as
the kernel resolves the paths, so `.`, `..`, an absolute parent and a symlink
into `.vmlab` are all refused. `workspace_of` (`src/labd/lab.rs`) applies the
same rule in the lab daemon, with the state directory the daemon really uses
(`VMLAB_WORK_DIR` included): no syncer starts, and `workspace.unavailable`
gives the reason. The syncer's floor (`src/labd/workspace/ignore.rs`) holds
`.vmlab` at any depth, so no `.gitignore` or `.vmlabignore` rule brings a state
directory into the sync in either direction. That also covers a second lab
nested in the workspace. Both tests named above are in the tree. Before the fix
both failed on helios: `workspace = "."` gave no issue, and the seed carried
`/src/.vmlab/smb/creds` into the guest. The test that held `workspace = "."` to
be valid no longer does. N10 stays open for a workspace outside the lab root
that does not hold the state directory.

### F2. A restore while the host workspace directory is away skips the bracket

**Where.** `src/labd/lab.rs:3273-3285` (`workspace_of` answers `None` when the
host directory is not there), `lab.rs:3726` and `lab.rs:3891` (the halt check
gets that `None`), `lab.rs:3760-3763` (no note is written for `None`).

**Why it is a fault.** `restore` reads the workspace through `workspace_of`.
That function resolves the declared directory on the host, and returns `None`
when it is missing. `None` also means "this machine has no workspace", so the
restore then does what it does for a machine that is not a dev machine: it
does not read the ledger for a halt, and it does not write the note that a
re-seed is owed. The ledger's path does not depend on the host directory. Only
the check of its `host_root` does.

The directory can be away for ordinary reasons: the host checkout is on a
branch that does not have it, a volume is not mounted, or the directory was
moved for a moment. `up` treats this case as "do not start, loudly". `restore`
treats it as "nothing to bracket".

What follows, read and not run: the directory comes back and `up` starts a
syncer. The ledger has no `reseed_owed`, so the syncer runs an ordinary walk
over the rewound guest. A file the snapshot holds in an older version differs
from the agreed digest on the guest side only, so the plan carries the old
version onto the host (`plan.rs:785-790`). That is the propagation the bracket
exists to prevent.

**Proof.** Probe `review672_p8`. A stopped dev machine has a snapshot, and its
ledger records a halt on `main.rs`. Control: `restore` without the flag is
refused. Then the host directory is renamed away. Result: the same `restore`
returns `Ok`. After the directory is back, the ledger has no `reseed_owed`,
and it still records the halt.

**Test to keep.**
`a_restore_with_the_host_workspace_away_still_refuses_and_still_notes_the_re_seed`:
the same steps, asserting the refusal on the halt, and with
`--discard-guest-changes` asserting `reseed_owed` on the ledger.

**How bad.** Data loss on the canonical copy, and a halt that is passed over
without the discard flag.

### F3. A running dev machine with no syncer is treated as stopped

**Where.** `src/labd/workspace/syncer.rs:537-545` and `syncer.rs:581-588`
(`before_capture` and `before_restore` read "no syncer" as "the machine is
down"), `src/labd/lab.rs:1366-1408` (`start_machine` starts no syncer),
`lab.rs:2527` (the one start, at the end of `up`), `lab.rs:3795-3797` (a
restore puts a syncer back only if it took one off),
`src/labd/lab/reload.rs:288-294`.

**Why it is a fault.** Only `up` starts a workspace syncer. A dev machine runs
without one after each of these, read from the code:

- `vmlab down dev01`, then `vmlab vm start dev01`. The partial `down` stops the
  syncer (`lab.rs:3072`), and `vm start` does not start one.
- An `up` whose provisioning fails. The machine runs, and `start_workspaces`
  was never reached.
- An online snapshot restored onto a stopped machine, which boots it.
- A wscript `start()`.

Nothing says so. `vmlab dev sync status` answers "it is not up, or it declares
no `@dev(workspace = …)`", and neither is true.

Both brackets then take the path written for a machine that is down. A capture
runs no flush and refuses only on a halt the ledger still records. A restore
does the same, so guest work goes without `--discard-guest-changes`. §19.6
says capture "refuses with no escape" where the guest holds work the host has
never seen. Here it does not look.

**Proof.** Probe `review672_p9`. `start_machine("dev01")` on a lab whose
`dev01` declares a workspace. Result: the machine is `Running`, no syncer
exists, `snapshot` returns `Ok`, and `restore` without the flag returns `Ok`.

**Test to keep.**
`a_dev_machine_started_outside_up_gets_its_syncer`, and
`a_capture_of_a_running_dev_machine_with_no_syncer_is_refused`.

**How bad.** Data loss (guest work that never reached the host), and a
workspace that does not sync with no word about it.

### F4. A stop takes no last pass and records nothing about owed work

**Where.** `src/labd/lab.rs:3059-3073` and `lab.rs:3150` (the syncer is stopped
before the machine, with no pass), `src/labd/workspace/syncer.rs:690-697`
(`stop`), `syncer.rs:581-588`, `src/labd/workspace/ledger.rs:158-166` (the
ledger keeps `reseed_owed` and `halted`, and nothing else about owed work).

**Why it is a fault.** `down` stops the syncer first and runs no last pass.
Guest work that is inside its quiet period, owed after a failed pass, deferred
behind a git lock, or waiting behind a rescan stays in the guest only. That is
safe while the clone lives: the next `up` walks the tree and finds it.

It is not safe across an offline restore. For a stopped machine
`before_restore` refuses only on `ledger.halted`. The last report knew which
paths were owed, and that knowledge went with the syncer. So `vmlab down`,
then `vmlab snapshot restore`, discards the work with no refusal and no flag.
§19.6 asks restore to "halt and surface" when the guest has unsynced work, and
names "sync paused" as a case.

**Proof.** Probe `review672_p3`. After a clean pass the guest edits `main.rs`
and creates `new.rs`, and the watch reports both. The syncer is stopped at
once, as `down` does. Result: the host holds neither change,
`before_restore` without the flag returns `Ok`, and `before_capture` returns
`Ok`.

**Test to keep.**
`a_restore_of_a_stopped_machine_refuses_on_work_its_last_pass_left_owed`. The
fix card decides between a last pass at stop and a record on the ledger.

**How bad.** Data loss (guest work).

### F5. `destroy` does not ask about a halted or unsynced workspace

**Where.** `src/labd/lab.rs:1568-1594` (`destroy_machine`), `lab.rs:3150-3159`
(the lab's `destroy`).

**Why it is a fault.** §19.6 says "`destroy` takes the clone and the guest
working copy and loses nothing". That holds only when the two copies are in
step. `destroy_machine` stops the syncer, removes the disks and removes the
ledger. It runs no flush and reads no report. A workspace that is halted holds
a guest copy of every conflicting path that the host never took, which is the
state a restore refuses on by name. Work the last pass left owed is in the
same position. `destroy` removes both without a word, and removes the ledger
that recorded the halt.

`Rebuild Container` is documented as `destroy` + `up` (§19.7), so this is the
path a developer is told to use.

**Proof.** Probe `review672_p10`. The ledger of `dev01` records a halt on
`main.rs`. Result: `destroy_machine("dev01")` returns `Ok` and the ledger is
gone. For a running machine with a live halt the code path is the same. That
part was read, not run.

**Test to keep.**
`a_destroy_of_a_halted_workspace_refuses_without_the_discard_flag`, and the
same for a workspace with unsynced paths. Whether `destroy` gets a flag or a
warning is the fix card's decision.

**How bad.** Data loss (guest work).

### F6. One machine's pass holds a host-wide lock across its guest I/O

**Where.** `src/labd/workspace/syncer.rs:111-147` (the lock),
`syncer.rs:1759-1760` (`pass`), `syncer.rs:2441-2442` (`reconverge`),
`syncer.rs:1504-1505`, `syncer.rs:1084-1109` (the pass is awaited with no
`select!` on `stop`).

**Why it is a fault.** Every pass takes one lock for its whole length: a
process-wide mutex, then a file lock shared by every lab daemon that uses the
same runtime directory. The comment says it "also serializes unrelated
workspaces". The lock is held while the pass opens the guest session, walks
the guest tree, and pushes and pulls every file.

So one machine holds up every other dev machine of every lab of this user for
as long as its pass runs:

- A first seed of a large repository, minutes long, stops all other sync.
- A guest that answers slowly, but inside the file channel's silence budget
  (`vm_agent.rs:2411-2428`), holds the lock without a bound. A hostile guest
  can do that on purpose.

The effects on the other machines, read and not run: a `dev sync flush` waits
`VERB_TIMEOUT` (120 s) and fails, every capture is refused because its
pre-flight could not finish, every restore without the flag is refused, and a
`down`, a `destroy` or a restore's `suspend` waits for a task that is itself
waiting for the lock (`syncer.rs:690-697`).

§19.6 says the scope of a halt is "one machine's workspace. Not the lab, not
other dev machines". The lock exists for machines that share or nest a host
root (`competing_guest_edits_sharing_a_host_root_are_serialized`). It covers
far more than that.

**Proof.** Probe `review672_p1`. Two workspaces with different host roots and
different guests. The first guest does not answer `open`. Result: the second
machine's pass does not reach its own guest while the first is silent.

**Test to keep.**
`a_silent_guest_does_not_stop_an_unrelated_workspace`: the same set-up,
asserting that the second machine's pass completes while the first guest is
still held. It needs no clock.

**How bad.** Security against a hostile guest or peer (it denies sync,
capture and restore to other machines), and wrong result for a slow honest
one.

### F7. `dev sync diff` reads a guest file of any size into memory

**Where.** `src/labd/workspace/diff.rs:213-224`.

**Why it is a fault.** The inline cap (`INLINE`, 4 MiB) is checked against the
size the guest reports in `lstat`. The file is then pulled whole to a scratch
file, and `std::fs::read` reads all of it into the lab daemon's memory. The
pull is bounded only by the free space of the scratch directory's filesystem
(`vm_agent.rs:404-413`). So the cap holds only for a guest that reports its
sizes truthfully, and for a file that does not grow between the two calls. A
log file that grows is the honest case.

A hostile guest reports a small size and sends as much as the scratch
filesystem takes. The scratch directory is under the system temporary
directory, which is often RAM-backed, and the read then doubles it. One lab
daemon serves every machine of the lab. The developer has to run
`vmlab dev sync diff`, and a halt, which the guest can cause at will, tells
them to.

**Proof.** Probe `review672_p4`. A guest whose `lstat` reports 1 byte and
whose pull delivers `INLINE + 1` bytes. Result: the reply carries all
`INLINE + 1` bytes as text, with nothing omitted.

**Test to keep.** `the_inline_cap_holds_whatever_size_the_guest_reported`:
the same guest, asserting the "too large" answer and that no more than the cap
was read.

**How bad.** Security against a hostile guest (host memory and scratch space),
and a crash of the lab daemon.

**Fixed by card #696.** `guest_side` no longer pulls the file to a scratch
file and reads that back. It calls the new `GuestFs::read_capped`
(`FileOps::pull_capped`), which reads into memory through the sink that
refuses the chunk that would pass `INLINE`. The cap is on the bytes as they
arrive, so the size the guest reported bounds nothing. A file over the cap is
answered as too large, with the reported size named as the guest's word. The
diff uses no scratch directory any more. The test
`the_inline_cap_holds_whatever_size_the_guest_reported` is in
`src/labd/workspace/diff.rs`, and
`pull_capped_answers_none_for_a_file_over_the_limit` in
`src/labd/vm_agent.rs` covers the real session.

### F8. Guest names and guest text reach the terminal with their escapes

**Where.** `src/cli/dev.rs:184-290` (`sync_report`, the paths at 193, 270, 280
and 286), `src/cli/dev.rs:298-418` (`render_diff` and `unified`),
`src/labd/workspace/diff.rs:86-92`, `src/labd/workspace/ignore.rs:555-567`.

**Why it is a fault.** A guest path is accepted when no component is empty,
`.`, `..`, or holds `/` or NUL. Every other byte passes, so a name can hold
ESC, BEL, a newline or a carriage return. `vmlab dev sync status`, `flush` and
`resolve` print those names raw. `vmlab dev sync diff` prints guest file text
raw as well: the only test for "is text" is valid UTF-8 with no NUL. The
comment there gives the reason for the test as "rendering it would scribble on
the terminal", and an escape sequence does that.

What a sequence can do depends on the terminal: clear the screen, move the
cursor, set the title, write the clipboard. A newline in a name is enough to
print a line that looks like vmlab's own. These verbs are where the developer
decides which copy wins, and `resolve --guest` overwrites the canonical copy,
so a guest that controls what they show controls that decision.

**Proof.** Probes `review672_p6` and `review672_p7`. A name with a
clear-screen sequence, a clipboard sequence and a newline passes
`validate_entry_name`, and `sync_report` returns it with the ESC byte and the
forged line. A guest file with the same sequences is taken as text, and
`render_diff` returns the ESC byte.

**Test to keep.** `a_guest_name_is_printed_with_its_control_bytes_escaped`
and `guest_text_with_control_bytes_is_not_shown_raw`.

**How bad.** Security against a hostile guest.

### F9. `dev sync diff` calls a symlink and a file identical

**Where.** `src/labd/workspace/diff.rs:147-152`, `diff.rs:170-174`,
`diff.rs:203-210`.

**Why it is a fault.** A symlink is described by the bytes of its target, and
`identical` compares digests only. A symlink to `target` on one side and a
file that holds `target` on the other have the same digest, so the diff says
"the two copies are identical — the next pass adopts them as agreed, and the
halt clears with no verb at all". The plan disagrees: the kinds differ, so it
is a `KindReplaced` conflict (`plan.rs:972-974`) and the halt stays.

A Windows guest whose login cannot create symlinks is a way to get there
without anyone meaning to.

**Proof.** Probe `review672_p5`. Host: a symlink `x` to `target`. Guest: a
file `x` that holds `target`. Result: `identical` is true.

**Test to keep.**
`a_link_and_a_file_with_the_same_bytes_are_not_identical`.

**How bad.** Wrong result: the developer is told a halt will clear, and it
does not.

### F10. The refusal for a stopped machine names verbs that cannot answer

**Where.** `src/labd/workspace/bracket.rs:137-166` and `bracket.rs:198-214`,
`src/labd/workspace/syncer.rs:669-678`, `src/cli/dev.rs:160-178`.

**Why it is a fault.** A capture of a stopped dev machine whose ledger records
a halt is refused with the same sentence as a running one: "`vmlab dev sync
status dev01` says what is outstanding and `vmlab dev sync flush dev01` waits
for it; a halt has to be resolved first". For a stopped machine all three
verbs answer "has no workspace syncer running". The halt can be shown and
resolved only after the machine is started, and the refusal does not say so.

**Proof.** Read, not run. `Outstanding::when_stopped` and `Outstanding::of`
share one `Display`.

**Test to keep.**
`a_refusal_for_a_stopped_machine_says_to_start_it_first`.

**How bad.** Cosmetic.

## Notes

These have no test proposed, so they are not in the list.

- **N1. `resolve --all` takes the halt as it is when the command arrives**
  (`lab.rs:3333-3336`). That can include conflicts that appeared after the
  developer last looked, and `--all --guest` writes the guest copy over the
  canonical one at each. The status shows at most 500 paths
  (`syncer.rs:171`), so for a larger halt the whole list cannot be seen
  first. The code comment argues for this choice. The lead may want it
  decided, not inherited.
- **N2. `resolve` takes any path, not only a halted one.** A resolution
  applies wherever one side moved (`plan.rs:785-790`). `--host` on a path the
  guest alone edited overwrites the guest's edit. `--guest` on a path the
  guest deleted removes the host copy and is exempt from the bulk-delete guard
  (`plan.rs:881`). The developer asked for it. A mistyped path is the risk.
- **N3. The pre-flight flush and the suspend are two steps**
  (`lab.rs:3891`, then `lab.rs:3740`). Guest work written between them is
  discarded without the flag. The window is short.
- **N4. A reload during a restore.** `rebind_workspace` does nothing while the
  syncer is off (`reload.rs:293`), and `resume` then starts the syncer on the
  machine handle and the declaration it held before the reload
  (`syncer.rs:652-655`).
- **N5. The mode bits of a pulled file come from the guest**
  (`guest.rs:150-166`). A guest file with mode `0777` makes the canonical file
  writable by every host user. A guest file with mode `000` makes it
  unreadable to the syncer itself.
- **N6. `destroy_machine` removes the disks, then the ledger, and ignores a
  failed removal** (`lab.rs:1586-1594`). A crash between the two leaves a
  ledger about a tree that is gone. A new clone then meets confirmed
  agreements for paths it does not hold, and halts on each. That is safe and
  confusing.
- **N7. A ledger about another workspace is dropped whole**
  (`ledger.rs:285-303`), with its `reseed_owed` and `halted`. Editing
  `@dev(workspace_guest = …)` between an offline restore and the next `up`
  gets there. The new guest root then takes the first-sync path, which halts
  where the sides differ, so nothing is overwritten.
- **N8. The halt marker says the host's status "has them all"**
  (`halt.rs:200`). The status is capped at 500.
- **N9. `diff` on a host FIFO blocks** (`diff.rs:179`). The size check passes
  for a FIFO and `std::fs::read` waits for a writer. Only a path the developer
  names can be one, because a special file never enters a halt.
- **N10. The host `workspace` path is not held under the lab root.** An
  absolute path or `..` is accepted (`validate.rs:2175-2176`). F1 is the case
  where that matters. A workspace of `~` is the same class.
- **N11. The CLI resolves a dev machine's profile from the shipped set only**
  (`cli/dev.rs:429-435`). A user-overridden profile's `workspace_guest` is not
  seen. The CLI uses only the machine's name today, so there is no effect yet.
