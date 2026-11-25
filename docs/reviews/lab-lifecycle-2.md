# Second review: lab lifecycle (labd, the supervisor, up and down)

Card #969. A second bug-hunting pass over how a lab daemon and the supervisor
start, stop and clean up, and over `vmlab up`, `down`, `destroy` and
`lab stop/restart/destroy`. It lists faults and makes no fix. Reviewed at
`next` commit `759d5fa7`. All paths are from the repository root, and all
line numbers are from that commit.

The first pass is `docs/reviews/lab-lifecycle.md` (card #530, findings F1 to
F14, notes N1 to N8). All fourteen of its findings are marked fixed. This
document continues its numbering, so a code names one finding across both:
F15 to F22 and N9 to N16 are new here. A finding of the first pass is named
again only where its fix is incomplete, and the text says which.

Severity words are those of the first pass: **data loss**, **crash** (of a VM
or a host process), **security**, **wrong result**, **cosmetic**. No finding
here is a security fault, for the reason the first pass gives: the control
sockets are mode 0600 in a 0700 directory, and no guest input reaches this
code.

## How the findings were checked

Every finding is from reading the code. No vmlab code was run for this
review. Each finding names the test that would prove it. A finding whose test
fails to reproduce is wrong and should be dropped. No test below uses a
wall-clock limit. Where two operations must overlap, the test blocks one of
them on a channel (the `Hold` double of `src/labd/lab.rs`, or a hook of the
fake hypervisor). Where the fault is a wait that never ends, the test runs
under tokio's paused clock (`#[tokio::test(start_paused = true)]`), which
moves virtual time only.

Three facts are used by several findings:

- A request handler of either daemon runs on its own task and is not
  cancelled when its client goes away (`src/proto/server.rs:425-436`). The
  only thing that drops a start half way is `abandon_up`
  (`src/labd/lab.rs:417-433`).
- A hypervisor process outlives the handle that spawned it: `Proc::spawn`
  sets `kill_on_drop(false)` (`src/qemu/process.rs:150`).
- `Client::send` has no timeout (`src/proto/client.rs:143`). A peer that
  accepts a connection and never answers holds its caller for good.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F15 | crash | The stop wait a lab daemon reports leaves out the workspace syncer's last pass, so a release can still kill guests in the middle of their shutdown (F6 fix incomplete) |
| F16 | crash | Every reap of a lab's orphans is SIGKILL at once, and the next `up` of a lab whose daemon crashed does it to guests that run |
| F17 | wrong result | A start that `up` aborts because another machine failed leaves the machine at `Starting` for the life of the daemon, and a hypervisor that no handle names |
| F18 | wrong result | A daemon restart for an edited lab file ends a daemon that runs an offline snapshot, a restore or a freeze, because the check sees only power states and downloads |
| F19 | wrong result | A lab daemon that accepts and does not answer holds the supervisor's registry lock and a release for good, so the release deadline never reaches it (F7 fix incomplete) |
| F20 | wrong result | Adoption judges a snapshot of the registry while requests are served, and marks a successor daemon `failed` by name |
| F21 | wrong result | `vm stop` returns success over a start under way, and `vm restart` boots a machine after the `down` that crossed it (F2 fix incomplete) |
| F22 | wrong result | `vmlab lab destroy` removes the lab directory before any release, and both destroy verbs ignore a release that failed (F7 fix incomplete) |

### F15. The reported stop wait leaves out the syncer's last pass (crash)

- Where: `src/labd/mod.rs:1316-1331` (`Shutdown` answers
  `stop_within_secs`), `src/labd/lab.rs:1446-1452` (`stop_wait`),
  `:3286-3305` (`down` runs each syncer's last pass before any machine
  stops), `src/labd/workspace/syncer.rs:838-856` (`stop_before_machine`),
  `:180` and `:482` (`VERB_TIMEOUT`, 120 s), `src/supervisor/mod.rs:54-72`
  (`RELEASE_DEADLINE`, `RELEASE_MARGIN`, `release_deadline`), `:856-888`.
