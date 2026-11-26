# Second review: wscript (the lab scripting)

Card #1074, 2026-11-26. A second bug-hunting pass over `src/scripting` and
what a script reaches in the lab daemon. It lists faults and makes no fix.
Reviewed at `next` commit `24c3593d`. All paths are from the repository root,
and all line numbers are from that commit.

The first pass is `docs/reviews/wscript.md` (card #617, findings F1 to F12,
notes N1 to N10). All twelve findings are marked fixed there. This document
continues its numbering, so a code names one finding across both: F13 to F20
and N11 to N21 are new here. A finding of the first pass is named again only
where its fix is incomplete, and the text says which (F13, F15, F18, F19).

Who is trusted is as in the first pass: a script is code of the host user, a
guest is not trusted. None of the findings below gives a guest something. They
are all of the second kind: what a script, or the runner, does wrong to the
lab that runs it.

Severity words are those of the first pass: **security**, **data loss**,
**crash**, **wrong result**, **cosmetic**.

## How the findings were checked

Every finding is from reading the code. Six facts were also run on helios,
with temporary tests applied to the checkout of the branch through
`stc run card-1074`, run with `cargo test --lib probe_1074`, and removed
again. No probe test is committed. The probes used the existing test
harnesses (`lab_of`, `reload_lab` and `edited` in `src/labd/lab.rs`, a
`TerminalHandle` with no session in `src/scripting/terminal.rs`) and printed
what the code does today:

- F13: a script that doubles a string 28 times (`s = s + s`) ends `Ok` and
  holds 256 MiB. It costs under 300 instructions of the 10,000,000 budget.
- F14: `web` runs, the lab file changes its image, `reload` leaves it pending.
  A script takes `lab.machine("web")`, calls `restart()` on it and logs
  `state()`. Output: `before: running`, `restart ok`, `same handle: stopped`,
  `fresh handle: running`.
- F15: a sink that cancels the script's token when it sees the first line,
  and a script that logs `first`, `second`, 1000 lines in a loop (each with a
  `vmlab::env` call) and `last`. The sink got all 1003 lines, `second` and
  `last` among them, and the run ended with `cancelled`.
- F17: `while i < n { i = i + 1 }` and then `lab.log("done")`. It ends `Ok`
  for n = 500,000 and 1,000,000. For n = 2,000,000, 3,000,000 and 5,000,000
  the run fails with `probe.ws: fuel exhausted (at main)`.
- F18: a terminal buffer `ok\xe2\x82`, then `read()`, then the byte `0xac`
  appended, then `read()`. The two results: `"ok\u{fffd}"` and `"\u{fffd}"`.
- F19: `fn main(lab: Lab) -> int { lab.log("ran") 1 }` through
  `run_script_source`. The sink got `ran`, and then the run failed with
  `conversion error: type mismatch at host boundary: expected unit, got int`.

F16 and F20 were not run. Each finding names the test that would prove it. A
finding whose test does not reproduce is wrong and should be dropped. No test
below uses a wall-clock limit.

One statement rests on code that was not read: F13 assumes that the VM
concatenates strings with Rust's infallible allocation, so a failed
allocation aborts the process. The concat site in `wscript-vm` at `faf09e6`
was not read.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F13 | crash | The instruction budget does not limit memory: a few hundred instructions allocate any amount, and the lab daemon dies (the rest of first-pass F5) |
| F14 | wrong result | A `Machine` handle keeps the instance it was made from, so after a rebuild it reads and drives a dead machine |
| F15 | wrong result | A cancelled script runs on through every host call that does not wait, to its end (fix of F5 incomplete) |
| F16 | wrong result | `vmlab script --record a --record b` leaves `a` recording when the start of `b` fails |
| F17 | wrong result | The budget of 10,000,000 instructions ends a correct script, is for the whole run, and is in no document |
| F18 | wrong result | `Term.read` destroys a character split across two reads (fix of F8 incomplete) |
| F19 | wrong result | The entry point is checked by `vmlab validate` only: `vmlab script` and a first-boot script run to the end and then fail (fix of F9 incomplete) |
| F20 | wrong result | `m.snapshots()` lists an interrupted capture as a snapshot |

### F13. The instruction budget does not limit memory (crash)

- Where: `src/scripting/runner.rs:18-21` (`SCRIPT_FUEL`) and `:266-267`
  (`vm.set_fuel`), `src/scripting/runner.rs:227` (the script runs on a thread
  of the lab daemon).
