# Review: lab lifecycle (labd, the supervisor, up and down)

Card #530. A bug-hunting review of how a lab daemon and the supervisor start,
stop and clean up, and of `vmlab up`, `down`, `destroy` and `lab restart`.
It lists faults and makes no fix. Reviewed at `next` commit `aa29533e`. All
paths are from the repository root, and all line numbers are from that commit.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped.

Severity words: **data loss**, **crash** (of a VM or a host process),
**security** (against a hostile guest or peer), **wrong result**,
**cosmetic**. No finding here is a security fault: the control sockets are
mode 0600 in a 0700 directory that `ensure_private_dir` refuses when another
uid owns it (`src/paths.rs:291-322`), and no guest input reaches this code.

Two facts are used by several findings:

- A hypervisor process outlives the lab daemon that started it. `Proc` spawns
  with `kill_on_drop(false)` (`src/qemu/process.rs:147`) and sets no parent
  death signal, and the daemon leaves with `std::process::exit(0)`
  (`src/labd/mod.rs:612`, `:1286`).
- vmlab-hv takes no lock on a disk image (a search of `hv/crates` for
  `flock`, `F_OFD_SETLK` and `LOCK_EX` finds none on the image path), and its
  launch removes the control and display sockets of the run before
  (`src/labd/hypervisor/vmlab_hv/mod.rs:277-291`). So a second vmlab-hv on
  the same clone starts and writes. QEMU refuses through its own image lock.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | data loss | Nothing makes a supervisor or a lab daemon the only one, and binding the socket unlinks a live one (fixed, card #546) |
| F2 | data loss | A teardown, a `down` and a `destroy` do not stop a start that is under way, so a machine boots after the last stop and is orphaned (fixed, card #547) |
| F3 | data loss | With no supervisor running, `down` and `destroy` from a second checkout act on the first checkout's lab of the same name (fixed, card #557) |
| F4 | data loss | `machine destroy` and `destroy` remove disks without excluding a start of the same machine (fixed, card #547) |
| F5 | crash | The orphan reaper knows a QEMU by lab name alone, across every vmlab environment on the host (fixed, card #559) |
| F6 | crash | The release deadline is shorter than one machine's graceful stop, so `vmlab daemon stop` kills guests that are shutting down (fixed, card #560) |
| F7 | crash | A release takes a daemon that does not answer for a dead one: it kills the lab's hypervisors and forgets a live daemon (fixed, card #561) |
| F8 | wrong result | The reference count of a global segment is anonymous: a lab that never attached can destroy it under another lab |
| F9 | wrong result | After a supervisor restart the adopted labs' global segments are gone and nothing attaches them again |
| F10 | wrong result | Orphaned hypervisors are reaped only through a registry entry, and several exits drop the entry first |
| F11 | wrong result | A termination signal does not close the supervisor to requests, so a lab daemon can be spawned during its teardown |
| F12 | wrong result | Two `up` commands after an edit both restart the daemon, and the second stops what the first started |
| F13 | wrong result | A `stopping` entry left by a killed supervisor is not adopted, and `up` waits 120 s and then refuses (fixed, card #574) |
| F14 | wrong result | A `labs.json` that does not parse is read as an empty registry (fixed, card #574) |

### F1. No single-instance guard (data loss)

- Where: `src/proto/server.rs:85-86` (`bind_with_events`),
  `src/cli/daemon.rs:33-61` (`ensure_supervisor`),
  `src/supervisor/mod.rs:146` and `:278-312`, `:452-488`
  (`adopt_existing_labs`, `ensure_lab_locked`), `src/labd/mod.rs:102-163`.
- Why it is a fault: `bind_with_events` runs `remove_file(path)` and then
  binds, whether or not a process still listens there. No pid file and no
  `flock` exists for either daemon. So a second daemon always starts, takes
  the socket path, and leaves the first one running and unreachable. Four
  paths lead there:
  1. Two CLI commands run while no supervisor runs (two terminals, or a
     script that starts two labs in parallel). Both fail to connect at
     `daemon.rs:35` and both spawn `vmlabd`. Each supervisor loads
     `labs.json`, and each later `save` overwrites the other's entries
     (`src/supervisor/registry.rs:89-99`). A lab daemon that supervisor 1
     spawned is unknown to supervisor 2, which spawns a second daemon for the
     same lab on the next `lab.ensure`.
  2. `adopt_existing_labs` marks a `running` entry `failed` when its socket
     does not answer, or when it answers but the pid does not match
     (`mod.rs:299-312`), without checking that the daemon is dead.
     `ensure_lab_locked` then spawns a successor for any entry that is not
     `running` (`mod.rs:452-488`).
  3. F7 and F14 drop the entry of a live daemon.
  4. The lab daemon itself builds the whole runtime and rewrites
     `state.json` (`src/labd/lab.rs:519`) before it binds
     (`src/labd/mod.rs:120`, `:161`), so it cannot notice a peer first.
  Two lab daemons for one lab each believe every machine is stopped. Each
  `up` launches a hypervisor on the same clone. With vmlab-hv both write the
  qcow2.
- Proof:
  1. Unit test `a_second_bind_does_not_take_a_live_socket` in
     `src/proto/server.rs`: bind a `Server` on a path, bind a second one on
     the same path. The second bind must fail and a client must still reach
     the first. Today the second bind succeeds and the first is unreachable.
  2. Supervisor test `a_failed_entry_with_a_live_daemon_is_not_shadowed`: a
     registry entry in state `failed` whose pid and start time name a live
     helper process, then `ensure_lab`. It must refuse or stop the old
     daemon first. Today it spawns a second `__labd`.
- Fixed in card #546. `SocketGuard` in `src/proto/server.rs` is an exclusive
  `flock` on `<socket>.lock`, which the kernel drops when the holder dies.
  Taking it also refuses a path that still accepts a connection. A bind needs
  the guard and fails with `AddrInUse`, naming the holder's pid, when a live
  daemon has it. A socket file that nobody listens on is replaced as before.
  The lab daemon takes the guard as its first step, before it loads the
  config or rewrites `state.json` (path 4), and the supervisor before it
  loads `labs.json` (path 1). `ensure_lab_locked` asks the guard before it
  spawns and answers `conflict` while a daemon holds it, whatever the
  registry says (paths 2 and 3). It does not stop that daemon, because an
  entry that was lost gives no root to check it against. The two proof tests
  failed before the fix and pass after it. Not fixed here: the registry
  overwrite of path 1 is closed only because the second supervisor no longer
  starts, and F7 and F14 still drop the entry of a live daemon.

### F2. A stop does not close the lab to starts (data loss)

- Where: `src/labd/mod.rs:261-272` (`teardown`), `:1276-1288` and `:593-614`
  (`Shutdown`, `Destroy`), `:228-232` (signal path),
  `src/labd/lab.rs:2832-2900` (`down`), `:2169-2212` (the wave loop of
  `up_with`).
- Why it is a fault: `teardown` calls `lab.down(&[], false)` once and the
  process then exits with status 0. Nothing marks the lab as stopping. The
  server keeps serving requests until the exit, and an `up` that is already
  in flight keeps running its later waves. `down` stops the machines that
  exist when its waves run (`lab.rs:2884-2890`). A machine that a later wave
  of `up`, a `machine.start` or an event handler script starts after that
  point runs on. At `exit(0)` it is orphaned. The supervisor takes the clean
  exit for a finished teardown and removes the entry
  (`src/supervisor/mod.rs:364-377`), so no later release reaps it (F10). The
  next `up` starts a new daemon and a second hypervisor on the same clone.
  The triggers are ordinary: `vmlab daemon stop`, a host shutdown, or a full
  `vmlab down` from a second terminal while an `up` with more than one wave
  is running. `teardown` also discards the result of `down`
  (`let _ = ...`, `mod.rs:262`), so a wave that failed to stop leaves the
  same state.
- Proof: lifecycle test `a_teardown_leaves_no_machine_running` in
  `src/labd/lifecycle_tests.rs`, on the fake hypervisor: a lab with machine
  `b` that depends on `a`, and a fake whose readiness for `a` waits on a
  channel. Start `up`, run `down(&[], false)` to its end, then release the
  channel and await `up`. Assert that the fake launched nothing after `down`
  returned and that every machine is `Stopped`. Today `b` launches.
- Fixed in card #547. `src/labd/start_gate.rs` is a gate that every start,
  `down` and destroy of the lab runtime goes through. A start carries a
  ticket from the moment it was asked for, and `up` takes one ticket for all
  its waves. A `down` refuses every older ticket for the machines it stops,
  so an `up` under way starts nothing after it. A start holds its machine's
  lock until the machine has started, and a stop takes that lock first, so a
  stop no longer passes over a machine that has no emulator yet. `teardown`
  calls `LabRuntime::stop_for_exit`, which closes the lab to every start
  before it stops the machines, and stops each machine by force when `down`
  fails. `destroy` closes the lab the same way.
  - The proof as written above does not reproduce: a machine that is stopped
    while `up` waits for its readiness fails that wait
    (`wait_until` in `src/labd/machine.rs`), so `b` never launches. The fault
    is real one step earlier, while the start of `a` is under way. The test
    is `a_down_leaves_no_machine_running_behind_an_up_under_way` in
    `src/labd/lab.rs`, where the lab runtime's doubles are, with a double
    that waits inside `start`. It fails before the fix (`a` and `b` run after
    `down` returned) and passes after it.
    `a_lab_stopped_for_exit_takes_no_start` covers the teardown.
  - Not fixed here: `vm stop` and `vm restart` do not make an earlier start
    stale, and the server still accepts requests during the teardown (they
    are refused only where they start a machine).

### F3. No supervisor is read as no registration (data loss)

- Where: `src/cli/daemon.rs:206-221` (`check_registration`), `:226-229`,
  `src/cli/lab.rs:444-474` (`cmd_down`), `:490-515` (`cmd_destroy`).
- Why it is a fault: `check_registration` returns `Ok` when it cannot
  connect to the supervisor, with the comment "No supervisor running means
  nothing is registered". That is false. A lab daemon outlives a supervisor
  that crashed or was killed, which `adopt_existing_labs` exists to handle.
  The lab socket is keyed by name only (`src/paths.rs:185`). So with the
  supervisor gone, `vmlab destroy` in a second checkout that declares the
  same lab name connects to the first checkout's daemon and sends
  `Destroy`. That deletes the first lab's clones, volumes and snapshots.
  `vmlab down` stops its machines the same way. `labs.json` on disk still
  holds the registration, and nobody reads it.
- Proof: unit test `a_registration_holds_while_no_supervisor_answers` in
  `src/cli/daemon.rs`, in a scratch XDG environment: write a `labs.json`
  with `mylab` registered from `/labs/first`, bind no supervisor socket,
  call `check_registration("mylab", "/labs/second")`. It must return the
  ADR-0011 conflict. Today it returns `Ok`.
- Fixed in card #557. When no supervisor answers, `check_registration`
  reads the registry the supervisor persisted (`Registry::load`, the
  `labs.json` the next supervisor adopts) and applies the same
  `LabEntry::claims` decision to it. The proof test (with a helper that runs
  in a child process with scratch XDG directories) failed before the fix and
  passes after it.

### F4. Destroy does not exclude a start (data loss)

- Where: `src/labd/lab.rs:1506-1516` (`destroy_machine`), `:2904-2917`
  (`destroy`), `:1333-1345` (`start_machine_with_output`),
  `src/labd/machine.rs:70-77` (`snapshot_guard`, which neither destroy
  takes).
- Why it is a fault: `destroy_machine` stops the machine, waits for it to
  settle, and removes its directory. Requests are served concurrently and
  no lock covers the three steps. A `machine.start` or `up` for the same
  machine that arrives after the stop finds it `Stopped`, makes or reuses
  the clone and launches the hypervisor. `remove_tree` then unlinks the
  disks under the running guest, and the state entry is cleared. The guest
  keeps running on unlinked files. Everything it writes is lost when it
  stops, and the status shows a machine with no clone. Lab `destroy` has the
  same gap between `down(&[], true)` and `remove_tree(&self.lab_local)`.
  `wait_settled` also gives up after `SETTLE_TIMEOUT` and the removal goes
  ahead (`lab.rs:3905-3909`).
- Proof: lifecycle test `a_machine_destroy_excludes_a_concurrent_start`: an
  image tool or fake whose stop completes and whose settle waits on a
  channel. Call `destroy_machine("a")`, then `start_machine("a")`, then
  release the channel. The start must be refused or must wait, and after
  both return either the machine is stopped with no directory or it runs
  with its disks present. Today it runs and its directory is gone.
- Fixed in card #547, with the gate of F2. `destroy_machine` makes every
  earlier start of the machine stale and holds the machine's start lock from
  before its stop until the directory and the state entry are gone. A start
  under way finishes first and is stopped, and a later start waits for the
  removal and then builds the machine again. Lab `destroy` closes the lab to
  every start before `down`, and opens it again only when the destroy fails.
  An online restore, which boots a stopped machine, takes the same lock.
  Tests in `src/labd/lab.rs`:
  `a_machine_destroy_excludes_a_concurrent_start` (the proof above),
  `a_machine_destroy_stops_a_start_under_way_before_it_removes_the_disks` and
  `a_lab_destroy_refuses_a_start`. Each fails before the fix and passes after
  it.
  - Not fixed here: `wait_settled` still gives up after `SETTLE_TIMEOUT` and
    the removal goes ahead.

### F5. The orphan reaper crosses environments (crash)

- Where: `src/qemu/process.rs:438-448` (`LabIdentity::new`), `:462-464` and
  `:488-492` (the QEMU arms of `started`), `:554-563` (`kill_lab_orphans`),
  `:597-620` (`process_table`), `src/supervisor/mod.rs:613`, `:623`,
  `src/supervisor/templates.rs:541`.
- Why it is a fault: a QEMU is recognised by `-name vmlab:<lab>/` and by
  nothing else. `process_table` reads every pid in `/proc`. The lab name is
  unique only inside one supervisor's registry. A second vmlab environment
  of the same user (another `XDG_RUNTIME_DIR` and `XDG_STATE_HOME`, which
  is how the test suites and the card checks on one build host run) can run
  a lab of the same name. A release of a failed or silent lab `demo` in
  environment A then sends SIGKILL to every QEMU of the healthy lab `demo`
  in environment B. A stopped template build does the same with the build
  lab's name, which is the same for the same template in every environment.
  vmlab-hv, swtpm and virtiofsd are matched by the runtime directory and
  are not affected.
- Proof: unit test `a_qemu_of_another_runtime_directory_is_not_an_orphan`
  beside the existing reaper tests in `src/qemu/process.rs`: a process table
  with one QEMU argv whose `-name` is `vmlab:demo/a` and whose QMP and
  serial socket paths lie under another runtime directory. `lab_orphans`
  with the identity of `demo` must return nothing. Today it returns the pid.
- Fixed in card #559. `LabIdentity::started` in `src/qemu/process.rs` now
  knows a QEMU (by its name and by the argv form of a renamed host-GPU
  binary) by its `-name vmlab:<lab>/` marker **and** a `-qmp unix:<path>`
  under the lab's runtime directory, which `cmdline.rs` and `container.rs`
  always pass. The proof test failed before the fix and passes after it.

### F6. The release deadline is shorter than a graceful stop (crash)

- Where: `src/supervisor/mod.rs:49` (`RELEASE_DEADLINE`, 120 s), `:600-617`
  (`release_lab`), `:208-221` (`teardown`), `src/labd/stop_plan.rs:26-35`
  and `:92-124`, `src/cli/daemon.rs:89`, `:297-311`.
- Why it is a fault: a release sends `Shutdown`, waits 120 s, then kills the
  lab daemon and calls `kill_lab_orphans`, which is SIGKILL for every
  hypervisor. The lab daemon's own stop is allowed to take longer. For one
  machine stopped while it boots, `plan_stop` gives 60 s for the agent to
  answer, then 30 s each for the agent, ACPI and quit rungs: 150 s. A guest
  that ignores ACPI gets its whole ready timeout plus 150 s. Waves stop one
  after the other, so two waves of guests that each need the ACPI rung take
  up to 180 s. `vmlab down` is not affected, because it sends `Down` first
  and waits without a limit. `vmlab daemon stop`, a SIGTERM to the
  supervisor and a host shutdown go straight to the release. For the guest
  the kill is a power cut in the middle of its shutdown, and QEMU gets no
  chance to flush. The CLI side disagrees a second time: `vmlab daemon stop`
  waits 60 s in total (`SHUTDOWN_WAIT`) while the supervisor releases labs
  one by one with 120 s each, so it reports a failure for a teardown that
  is still in order.
- Proof: unit test `the_release_deadline_covers_the_longest_stop_plan` in
  `src/supervisor/mod.rs`: build `plan_stop` for a booting guest with a
  sealed agent and `acpi_shutdown = true`, sum the waits of its rungs, and
  assert the sum is below `RELEASE_DEADLINE`. Today the sum is 150 s. No
  clock is needed: the test compares constants.
- Fixed in card #560. `plan_wait` in `src/labd/stop_plan.rs` sums a stop
  plan's waits. Each machine reports the stop wait of the ladder it would
  climb now (`Machine::stop_wait`, zero when nothing runs), and the lab
  daemon answers `shutdown` with the sum over its machines
  (`stop_within_secs`), since waves stop one after the other. `release_lab`
  waits that sum plus `RELEASE_MARGIN` (30 s) for the rest of the teardown,
  and never less than `RELEASE_DEADLINE`, now 180 s, which covers the 150 s
  ladder of a booting guest for a daemon whose reply names nothing.
  `vmlab daemon stop` has no fixed wait any more: like `vmlab down` it waits
  until the supervisor is gone, which the supervisor bounds lab by lab, and
  says after 10 s that it is still waiting. The proof test failed before the
  fix (150 s against 120 s) and passes after it, beside
  `a_release_waits_out_the_stop_wait_its_daemon_reports`. Not fixed here:
  `ensure_lab_locked` still waits `RELEASE_DEADLINE` for a lab that is
  stopping and then refuses (no kill), whatever the daemon reported.

### F7. A daemon that does not answer is taken for dead (crash)

- Where: `src/supervisor/mod.rs:574-631` (`release_lab`, the `else` branch at
  `:618-630`), `:420-459`, `:531-545`, `src/cli/lab.rs:447-457`, `:496-508`,
  `:1221-1232`.
- Why it is a fault: when the connect to the lab socket fails, `release_lab`
  calls `kill_lab_orphans` and removes the entry. It checks neither the pid
  nor the start time it read at `:585-586`, and it does not signal the
  daemon. `release_lab` also does not take the per-lab `ensure` lock. A
  daemon is alive without an answering socket in two cases. First, while it
  starts: the socket is bound only after `LabRuntime::build`
  (`src/labd/mod.rs:120`, `:161`). A `vmlab down` or `vmlab destroy` in a
  second terminal during another terminal's first `up` removes the entry,
  the waiting `lab.ensure` reports "exited during startup" (`mod.rs:539-543`)
  for a daemon that then comes up unregistered, and the next `up` spawns a
  second one (F1). In the same window `cmd_destroy` sees no daemon and runs
  `remove_dir_all` on the lab directory under the starting daemon
  (`src/cli/lab.rs:500-503`). Second, when the socket file is gone or
  replaced (F1, or a cleaned runtime directory): the daemon and its guests
  are healthy, and the release kills every hypervisor of the lab with
  SIGKILL while the daemon stays.
- Proof: supervisor test `a_release_does_not_forget_a_live_daemon`: a
  registry entry `running` whose pid and start time name a live helper
  process, and no socket at the lab's path. Call `release_lab`. Afterwards
  either the process is gone or the entry is still there. Today the entry
  is gone and the process runs.
- Fixed in card #561. `release_lab` takes the per-lab `ensure` lock, so a
  release waits for a start under way and then stops the daemon through its
  socket. When the socket does not answer but the entry's pid and start time
  still name a running process, the release sends that daemon SIGTERM (its
  termination signal runs the same teardown as `Shutdown`) and waits for it
  up to `RELEASE_DEADLINE`. Only past the deadline does it kill the daemon,
  reap the lab's orphans and drop the entry. `cmd_destroy` with no answering
  daemon asks for the release before it removes the lab directory. The test
  `a_release_does_not_forget_a_live_daemon` in `src/supervisor/mod.rs`
  failed before the fix and passes after it.

### F8. A global segment's reference count is anonymous (wrong result)

- Where: `src/supervisor/global.rs:596-618` (`attach`, `refcount += 1`),
  `:694-712` (`detach`), `:483-490` (`leave`), `src/labd/network.rs:310-358`
  (`attach_globals`), `:369-391` (`detach_globals`),
  `src/labd/mod.rs:139-150`, `:266-270`.
- Why it is a fault: `detach` subtracts one for any caller, whether or not
  that lab attached. `detach_globals` sends a detach for every global
  segment the lab declares. `attach_globals` stops at the first error, and a
  worktree lab that is refused attaches none, so a lab can detach segments
  it never joined. Lab B attaches `corp` (count 1). Lab A declares `corp`
  and a second segment whose attach fails first. When A's daemon stops, its
  detach brings `corp` to 0. The supervisor drops the segment's gateway
  (DHCP and DNS), its listener and its socket while B's machines are on it,
  and a later attach of `corp` builds a new switch that B is not bridged
  to. The count also leaks the other way: a lab daemon that crashes never
  detaches, and its restart attaches again, so the segment and its
  reservations stay for the life of the supervisor.
- Proof: unit tests in `src/supervisor/global.rs`.
  `a_lab_that_never_attached_cannot_destroy_a_segment`: attach `corp` as lab
  `b`, detach `corp` as lab `a`, assert `list()` still holds `corp`. Today
  it is empty. `a_second_attach_by_one_lab_is_one_reference`: attach `corp`
  as lab `a` twice, detach once, assert `list()` is empty. Today the
  segment stays with count 1.

### F9. Adopted labs lose their global segments (wrong result)

- Where: `src/supervisor/mod.rs:278-326` (`adopt_existing_labs`),
  `src/labd/mod.rs:139-150` (the only call of `attach_globals`),
  `src/supervisor/global.rs:596-688`.
- Why it is a fault: a global segment lives in the supervisor's memory. A
  lab daemon attaches once, when it starts. When the supervisor dies and a
  new one adopts the running lab daemons, the new supervisor has no
  segments, and the adopted daemons hold trunk streams to a process that is
  gone. Nothing attaches again. Machines on a global segment lose DHCP, DNS
  and the path to the other labs, and `vmlab status` shows nothing wrong.
  The first later attach also allocates the subnet again from index 0
  (`global.rs:573-586`), which need not be the subnet the running guests
  hold leases in.
- Proof: test steps, for the e2e suite or a supervisor test with two real
  lab daemons: start two labs that share a global segment with no declared
  subnet, send SIGKILL to `vmlabd`, run `vmlab daemon status` (which starts
  and adopts). `global.list` must name the segment with both labs as
  members, and a machine of lab A must still resolve and reach a machine of
  lab B. Today `global.list` is empty.

### F10. Orphans are reaped only through a registry entry (wrong result)

- Where: `src/supervisor/mod.rs:581-583` (`release_lab` returns when the
  name has no entry), `:364-382` (`daemon_exited`), `:641-654`
  (`wait_daemon_gone`), `:452-488` (`ensure_lab_locked`),
  `src/cli/lab.rs:447-458`.
- Why it is a fault: `kill_lab_orphans` runs only inside `release_lab`, and
  only for a name that is still in the registry. The entry is removed
  without a reap in these cases: the daemon exits with status 0 while a
  machine still runs (F2), the daemon is killed while its entry is
  `stopping` (`daemon_exited` takes any exit in that state for the expected
  one, and `wait_daemon_gone` then returns true, so `release_lab` skips the
  kill at `:600-617`), and the registry is lost (F14). After that
  `vmlab down` prints "any orphaned processes were reaped" and reaps
  nothing. In the other direction, `vmlab up` on a `failed` entry spawns a
  new daemon and reaps nothing either, although the hypervisors of the
  crashed daemon still run. In both cases the next start of the machine
  meets the old hypervisor: QEMU refuses the image lock with an error that
  does not name the cause, and vmlab-hv starts a second writer.
- Proof: supervisor test `a_release_reaps_a_lab_that_has_no_entry`: an empty
  registry and a helper process whose argv is a QEMU with
  `-name vmlab:<lab>/a` and `-machine`. `release_lab(<lab>)` must end the
  helper. Today it returns at once. A second test,
  `a_failed_lab_is_reaped_before_its_successor_starts`, with a `failed`
  entry and the same helper, then `ensure_lab`: the helper must be gone
  before `__labd` is spawned.

### F11. A signal does not close the supervisor (wrong result)

- Where: `src/supervisor/mod.rs:37-41` (the field and its reason),
  `:195-202` (signal path), `:706-711`, `:847-861`, `:208-221`.
- Why it is a fault: `shutting_down` is set only by the `shutdown` request.
  On SIGTERM, SIGINT or SIGHUP `run_async` goes straight to `teardown`, and
  the server keeps serving. A `lab.ensure` that arrives then spawns a lab
  daemon after `teardown` has read the list of names (`:212-215`). That
  daemon outlives the supervisor and is adopted by the next one, which is
  the case the field's own comment says must not happen. The `shutdown`
  path has a smaller form of the same gap: a `lab.ensure` accepted before
  the flag was set, and still inside `ensure_lab_locked`, registers its
  daemon after the name list was read.
- Proof: supervisor test `a_signalled_teardown_refuses_requests`: build a
  `Supervisor` and its handler, run the teardown the signal path runs, and
  while it waits on a lab release send `LabEnsure` for another name. The
  request must be refused with the "shutting down" conflict. Today it
  spawns `__labd`.

### F12. Two restarts of one daemon (wrong result)

- Where: `src/cli/lab.rs:194-275` (`ensure_current_daemon`, the `Restart`
  arm at `:246-274`), `:118-134` (`daemon_freshness`),
  `src/supervisor/mod.rs:670-691` (`restart_lab`).
- Why it is a fault: the CLI reads the status, sees an edited file and no
  running machine, and asks for `lab.restart`. The supervisor restarts
  without a condition: its comment leaves "is the lab down" to the caller.
  The check and the restart are two requests. Run `vmlab up a` and
  `vmlab up b` together after an edit. Both read the old daemon's status
  and both choose `Restart`. The first restart finishes and its `up` boots
  `a` on the new daemon. The second restart, which waited on the per-lab
  lock, releases that new daemon, and its teardown stops `a` in the middle
  of its boot or its provisioning. The first command fails with a dropped
  connection. `vmlab lab restart` has the same gap between its check
  (`lab.rs:1275-1283`) and its request.
- Proof: supervisor test `a_restart_names_the_daemon_it_replaces`: with a
  fake lab daemon, send two `LabRestart` requests for one lab that were
  both decided against the first daemon, and start a machine on the
  successor between them. The second request must leave the successor in
  place (same pid) and must not stop the machine. Today it shuts the
  successor down.

### F13. A `stopping` entry is not adopted (wrong result)

- Where: `src/supervisor/mod.rs:280-293` (`adopt_existing_labs` skips every
  entry that is not `running`), `:429-442` (`ensure_lab_locked`),
  `:587-588`.
- Why it is a fault: `release_lab` writes `stopping` to `labs.json` before
  it sends `Shutdown`. A supervisor that is killed in between (F6 makes a
  long teardown likely to be cut short by the service manager) leaves a
  `stopping` entry for a daemon that was never asked to stop and still runs
  its machines. The next supervisor neither watches that daemon's exit nor
  forwards its events. `vmlab up` then waits the whole `RELEASE_DEADLINE`
  for a daemon that is not going anywhere and refuses with "is still
  stopping". `vmlab lab list` shows the lab as `stopping` without end. Only
  `vmlab down` gets out of it.
- Proof: supervisor test `a_stopping_entry_with_a_live_daemon_is_adopted`: a
  registry with a `stopping` entry whose pid names a live fake lab daemon
  that answers `ping`. After `adopt_existing_labs`, the entry must be
  `running` (or the daemon must have received `Shutdown`), and its exit
  must be watched. Today the entry is unchanged and nothing watches it.
- Fixed in card #574. `adopt_stopping` in `src/supervisor/mod.rs` settles a
  `stopping` entry before the other entries are judged. A daemon that runs
  and answers `ping` becomes `running` and is adopted like any other: its
  events are forwarded and its exit is watched. It is not sent `Shutdown`,
  so its machines keep running, and `up`, `down` and `lab list` see a
  running lab. A daemon that runs and does not answer keeps `stopping` and
  its exit is watched, so the entry goes when it does. An entry whose daemon
  is gone is dropped. A `stopping` entry with a live daemon is no longer
  dropped when its lab file is gone. The proof test failed before the fix
  (the entry stayed `stopping`) and passes after it. It also checks the
  dropped entry and that the adopted daemon's exit is noticed. Not fixed
  here: a daemon in its teardown answers `ping` like one that was never
  asked, and nothing in the protocol tells them apart. A supervisor that
  starts during a teardown therefore adopts that daemon as `running` and
  marks it `failed` with `lab.daemon_crashed` when it exits. The next `up`
  starts the lab as usual.

### F14. An unreadable registry is an empty one (wrong result)

- Where: `src/supervisor/registry.rs:74-87` (`load`,
  `serde_json::from_str(&s).unwrap_or_default()`), `:89-99` (`save`).
- Why it is a fault: any parse error gives an empty registry, with no log
  line. That includes a file written by a newer build that added a state or
  a field type, and a file cut short. Every running lab daemon is then
  forgotten: not adopted, not watched, not released by `vmlab daemon stop`,
  and its name is free for a second daemon (F1) and for a lab at another
  root (ADR-0011's check passes). The next `save` overwrites the file, so
  the entries are gone for good. `LabState::load` treats the same condition
  as an error on purpose (`src/labd/state.rs:198-210`, test
  `a_corrupt_state_file_is_an_error_not_an_empty_lab`).
- Proof: unit test `a_registry_that_does_not_parse_is_not_empty` in
  `src/supervisor/registry.rs`, in a scratch state directory: write
  `{ this is not json` to `labs.json` and call `Registry::load`. It must
  return an error, or keep the file aside and say so. Today it returns an
  empty registry and the next `save` replaces the file.
- Fixed in card #574. `Registry::load` returns a `Result`, as
  `LabState::load` does: a missing file is an empty registry, a file that
  cannot be read or parsed is an error that names the file, and the file is
  left as it is. The supervisor refuses to start on that error, so it never
  saves over the file. `check_registration` in `src/cli/daemon.rs`, which
  reads the registry when no supervisor answers, refuses too and does not
  take the name for unregistered. The store's backing scan logs a warning
  and scans no lab. The proof test failed before the fix (`{ this is not
  json` was read as a registry of 0 labs) and passes after it. It also
  covers a file cut short and a state this build does not know. Not fixed
  here: the error of a supervisor that the CLI started reaches the user
  only as "did not come up" and the supervisor's log.

## Notes

These have no proving test that runs without fault injection or a clock, or
they are consequences of a finding above.

- N1. `Registry::save` and `LabState::save` write a temporary file and
  rename it without `sync_all` (`src/supervisor/registry.rs:95-98`,
  `src/labd/state.rs:233-238`). After a power loss the file can be empty.
  For `state.json` that is a lab that refuses to start until the file is
  removed, and removing it regenerates every MAC.
- N2. A SIGTERM to the whole scope or session (a `systemctl --user stop` of
  the supervisor's scope, a host shutdown) reaches the hypervisors at the
  same time as the daemons. QEMU quits on SIGTERM at once, so the graceful
  stop the daemons then try has nothing left to stop. The daemons catch the
  signal (`src/lifecycle.rs:92-118`). The hypervisors are not shielded.
- N3. After the first termination signal the daemons keep the handlers
  installed and read no second signal (`src/lifecycle.rs:92-118`). A
  teardown that hangs can be ended only with SIGKILL.
- N4. `ensure_lab_locked` returns "did not come up" after 120 s and leaves
  the child running and its entry `running` (`src/supervisor/mod.rs:547-551`).
- N5. `ensure_locks` gains one entry per lab name and never loses one
  (`src/supervisor/mod.rs:394-400`). Cosmetic.
- N6. A lab daemon that fails to start (an invalid lab file, a corrupt
  `state.json`) leaves a `failed` entry and emits `lab.daemon_crashed`
  (`src/supervisor/mod.rs:364-391`), although nothing crashed. Cosmetic.
- N7. `ensure_supervisor` treats a supervisor that accepts a connection but
  does not answer `ping` as one that is shutting down, waits 60 s and says
  "try again shortly" (`src/cli/daemon.rs:43-47`). A hung supervisor gives
  that message every time, with no way out named.
- N8. The daemon-less branch of `cmd_destroy` and `cmd_lab_destroy` removes
  the lab directory before the release that reaps orphaned hypervisors
  (`src/cli/lab.rs:500-513`, `:1320-1341`), and it does not remove the
  machines' runtime directories that `lab.destroy` removes
  (`src/labd/lab.rs:2921-2926`).

## What was read, and what was not

Read in full: `src/supervisor/mod.rs`, `src/supervisor/registry.rs`,
`src/cli/daemon.rs`, `src/labd/mod.rs`, `src/lifecycle.rs`,
`src/labd/stop_plan.rs`.

Read in part: `src/cli/lab.rs` (lines 1-545 and 1095-1380: `up`, `down`,
`destroy`, and `lab list/info/stop/restart/destroy`), `src/labd/lab.rs`
(`build`, `start_machine`, `stop_machine`, `destroy_machine`, `up_with` to
the wave loop, `down`, `destroy`, `wait_settled`), `src/proto/server.rs`
(`bind` and the accept loop), `src/qemu/process.rs` (spawn, `LabIdentity`,
the reaper), `src/paths.rs` (runtime, state and lab directories,
`ensure_private_dir`), `src/supervisor/global.rs` (`attach`, `detach`,
`admit`, `leave`), `src/labd/network.rs` (`attach_globals`,
`detach_globals`), `src/labd/state.rs` (`load`, `save`), `src/instance.rs`
(`effective_name`, `adopt`), `src/labd/hypervisor/vmlab_hv/mod.rs` (the
launch's socket removal), `src/template/build.rs` (the work directory lock
and sweep), and cards #324 and #372.

Not read: `src/labd/vm.rs` and `src/labd/container.rs` beyond the stop entry
points, `src/labd/machine.rs` beyond the snapshot guard, the provisioning and
reload paths (`provisioning.rs`, `reload_plan.rs`), `src/cli/lab_move.rs`,
`src/supervisor/templates.rs`, `store.rs`, `backing.rs`, `keepawake.rs` and
`trunk_auth.rs`, the worktree paths (`src/freeze/`, `labd/worktree.rs`,
`freeze_clone.rs`, `isolation.rs`), the SMB server's lifecycle, the port
slots of `src/freeze/ports.rs`, `lifecycle_tests.rs` (so a finding may
already have a test that this review did not see), and the vmlab-hv process
itself. `restart_machine` and `vm restart` were not reviewed.