- Why it is a fault: F6's fix makes the supervisor wait the sum of the
  machines' stop ladders plus 30 s, and never less than 180 s. `stop_wait`
  sums `Machine::stop_wait` only. Since #695 a `down` first gives every
  running workspace syncer a last pass (`stop_before_machine(name, true)`),
  one machine after the other, each bounded by `VERB_TIMEOUT` (120 s), and
  then waits for the syncer's task to end with no bound
  (`syncer.rs:878-883`). None of that is in the number the daemon reports.
  One dev machine whose agent has stopped answering takes the whole 120 s
  for its last pass and then the whole ladder (30 s for the agent rung, 30 s
  for ACPI, 30 s for quit): 210 s against a deadline of 180 s. Two dev
  machines with a slow last pass each take 240 s before the first machine is
  asked to stop. Past the deadline `release_lab_locked` kills the lab daemon
  and then every hypervisor of the lab with SIGKILL (`mod.rs:871-894`). For
  the guests that is a power cut in the middle of a shutdown, or before one
  was asked for. The triggers are those of F6: `vmlab daemon stop`, a
  termination signal to the supervisor, a host shutdown, and
  `vmlab lab restart` with a daemon whose status did not answer. `vmlab
  down` is not affected, because it sends `Down` first and waits without a
  limit.
- Proof: test `the_reported_stop_wait_covers_a_last_pass` in
  `src/labd/lab.rs`, with the doubles of that file: a lab with one `@dev`
  machine that runs and whose workspace syncer runs. Assert that
  `lab.stop_wait()` is at least the machine's own `stop_wait()` plus the
  bound of one last pass. Today the two are equal. The test compares
  durations and needs no clock.

### F16. Orphans are reaped with SIGKILL only (crash)

- Where: `src/qemu/process.rs:592-647` (`reap_lab_orphans`,
  `signal_lab_orphans`), `src/supervisor/mod.rs:707-709`
  (`ensure_lab_locked`), `:581-601` (`daemon_exited`), `:892-894`
  (`release_lab_locked`), `src/labd/vm.rs:1940-1944` (the stop ladder's own
  reason against SIGKILL).
- Why it is a fault: F10's fix reaps a lab's orphans in three more places,
  and each reap is one SIGKILL to every process, with no gentler signal
  first. The ordinary case is a lab daemon that died (out of memory, a
  panic, `kill -9`). Its entry becomes `failed` and its guests run on,
  unmanaged, which is what PRD §3 asks for. The next `vmlab up` in that lab
  reaches `ensure_lab_locked`, which kills every one of those guests before
  it spawns the successor. `vmlab down` with no daemon does the same through
  the release. The stop ladder in `vm.rs` says why a QEMU must not be
  killed this way: "a SIGKILL drops the qcow2 metadata it still holds, and
  the disk then reads as empty or unbootable". A QEMU quits and flushes on
  SIGTERM. A guest that ran for hours after the daemon's crash loses what
  its caches held, and nothing says so: the `up` prints no line about the
  guests it killed. I did not read what vmlab-hv does on SIGTERM or what it
  holds unflushed, so for an x86 guest the severity may be lower. QEMU runs
  the aarch64 and riscv64 guests.
- Proof: supervisor test `a_reap_asks_before_it_kills`, beside
  `a_failed_lab_is_reaped_before_its_successor_starts` and with its orphan
  fixture (`src/supervisor/mod.rs:1157-1187`): a helper process with the
  argv of a QEMU of the lab that writes a marker file when it gets SIGTERM
  and then exits. Run `ensure_lab` on a `failed` entry. The marker file must
  exist afterwards. Today it does not: the helper gets SIGKILL only.

### F17. An aborted start leaves `Starting` and an unowned hypervisor (wrong result)