- Why it is a fault: first-pass F5 ends with "The same missing limit lets a
  script grow a list until the daemon is out of memory". The fix of F5 gave
  every run a budget of 10,000,000 instructions. One instruction can allocate
  any number of bytes. `s = s + s` doubles a string, so 40 passes of a loop,
  about 400 instructions, ask for 1 TiB. The probe did 28 passes (256 MiB) and
  the run was `Ok`. A list doubles the same way. The process is the lab
  daemon, the only manager of the lab's machines. On a host that overcommits,
  the kernel kills it or another process. With a memory limit, the allocation
  fails and Rust aborts the process. No error reaches `up`. The script is the
  host user's own, so this is an accident and not an attack: a loop that
  builds a string and has a wrong end condition is enough, and the budget
  that was added to stop such a loop does not stop this one.
- Proof: test `a_script_that_allocates_without_end_faults` in
  `src/scripting/runner.rs`, in a child process, as
  `a_deep_value_faults_instead_of_aborting` (`:569`) does it, so that the end
  of the process is an exit status. The child sets `RLIMIT_AS` to 4 GiB and
  runs, with the budget of `execute`:

  ```wscript
  fn main() {
      let s = "x"
      let i = 0
      while i < 40 {
          s = s + s
          i = i + 1
      }
  }
  ```

  Expected: the run returns with a fault that names a memory limit, and the
  child exits 0. Today (not run): the child dies on SIGABRT with `memory
  allocation of … bytes failed`. The limit on the address space makes the
  test safe to run. Without it the test would take the memory of the host.

### F14. A `Machine` handle keeps the instance it was made from (wrong result)

- Where: `src/scripting/mod.rs:91` (`MachineHandle::machine`, an
  `Arc<dyn Machine>`), `:603-618` (`handle_for` stores the instance of the
  moment), `:850-879` (`start`, `stop`, `restart` go through the lab by name),
  `:896-960` (`state`, `is_ready`, `wait_ready`, `agent_answering`,
  `boot_id`, `wait_rebooted` ask the stored instance), `:517-537` (`exec`),
  `src/labd/lab.rs:1606` (a start settles a pending rebuild first) and
  `:2229-2250` (`restart_machine`).