- Where: `src/labd/lab.rs:417-433` (`abandon_up`: `starting.abort_all()`),
  `:2576-2611` (the wave tasks), `:2701-2703` (a start or a provisioning step
  that fails abandons the `up`), `src/labd/vm.rs:1421-1428` (the state is
  set to `Starting`), `:1578-1606` (`start_emulator`, the handle is stored,
  `before_resume`, `resume`), `:1612-1619` (the only place that resets the
  state, the error path), `:2542-2548` (`start` answers `Ok` for any state
  but `Stopped` and `Suspended`), `:1887-1892` (`stop_ladder` returns when
  no handle is stored), `src/labd/hypervisor/vmlab_hv/mod.rs:421-424`
  (spawn, then wait for the control socket),
  `src/labd/container.rs:801-808`.
- Why it is a fault: the comment at `lab.rs:2571-2575` gives the intent:
  when one machine of a wave fails, the rest are aborted on the spot. The
  abort drops `VmInstance::boot_inner` at whichever `await` it stands at.
  `boot_inner` has no drop guard. Its state went to `Starting` at the top,
  and only the error path of `run.await` puts it back. A start takes
  seconds (disks, swtpm, virtiofsd, the wait for the control socket), so a
  sibling that fails fast (a vmlab-hv refusal, a missing ISO) aborts it
  there. A provisioning step of an earlier wave that fails while a later
  wave boots does the same. What is left depends on the point of the drop:
  1. Before the spawn: the machine is `Starting` and nothing runs.
  2. Between the spawn and `vm.rs:1600`: a hypervisor runs, paused, and
     no field holds its handle. The swtpm and the virtiofsd processes run
     too, and their handles are held.
  3. After `:1600`: the handle is held, no exit monitor was spawned, and the
     guest may already run.
  In every case the machine stays `Starting` for the life of the daemon.
  `vm start` and `up` then answer success and start nothing (`:2547`).
  `vm stop` answers success and stops nothing in cases 1 and 2. In case 3
  the ladder runs with no control handle, ends with a kill, and the state
  stays `Stopping`, because the exit monitor is what sets `Stopped`.
  `vmlab status` shows `starting` without end. `vmlab up` after an edit
  takes the machine for a running one and reloads where it would restart
  (`src/cli/lab.rs:123-134`), and `vmlab lab restart` refuses with "still
  has machines running — stop them first". `machine destroy` waits 30 s in
  `wait_settled` and then removes the disks under the paused hypervisor of
  case 2. Only a full `vmlab down` gets out of it, because it replaces the
  daemon and the supervisor then reaps the lab's processes. The first
  command printed the error of the machine that failed, and nothing about
  the one it left behind.
  A container's `boot` sets `Starting` the same way (`container.rs:807`). I
  did not read `start_attempt` to its end.
- Proof: test `an_aborted_start_leaves_the_machine_stopped` in
  `src/labd/lifecycle_tests.rs`, with the fake hypervisor and a services
  double whose `before_resume` waits on a channel (`lifecycle_tests.rs:176`).
  Spawn `start_vm`, wait until `before_resume` is reached, and abort the
  task. Then assert that the machine is `Stopped`, that the fake has no live
  emulator or helper (`live_helpers`), and that a second `start_vm` makes
  the fake's `starts()` go to 2. Today the state is `Starting`, the first
  emulator lives, and the second start returns `Ok` with `starts()` at 1. A
  second test at the lab level, `a_failed_sibling_leaves_no_machine_starting`
  in `src/labd/lab.rs`: two machines in one wave, `a` held inside its start
  (`FakeMachine::held`) and `b` failing its start. After `up` returned the
  error and the hold is released, `a` must be `Stopped`.

### F18. A daemon restart ends a daemon that works on stopped machines (wrong result)

- Where: `src/cli/lab.rs:118-134` (`daemon_freshness`), `:255-274` (the
  `Restart` arm), `:1317-1324` (`cmd_lab_restart`), `src/status.rs:710-752`
  (`LabStatus`), `:858-860` (`all_stopped`), `src/supervisor/mod.rs:940-971`
  (`restart_lab`), `src/labd/mod.rs:273-281` (`teardown`), `:1316-1331`.