- Why it is a fault: PRD §7.1 lets a reload rebuild a machine: at once when it
  is stopped, at its next boot when it runs. The lab daemon knows it, and
  each of its own paths takes the instance again after a start ("The instance
  that booted: a reload may have rebuilt it", `src/labd/lab.rs:2263`). A
  script handle does not. Its lifecycle calls go by name and reach the new
  instance. Every other call goes to the instance the handle was made from.
  After a rebuild that instance is stopped for ever. So this script, run
  after the lab file changed and `vmlab reload` left `web` pending:

  ```wscript
  let m = lab.machine("web").expect("web")
  let r = m.restart()
  let w = m.wait_ready(300)
  ```

  restarts the machine, and then `wait_ready` waits 300 s on the old instance
  and fails, while `web` is up. `m.state()` says `stopped`, `m.exec` fails,
  and `m.snapshot` and `m.record` (by name) work. The same holds for a handle
  that a long script keeps while somebody reloads the lab and the machine is
  stopped. A handler script that restarts a machine is the common case
  (`on "container.crashed" { run = "restart.ws" }`).
- Measured on helios: `same handle: stopped`, `fresh handle: running`.
- Proof: test `a_script_handle_follows_a_rebuilt_machine` in
  `src/labd/lab.rs`, with the fixture of
  `a_reload_rebuilds_what_is_stopped_and_leaves_what_runs_pending` (`:9849`):
  `reload_lab`, start `web`, `reload_with` a file that changes `web`'s image.
  Run, with a sink that collects:

  ```wscript
  fn main(lab: Lab) {
      let m = lab.machine("web").expect("web")
      let r = m.restart()
      lab.log(m.state())
  }
  ```

  Expected: the sink holds `running`. Today: `stopped`.

### F15. A cancelled script runs on through every host call that does not wait (wrong result)

- Where: `src/scripting/cancel.rs:89-93` (`check`) and `:100-115`
  (`block_on`, the only place a host call looks at the token),
  `src/scripting/runner.rs:228-232` (the runner checks before and after the
  run), `src/scripting/mod.rs:678-680` (`lab.log`), `:659-661` (`vmlab::env`),
  `:666-673` (`templeos_agent_script`), `:682-748` (`machine`, `machines`,
  `vm`, `vms`, `container`, `containers`, `this_vm`), `:1245-1253` (`m.logs`,
  which reads the console log file), `:1316-1318` (`m.logins`).
- Why it is a fault: first-pass F5 named it ("`lab.log` does not look at the
  token either"), and its proof test asks for no line after a cancellation.
  The fix checks the token before the run, after the run, and inside
  `cancel::block_on`. A host function that does not wait on the daemon never
  reaches `block_on`. A script that is cancelled while it runs goes on until
  its next waiting call or the end of its budget. Three results:
  1. `up` has failed and printed why, and the other provisions keep writing
     lines into its output.
  2. A script made of such calls runs to its last line, does all its work,
     and is then reported as `cancelled`.
  3. The time it takes is not bounded by anything small. `m.logs(n)` reads
     the whole console log on each call, costs a few instructions, and does
     not look at the token. A loop over it runs until the budget is spent,
     which is about a million passes (F17). `abandon_up` waits for it
     (`src/labd/lab.rs:423-440`), and so does a daemon that got SIGTERM.
- Measured on helios: 1003 lines after the cancellation, the last one among
  them, and the result `cancelled`.
- Proof: test `a_cancelled_script_logs_nothing_more` in `src/labd/lab.rs`:
  `run_script_source` with
  `ScriptOwner::provision("box").cancelled_by(token)` and a sink that
  collects each line and cancels `token` when the line holds `first`. Script:
  `fn main(lab: Lab) { lab.log("first") lab.log("second") }`. Expected: the
  run is `ScriptCancelled` and the sink holds no `second`. Today: the sink
  holds `second`. The test ends in both cases.

### F16. `vmlab script --record` leaves a recording running when a later one fails to start (wrong result)

- Where: `src/labd/mod.rs:608-634` (the `Run` verb: `record_start(…).await?`
  in a loop at `:617-620`, `record_finalise_run(run)` at `:632`),
  `src/labd/record.rs:93-118` (`record_prepare`: no display, already
  recording, a recording armed).
- Why it is a fault: the verb starts one recording for each `--record`, then
  runs the script, then finalises the run. The comment says: "the finalise
  that ends every recording a run started — success or failure — ends these
  too". When the second `record_start` fails, `?` leaves the verb before the
  script and before the finalise. The first recording is live, belongs to a
  run that nothing will finalise, and goes on until the machine stops or
  `max_duration` ends it. `vmlab script x.ws --record vm1 --record web`,
  where `web` is a container, prints `machine web has no display` and leaves
  `vm1` recording. The next `vmlab script x.ws --record vm1` fails with
  `machine vm1 is already recording`.
- Proof: test `a_script_run_that_cannot_start_a_recording_leaves_none` in
  `src/labd/lab.rs`, with the lab of
  `a_run_finalises_the_recordings_it_started` (`:8937`: `a` has a display,
  `web` is a container). Send `LabRequest::Run` with `record` for `a` and
  then for `web` through the daemon's request handler. The request fails.
  Expected: `lab.recordings.live_machines()` is empty. Today: it holds `a`.
  The verb's body is not a function of its own today, so the test needs the
  handler of `src/labd/mod.rs` or a small function cut out of it.

### F17. The instruction budget ends a correct script (wrong result)

- Where: `src/scripting/runner.rs:18-21` (`SCRIPT_FUEL: u64 = 10_000_000`)
  and `:266-267`.
- Why it is a fault: the budget was added so that a script that never calls
  the host still ends (first-pass F5). It is set once for the whole run and
  is never filled again, also not when the script proves it is alive by
  waiting on the host. So it is a limit on the work of a correct script too:
  1. A counting loop ends between 1,000,000 and 2,000,000 passes (measured).
     A script that walks the lines or the characters of a command's output
     reaches that with a few MiB of text. `exec` may return 64 MiB.
  2. A long script (a first boot may run 1800 s, a `vmlab script` run has no
     time limit) that polls a guest in a loop without a sleep, which is
     correct because each poll waits on the agent, ends after a million
     polls or so.
  3. The fault is `fuel exhausted (at main)`. It does not say what the fuel
     is, how much there was, or that vmlab set it. The word is in no page of
     the manual, in no skill and not in the PRD (searched for `fuel`,
     `budget`, `instruction` in `docs/manual`, `docs/vmlab-prd.md`).
  A budget that is filled again at each host call that looked at the cancel
  token would still stop a script that spins without the host, which was the
  purpose.
- Measured on helios: `Ok` at 1,000,000 passes, `fuel exhausted` at
  2,000,000.
- Proof: test `a_long_script_that_calls_the_host_is_not_ended` in
  `src/labd/lab.rs`: `run_script_source` with a sink that collects and

  ```wscript
  fn main(lab: Lab) {
      let i = 0
      while i < 3000000 {
          if i % 1000 == 0 { let n = lab.name() }
          i = i + 1
      }
      lab.log("done")
  }
  ```

  Expected: the sink holds `done`, or the fault names the limit and its size.
  Today: `fuel exhausted (at main)`. The loop is bounded, so the test ends in
  both cases.

### F18. `Term.read` destroys a character split across two reads (wrong result)

- Where: `src/scripting/terminal.rs:89-95`.
- Why it is a fault: this is first-pass F8 in the other method. The fix of F8
  made `expect` keep the raw bytes after a match. `read` still returns
  `String::from_utf8_lossy(&st.buf)` and clears the buffer. When the buffer
  ends in the first bytes of a character, those bytes become U+FFFD and are
  gone, and the rest of the character arrives with the next read and becomes
  a second U+FFFD. The proof test of F8 does not see it, because its `read`
  comes after the last byte arrived. A script that collects output with
  `read()` in a loop gets two replacement characters where the guest wrote
  one character.
- Measured on helios: `"ok\u{fffd}"`, then `"\u{fffd}"`.
- Proof: test `read_keeps_a_character_split_across_reads` in
  `src/scripting/terminal.rs`: a `TerminalHandle` with `session: None`,
  `ended: Some("closed")` and `buf = b"ok\xe2\x82"`. `t.read()`, then push
  `0xac` to `state.buf`, then `t.read()`. Expected: `"ok"` and then `"€"`.
  Today: `"ok\u{fffd}"` and `"\u{fffd}"`.

### F19. The entry point is checked by `vmlab validate` only (wrong result)

- Where: `src/scripting/mod.rs:1424-1446` (`check_script_entry`),
  `src/config/validate.rs:594-717` (its only caller, through
  `ValidateCtx::check_script`), `src/scripting/runner.rs:244-270` (`execute`
  compiles and calls, with no check between), `src/labd/mod.rs:608-631` (the
  `Run` verb), `src/labd/lab.rs:3251-3281` (`run_first_boot` checks the
  surface version and then runs the embedded source).
- Why it is a fault: the fix of first-pass F9 put the check of the entry
  point's name and signature into validation. Two kinds of script do not pass
  through validation of this lab file: the file given to `vmlab script`, and
  the first-boot script inside a template that was pulled or imported. For
  them the fourth case of F9 is unchanged: `fn main(lab: Lab) -> int` runs to
  its last line, and then the run fails with `type mismatch at host boundary:
  expected unit, got int`. A first boot whose work is done is reported
  failed, its `firstboot.done` marker is not written, and the next start runs
  the whole script a second time against a guest that is already provisioned.
  `vmlab script` exits with an error after a script that did everything.
- Measured on helios: the line `ran`, then the conversion error.
- Proof: test `a_script_with_the_wrong_signature_does_not_run` in
  `src/labd/lab.rs`: `run_script_source` with a sink that collects and
  `fn main(lab: Lab) -> int { lab.log("ran") 1 }`. Expected: `Err` that names
  the signature, and the sink holds no `ran`. Today: the sink holds `ran`.

### F20. `m.snapshots()` lists an interrupted capture (wrong result)

- Where: `src/scripting/mod.rs:1022-1039` (takes `name` of every entry),
  `src/labd/lab.rs:4588-4595` (`snapshots` adds each pending capture with
  `"pending": true`), `:4392` (`restore` refuses a pending one), `:4045` (a
  capture is pending from its start to its end).
- Why it is a fault: the daemon's list marks a capture that was interrupted,
  or that is running now, with `pending` and a reason ("interrupted capture,
  delete to clean up before retrying"). The script method keeps the name and
  drops the mark. A script cannot tell such a name from a snapshot. The usual
  form, `if names.contains("base") { m.restore("base") } else {
  m.snapshot("base") }`, takes the restore arm and fails. The manual says the
  method returns "the names of the machine's snapshots"
  (`docs/manual/pages/reference/wscript-machine.wcl:525`). The entries marked
  `unrestorable` and `locked` are snapshots and are rightly in the list.
- Proof: test `a_script_does_not_list_a_pending_capture` in
  `src/labd/lab.rs`, with the lab of
  `failed_lab_capture_rolls_back_prior_machines_and_keeps_pending_visible`
  (`:5762`): after `snapshot_all("capture")` failed, machine `b` holds the
  pending capture. Run
  `fn main(lab: Lab) { for n in lab.vm("b").expect("b").snapshots().expect("list") { lab.log(n) } }`
  with a sink that collects. Expected: the sink is empty. Today: it holds
  `capture`.

## Notes

These have no proving test without a wall-clock limit, or they are by design,
or they belong to another review.

- N11. `Term.send` can wait for ever. `SessionSender::send`
  (`src/labd/vm_agent.rs:1757-1767`) waits in `SendCredit::take` (`:121-137`)
  until the guest grants window, with no timeout. `Term.send`
  (`src/scripting/terminal.rs:72-79`) holds the terminal's state lock while
  it waits, and nothing takes the guest's output meanwhile: output is taken
  only by `expect` and `read`. A text longer than the window (256 KiB,
  `INITIAL_WINDOW`) sent to a shell that echoes can stop both ways: the guest
  cannot write its echo because the host grants no receive window, and may
  then stop reading input. Whether the guest agent stops reading in that
  state was not checked, so this is not a finding. A cancellation ends the
  wait. A `vmlab script` run or a handler has only the disconnect or the
  shutdown to end it.
- N12. The pointer position is a property of the handle, not of the machine
  (`src/scripting/mod.rs:97`, `:609`, `:1071-1088`). `lab.vm("dc").mouse_move(x,
  y)` and then `lab.vm("dc").mouse_click("left")` clicks at (0, 0), because
  each `lab.vm` makes a handle with its own position, and `mouse_click`
  always sends its handle's position (`src/labd/display/mod.rs:249-269`). The
  manual says so ("The position is remembered on the handle", "with no move
  yet the position is (0, 0)"), so it is by design. A helper function that
  takes the machine by name loses the position without a word.
- N13. `m.type_text_paced(text, delay_ms)` has no upper limit on `delay_ms`
  (`src/scripting/mod.rs:1063-1068`). The fix of first-pass F7 bounded the
  waits and `sleep_ms` at 365 days. This delay is not among them: a very
  large value types one character and then sleeps. It does not panic, and a
  cancellation ends it.
- N14. `wait_for_text` with a timeout shorter than one OCR run can never
  match (`src/scripting/mod.rs:1466-1486`): the timeout cuts the OCR that is
  running, and its result is dropped. `wait_for_text(p, 0)` never reads the
  screen, while `wait_for_image(p, 0)` looks one time (`:583-593`). The test
  `wait_for_text_keeps_its_timeout_while_tesseract_runs` shows that the cut
  is wanted. The difference between the two waits is not in the manual.
- N15. `Term.expect(p, 0)` never takes new output
  (`src/scripting/terminal.rs:130-137`): the deadline has passed before the
  first `pump`, so only what an earlier call left in the buffer can match.
- N16. `Term.expect` with a pattern that matches U+FFFD at the end of the
  buffer consumes the first bytes of a split character (`:110-119`,
  `raw_utf8_offset`). Measured: buffer `ok\xe2\x82`, pattern `ok.`, the match
  is `"ok\u{fffd}"` and the later byte `0xac` reads as `"\u{fffd}"`. A
  pattern that ends in `.` or `\s*$` can hit it. It is the rest of F8 and is
  rare.
- N17. A cancelled `m.restart()` leaves the machine stopped
  (`src/labd/lab.rs:2229-2250`: the stop, the wait and the start are one
  future, and `cancel::block_on` drops it). The first pass named the drop for
  every host call under F3 and followed `restore` only. The fix of F3 made
  `restore` and `snapshot` tasks of their own (`:4013`, `:4203`). `restart`,
  `usb_attach` and `copy_from` are as they were. The start itself runs to
  its end (`finish_start`, `:1620`).
- N18. First-pass N3 is unchanged (`src/labd/mod.rs:205-233`): no limit on
  the handler runs in flight, a `Lagged` receiver drops events without a log
  line (`:218`), and a handler that causes its own event loops. Each run
  holds one thread of tokio's blocking pool.
- N19. A handler's `print` without a newline is one log record for each call
  (`src/labd/mod.rs:225-226`: the sink logs each chunk with `trim_end`). The
  sink of `up` joins chunks into lines (`LinePrefix`,
  `src/labd/provisioning.rs:89-103`), and its buffer for a line that never
  ends has no limit of its own.
- N20. `seg.block_port(cidr, "icmp", port)` is accepted
  (`src/scripting/runner.rs:346-378`). I did not check what the rule engine
  does with an ICMP rule that has a port, or whether validation refuses the
  same rule in the lab file. It belongs to the networking review.
- N21. The queue that frees values without recursion (the fix of F2,
  `vendor/wscript-core/src/value.rs:149-192`) keeps what is left in it when
  the drop of a host value panics. The values stay on the thread, which the
  blocking pool uses again, until the next script on that thread frees a
  value. No host value of vmlab has a `Drop` that panics, as far as I read.

Checked and found right:

- The fix of F1: `script_forward_target` (`src/labd/lab.rs:2351-2404`) takes
  the NIC on the segment the handle names and the address the DHCP server
  vouches for, and `add_forward` binds that address to the NIC's MAC. A claim
  that is believed because the NIC has no lease still reaches only that NIC.
- The fix of F2 covers the four kinds a value can nest through without limit.
  Lists and maps directly inside one another are bounded by the static type.
  The parser charges chains of operators and calls against its nesting budget
  (`vendor/wscript-compiler/src/parser.rs`).
- The fix of F4: the first-boot claim travels with the spawned run
  (`src/labd/lab.rs:3273-3294`), and the timeout cancels and then waits.
- The fix of F6 and F7 in `terminal.rs`, `cancel.rs` and `script_timeout`.
  The region of the fix of F12 clamps to `u32::MAX`, and both crops
  (`src/vision/matching.rs:72-78`, `src/vision/ocr.rs:135-142`) take such a
  region without an overflow.
- A cancellation unwinds with `resume_unwind`, and no profile in `Cargo.toml`
  sets `panic = "abort"`.
- `vmlab validate` checks every provision, handler and first-boot script of
  the lab file with the entry its place asks for
  (`src/config/validate.rs:594-706`).

## What I read and what I did not

Read in full: `docs/reviews/wscript.md`, `src/scripting/runner.rs`,
`cancel.rs`, `terminal.rs`, `record.rs`, `ocr.rs`, `surface.rs`, and
`src/scripting/mod.rs` up to line 1700 (the tests after that by name only).

Read in part: `src/labd/lab.rs` (`until_cancelled`, `abandon_up`,
`start_machine_at`, `stop_machine_alone`, `restart_machine`,
`run_owed_first_boot`, `script_forward_target`, `run_step`, `run_first_boot`,
`stop_for_exit`, `snapshots`, the destroy path up to its rebuild, and the
tests named above), `src/labd/mod.rs` (handler dispatch,
`matching_handler_runs`, the `Run` verb), `src/labd/provisioning.rs`
(`LinePrefix`), `src/labd/record.rs` (`record_prepare`, `record_start`,
`record_launch`, `record_finalise_run`), `src/labd/vm_agent.rs`
(`SendCredit`, `RecvCredit`, `AgentSession`, `SessionSender`),
`src/labd/display/mod.rs` (the keyboard and mouse methods),
`src/net/dhcp.rs` (`vouched`), `src/vision/matching.rs` and `ocr.rs` (the
region crops), `src/cli/validate.rs` (`check_script`),
`src/config/validate.rs` (the callers of `check_script`),
`vendor/wscript-core/src/value.rs` (the `Value` type and `release`), and the
manual's pages `reference/wscript-machine.wcl` and `wscript-types.wcl` for
`mouse_move`, `mouse_click`, `snapshots` and `Term.read`.

Read of the language, at `faf09e6`: only a search of `wscript-vm/src` for the
fuel and for limits on size (there is a fuel and a depth limit, and no limit
on bytes). The dispatch loop, the builtins and the compiler were not read.

Not read: the guest agent's side of the terminal, `exec` and `fileops`
(`docs/reviews/guest-agents-2.md`). The snapshot, restore and USB code behind
the script calls (`docs/reviews/snapshots-2.md`, `docs/reviews/usb-2.md`).
The rule engine and the DNS zone behind the segment calls
(`docs/reviews/networking-2.md`). The recorder and the vision code beyond the
crops (`docs/reviews/record-vision.md`). The playbook runner. `src/cli` for
`vmlab script`. `src/template/trust.rs` and the template paths that embed a
script. `src/scripting/fixtures`, the examples and the e2e scripts. No GPU
code and nothing in the TCG crates. No lab was started. Apart from the six
probes and `just ci::check`, no vmlab test was run for this review.