- Why it is a fault: `vmlab up`, `pull`, `vm start` and `vm restart` replace
  the lab daemon when the lab file changed (or the freeze moved) and
  "nothing would be lost". The test for that is: no machine is in another
  state than `Stopped`, and no download runs. A daemon does more than that
  on stopped machines: an offline snapshot capture, an offline restore, a
  `worktree freeze` or `thaw` (a copy and a rebase of every disk), the clone
  of a worktree machine from the freeze at the head of an `up`
  (`src/labd/lab.rs:2518-2521`), a `vmlab script` between two boots.
  `LabStatus` has no field for any of them. The restart sends `Shutdown`.
  The daemon's teardown stops machines, gives its background tasks five
  seconds and calls `exit(0)`. It waits for no request in flight and takes
  no snapshot gate. So the operation is cut where it stands: a restore with
  some disks rewound, a freeze with its steps half run and its undo list
  lost with the process. `docs/reviews/snapshots-2.md` F18 describes what an
  offline restore leaves when its daemon dies. This finding is an ordinary
  way to get there: one terminal runs `vmlab restore`, a second runs
  `vmlab up` after an edit. F12's fix notes that a start which reaches the
  daemon between the check and the restart is still stopped. This is the
  wider case: the operation was running before the check and the check
  cannot see it. `vmlab down` and `vmlab daemon stop` end the daemon the
  same way, but there the user asked for the lab to stop.
- Proof: test `a_daemon_at_work_on_stopped_machines_is_not_restarted`, in
  `src/labd/lab.rs` with its doubles: a lab whose machine is stopped and
  whose offline restore waits on a channel inside the image step. Take
  `lab.status()` while it waits and pass it to
  `crate::cli::lab::daemon_freshness` with another digest. The answer must
  not be `Restart`. Today it is, because every machine is `Stopped` and no
  pull runs.

### F19. A daemon that accepts and does not answer wedges the supervisor (wrong result)

- Where: `src/supervisor/mod.rs:668-684` (`ensure_lab_locked` pings under
  the registry lock), `:856-860` (`release_lab_locked` sends `Shutdown`
  before it has a deadline), `:469-473` and `:524-527` (adoption pings),
  `src/proto/client.rs:143` (`send`, no timeout).
- Why it is a fault: F7's fix covers a daemon whose socket refuses the
  connection. A daemon that accepts and never answers is the other half. It
  is a daemon stopped with SIGSTOP, held by a debugger, frozen with its
  cgroup, or hung. Three places then wait without end:
  1. `ensure_lab_locked` holds the registry mutex across the connect and the
     `Ping` of a `running` entry. Every request that reads the registry waits
     behind it: `status` (which `vmlab status`, `down` and `destroy` of every
     lab send through `check_registration`), every `lab.ensure`, every
     release, and `daemon_exited` for every other lab. One silent lab daemon
     stops the supervisor for all labs.
  2. `release_lab_locked` waits for the reply to `Shutdown`. The deadline,
     the SIGKILL and the reap come after that reply, so they never come.
     `vmlab down`, `vmlab daemon stop` and the supervisor's own teardown on a
     termination signal hang on that lab. The deadline exists to end a daemon
     that hangs, and a daemon that hangs is the one it does not reach.
  3. `adopt_existing_labs` stops at that entry, and the entries after it are
     never adopted: not watched, their events not forwarded. F20 then
     applies to them for as long as the supervisor runs.
- Proof: supervisor tests under the paused clock, with a fake lab daemon
  that is a `UnixListener` which accepts and never writes, registered
  `running` under the pid and start time of a live helper process.
  `a_release_reaches_its_deadline_on_a_silent_daemon`: call `release_lab`
  and advance the clock past `RELEASE_DEADLINE`. The release must have
  returned and the helper must be gone. Today the call never returns.
  `a_silent_daemon_does_not_hold_the_registry`: start `ensure_lab` for that
  lab, then send `SupRequest::Status` and advance the clock. `Status` must
  answer. Today it does not.

### F20. Adoption marks a successor daemon failed (wrong result)

- Where: `src/supervisor/mod.rs:196-213` (the server is bound, then
  `adopt_existing_labs` runs), `:450-501` (the snapshot at `:451`, the two
  `mark_crashed` calls at `:475` and `:485`), `:603-610` (`mark_crashed`, by
  name only), `:537-541` (`adopt_stopping`, which does check the pid),
  `:782-797` (the startup wait of `ensure_lab_locked`), `:691-699`.
- Why it is a fault: the supervisor serves requests before it has adopted
  the entries of the run before. `adopt_existing_labs` copies the entries
  once and then judges each copy, with a connect and a `Ping` per entry in
  between. A `lab.ensure` for a later entry that arrives meanwhile finds its
  daemon dead (a host reboot leaves every entry `running` with a dead pid),
  spawns a successor and writes the successor's pid to the entry. Adoption
  then reaches its stale copy of that entry. If the successor has not bound
  its socket yet, the connect fails. If it has, the old pid is not running.
  Either way `mark_crashed(name)` sets the entry of the healthy successor to
  `failed` and emits `lab.daemon_crashed`. The waiting `lab.ensure` reads
  `failed` and answers "failed during startup" for a daemon that comes up.
  Every later `vmlab up` is then refused with "still has a daemon the
  supervisor lost track of", and tells the user to kill a process that
  works. `vmlab lab list` shows the lab as `failed`. The window is the time
  adoption needs for the entries ahead in the list: short with none, one
  `Ping` each with several labs, and without end behind a silent daemon
  (F19).
- Proof: supervisor test `adoption_does_not_fail_a_successor`: a registry
  with entries `a` and `b`, both `running`, `b` with a pid that is gone. A
  fake lab daemon for `a` whose `Ping` handler, before it answers, replaces
  the entry of `b` with the pid and start time of a live helper that serves
  `b`'s socket (what `ensure_lab_locked` does at `:742-752`). Run
  `adopt_existing_labs`. The entry of `b` must be `running` afterwards and
  no `lab.daemon_crashed` event may name `b`. Today the entry is `failed`.

### F21. `vm stop` and `vm restart` still cross a start (wrong result)

- Where: `src/labd/lab.rs:1747-1752` (`stop_machine`), `:2168-2181`
  (`restart_machine`), `:1568-1608` (`start_machine_at`),
  `src/labd/mod.rs:690-709`, `src/labd/start_gate.rs:10-25`.
- Why it is a fault: F2's fix names this as not fixed: "`vm stop` and
  `vm restart` do not make an earlier start stale". Two faults follow, and
  both are of the kind the gate was made for.
  1. `stop_machine` neither marks a stop nor takes the machine's start lock.
     While a start of the machine is under way and has no emulator yet (the
     download of its template, its disks, its swtpm), `vm stop` finds
     nothing to stop and answers success. The machine then boots. A wscript
     `stop()` goes the same way.
  2. `restart_machine` stops the machine, waits for `Stopped`, and only then
     takes its start ticket (inside `start_machine_with_output`). A `down`
     that runs to its end during the stop is older than that ticket. So the
     restart boots the machine after `vmlab down <machine>` answered
     success. After a full `vmlab down` the CLI releases the daemon, whose
     teardown stops the machine a second time, in the middle of its boot.
- Proof: tests in `src/labd/lab.rs` beside
  `a_down_leaves_no_machine_running_behind_an_up_under_way`.
  `a_vm_stop_stops_a_start_under_way`: machine `a` held inside its start
  (`FakeMachine::held`). Call `stop_machine("a", false)`, run until stalled,
  release the hold, await both. `a` must be `Stopped`. Today it runs.
  `a_restart_boots_nothing_behind_a_down`: a double whose first stop waits
  on a channel. Call `restart_machine("a")`, wait until the stop is reached,
  run `down(&["a"], false)` to its end with the hold released for it, then
  await the restart. `a` must be `Stopped`. Today it runs.

### F22. `lab destroy` removes the directory first, and a failed release is ignored (wrong result)

- Where: `src/cli/lab.rs:1359-1395` (`cmd_lab_destroy`, the removal at
  `:1373-1378`, the release at `:1386-1392`), `:555-570` and `:576-580`
  (`cmd_destroy`), `src/supervisor/mod.rs:882-894`.
- Why it is a fault: F7's fix made `cmd_destroy` ask for the release before
  it removes `.vmlab/` when no daemon answers. `cmd_lab_destroy`, the same
  verb by lab name, was left as it was: with no answering daemon it removes
  the directory at once and releases afterwards. So `vmlab lab destroy
  <name>` during another terminal's first `up` (the daemon has no socket
  yet) removes the lab directory under a daemon that is starting, and for a
  lab whose daemon crashed it unlinks the disks under guests that still run,
  before the reap. N8 of the first pass named the order for both verbs.
  Both verbs also discard the release's result (`if let Ok(sup) ...` and
  `let _ = sup.send(...)`). The release fails when the supervisor is of
  another build than the CLI (`ensure_supervisor` refuses it), when a daemon
  holds the socket guard and does not answer, when a daemon survives
  SIGKILL, and when an orphan cannot be killed. In each case the destroy
  goes on, removes the directory and prints "destroyed", with the lab's
  daemon or its hypervisors still running on unlinked files.
- Proof: CLI tests with the scratch-XDG child-process helper that F3's test
  uses (`src/cli/daemon.rs`), and a fake supervisor on the scratch socket.
  `a_lab_destroy_releases_before_it_removes`: the fake records when it gets
  `LabRelease` whether `.vmlab/` still exists. It must. Today the directory
  is gone by then. `a_destroy_stops_at_a_failed_release`: the fake answers
  `LabRelease` with an error. `cmd_destroy` must fail and `.vmlab/` must
  still be there. Today it succeeds and the directory is gone.

## Notes

These have no proving test that runs without fault injection, or they are
small, or I did not follow them to the end.

- N9. `ensure_supervisor` takes a supervisor that accepts and does not
  answer `ping` for one on its way out, and since F6's fix it waits for it
  with no limit (`src/cli/daemon.rs:35-46`, `:95-105`). With a hung
  supervisor every verb waits for good and says "vmlabd is stopping its
  labs' machines", which is not what happens. This replaces N7, whose 60 s
  limit is gone.
- N10. `daemon_exited` holds the registry lock across `reap_lab_orphans`,
  which waits up to 5 s for the killed processes
  (`src/supervisor/mod.rs:582-595`, `src/qemu/process.rs:592-612`). Every
  request that reads the registry waits that long.
- N11. `SocketGuard::acquire` sleeps on the calling thread, up to one second
  in all, and makes a blocking connect (`src/proto/server.rs:157-183`). The
  supervisor calls it from async code (`src/supervisor/mod.rs:691`, `:892`).
- N12. `start_with_deferred_refresh_at` notes the refresh owed to the
  handshake before the start is admitted (`src/labd/lab.rs:1922-1929`). A
  start that the gate refuses leaves the note, and the machine's next start
  by any path runs a refresh that nobody asked for at its handshake.
- N13. `up_with` starts the SMB server after its downloads and before its
  first wave (`src/labd/lab.rs:2527-2547`). A full `down` that ran to its
  end during the downloads stopped smbd, the `up` starts it again, and its
  first start is then refused as stale. smbd runs until the daemon goes.
- N14. `TaskGroup::shutdown` joins the tasks registered when it is called
  (`src/lifecycle.rs:64-75`). The handler dispatch loop can register a
  `handler-run` after that (`src/labd/mod.rs:221-231`), for the `vm.stopped`
  events the teardown itself emits. Such a run is cut by `exit(0)` with no
  grace.
- N15. `Registry::save` discards every error
  (`src/supervisor/registry.rs:107-118`). On a full disk the registry in
  memory and `labs.json` differ, nothing says so, and the next supervisor
  adopts the older file. N1 of the first pass (no `sync_all`) is unchanged.
- N16. `start_machine_with_output` calls `ensure_workspace` after the start
  lock is released (`src/labd/lab.rs:1573-1576`, `:3457-3476`). A `down`
  that waited for that start stops the machine next, and the syncer is then
  started for a machine that is stopping. `down` stopped the syncers before
  it took the lock (`:3301-3305`), so this one gets no last pass and no stop
  record on its ledger. I did not follow what the brackets then read.

## What I read

Read in full, tests excluded: `src/supervisor/mod.rs` (lines 1-1146),
`src/labd/start_gate.rs`, `src/lifecycle.rs`, `src/cli/daemon.rs` (lines
1-370), `docs/reviews/lab-lifecycle.md`.

Read in part: `src/labd/mod.rs` (lines 60-480: the daemon's start, signal
path, `announce_exit`, `teardown`. Lines 565-730 and 1305-1336: the `Up`,
`Down`, `Destroy`, `Shutdown` and machine power handlers), `src/labd/lab.rs`
(lines 417-433, 1420-2220, 2380-2780, 3180-3480, 4489-4562:
`abandon_up`, the start and stop of one machine, `destroy_machine`, the
deferred refresh, `restart_machine`, `up_with`, `down`, `stop_for_exit`,
`destroy`, `ensure_workspace`, `wait_settled`. Of the tests, lines
7135-7300), `src/labd/vm.rs` (lines 1377-1720, 1848-1950, 2292-2300,
2505-2620: `boot_inner`, `teardown`, the head of `stop_ladder`, `stop_wait`,
`start`), `src/labd/container.rs` (lines 795-830), `src/labd/machine.rs`
(`wait_state`), `src/labd/hypervisor/vmlab_hv/mod.rs` (lines 332-455,
`start_emulator`), `src/qemu/process.rs` (lines 120-215 and 380-705: the
spawn, `LabIdentity`, the reaper), `src/proto/server.rs` (lines 86-275 and
320-445: `SocketGuard`, the bind, `serve_conn`), `src/proto/client.rs` (the
signatures of `connect` and `send`), `src/supervisor/registry.rs` (lines
100-178), `src/cli/lab.rs` (lines 60-610 and 1254-1430: `daemon_freshness`,
`ensure_current_daemon`, `up`, `pull`, `down`, `destroy`, and
`lab stop/restart/destroy`), `src/labd/workspace/syncer.rs` (lines 170-182,
712-726, 838-892, 923-931), `src/status.rs` (`LabStatus`, `PowerState`,
`all_stopped`), and the names of the doubles in `src/labd/lifecycle_tests.rs`
and `src/labd/hypervisor/fake.rs`.

## What I did not read

- `src/labd/lab.rs` outside the lines above: `LabRuntime::build`, `reload`
  and `settle_pending`, `ensure_pulled`, provisioning, the snapshot and
  restore verbs, `status`. F18 rests on `LabStatus` having no field for an
  operation in flight and on the teardown waiting for none. I did not read
  the offline restore itself.
- `src/supervisor/global.rs`, `templates.rs`, `store.rs`, `backing.rs`,
  `keepawake.rs`, `trunk_auth.rs`. The global segments' fixes (F8, F9) were
  not looked at again.
- `src/labd/container.rs` and `container_ctl.rs` beyond the first lines of
  `boot`, `src/labd/machine.rs` beyond `wait_state`, `src/labd/vm.rs`
  outside the lines above, `src/labd/stop_plan.rs`,
  `src/labd/state.rs`, `src/labd/network.rs`, `src/labd/build_lab.rs`.
- `src/cli/lab_move.rs`, `src/cli/lab.rs` outside the lines above (`status`,
  `lab list` and `info`, the machine verbs), `src/instance.rs`,
  `src/paths.rs`.
- The worktree paths (`src/freeze/`, `labd/worktree.rs`, `freeze_clone.rs`,
  `isolation.rs`), the workspace syncer beyond the stop entry points, the
  SMB server's lifecycle, the recorder.
- The vmlab-hv process itself: what it does on SIGTERM and SIGKILL (F16),
  and whether it holds an image lock.
- The tests of the files above, except where a finding names one. A finding
  may already have a test that this review did not see.
