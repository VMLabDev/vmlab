# Review: wscript (the lab scripting)

Card #617. A bug-hunting review of `src/scripting`: the host module vmlab
gives a wscript script, the runner, the cancellation, the terminal, record and
OCR handles, and what a lab file or a template can make them do. It lists
faults and makes no fix. Reviewed at `next` commit `6db151df`. All paths are
from the repository root, and all line numbers are from that commit.

**Who is trusted.** PRD §2 names one trusted host user and says that guests and
pulled templates are untrusted. A script is code of the host user: a
`provision {}` or `on {}` script comes from the lab file, and a template's
first-boot script is refused at pull and at import unless its repository is in
`trusted_template_repositories` or the import says `--trust-scripts`
(`src/template/trust.rs:30-38`, `src/oci/client.rs:706-712`,
`src/template/store.rs:770-771`). So the findings below are of two kinds: what
a hostile guest reaches through a script that the host user wrote in good
faith (F1, F6), and what a script does wrong to the lab that runs it (the
rest). What a trusted script may do on the host by design is in Notes (N1).

Severity words: **security** (against a hostile guest or a peer),
**data loss**, **crash**, **wrong result**, **cosmetic**.

## How the findings were checked

Each finding is from reading the code. The language itself (`wscript`,
`wscript-vm`, `wscript-std`) was read at the revision `Cargo.toml` pins,
`faf09e6`, next to the two vendored crates.

Four facts were also measured on helios, with a scratch crate that depends on
the pinned `wscript` only (no vmlab code, no lab started, the crate removed
after the run):

- A script that builds a linked struct 3,000,000 nodes deep ends the process
  with `thread has overflowed its stack` and SIGABRT (F2). 1,000 nodes is
  fine.
- `Instant::now() + Duration::from_secs(i64::MAX as u64)` panics with
  `overflow when adding duration to instant` (F7).
- `main` with a parameter of another type compiles, and fails at run time with
  `type confusion: expected int, found opaque`. `main` that returns `int`
  runs to its end and then fails with `type mismatch at host boundary:
  expected unit, got int` (F9).
- Unbounded recursion faults with `stack overflow: too many nested calls`, and
  the fault carries 10,000 trace frames (F11).

No other finding was run. Each names the test that proves it. A finding whose
test does not reproduce is wrong and should be dropped.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | security (peer) | `seg.forward()` aims the host port at the address the guest reports, not at its DHCP lease (fixed, card #665) |
| F2 | crash | A script value nested deep enough overflows the native stack when it is dropped and aborts the lab daemon |
| F3 | wrong result | A provision cancelled inside `m.restore()` leaves the workspace syncer stopped |
| F4 | wrong result | A first-boot script runs on after its 1800 s timeout, and the next start runs a second one beside it |
| F5 | wrong result (hang) | A script that does not call the host cannot be stopped: `up` waits on it for ever, and so does a daemon that got SIGTERM |
| F6 | security (hostile guest) | A terminal's buffer has no limit, and `expect` scans all of it again on every chunk |
| F7 | crash (the script run) | A very large timeout or `sleep_ms` panics the script thread, or sleeps for ever |
| F8 | wrong result | `Term.expect` destroys a character that was split across two reads |
| F9 | wrong result | `vmlab validate` accepts a script with no entry point, or with the wrong one |
| F10 | wrong result | `print` and `println` write to the daemon's log file, not to the output of `up` |
| F11 | cosmetic | A recursion fault prints 10,000 function names on one line |
| F12 | cosmetic | A region number above 4,294,967,295 wraps instead of being refused |

### F1. A script forward follows the guest's claim, not the lease (security, peer)

- Where: `src/scripting/runner.rs:341-362` (`SegmentHandle::add_forward`),
  `src/labd/machine.rs:622-637` (`guest_ips`, `guest_ip`),
  `src/labd/lab.rs:2000-2052` (`forward_observations`, the fixed path).
- Why it is a fault: `seg.forward(host_port, vm, guest_port)` takes the target
  address from `machine.guest_ip(None)`. That is the first IPv4 address the
  guest's agent reports for any of the machine's NICs. The guest writes that
  report. Card #614 fixed the same fault for declared forwards and container
  ports (`docs/reviews/containers.md` F1): `forward_observations` now asks the
  segment's DHCP server which address the NIC holds
  (`DhcpServer::vouched`). The script path does not go through it. A guest
  that configures another machine's address on its NIC gets the host port,
  which listens on `0.0.0.0` outside worktree isolation
  (`src/labd/isolation.rs:165-170`), aimed at that other machine. The script
  author meant to publish `rogue:80` and publishes `victim:80`.
- A second fault in the same lines: the address is not tied to the segment
  the handle names. For a machine with two NICs, `lab.segment("b").forward(…)`
  installs the forward on segment `b` with the address of the first NIC that
  reports one, which may be on segment `a`. The comment at `:351` says "on
  this segment". The code does not check it.
- Proof: test `a_script_forward_targets_the_dhcp_lease_not_the_guest_claim` in
  `src/labd/lab.rs`, with the fixture of
  `a_forward_targets_the_dhcp_lease_not_the_guest_claim` (`:7874`): `victim`
  holds the lease `10.0.0.10`, `rogue` holds `10.0.0.11` and reports
  `10.0.0.10`. Run a script with `lab.segment("lan").forward(<free port>,
  "rogue", 80)`. Connect to the port and read which NIC the NAT engine sends
  the SYN to. Expected: `rogue`'s NIC, or an error that names the claim.
  Today: `victim`'s NIC.
- Fixed by card #665. `SegmentHandle::add_forward` asks
  `LabRuntime::script_forward_target` (`src/labd/lab.rs`), which takes the
  machine's NIC on the segment the handle names and the address the segment's
  DHCP server vouches for on that NIC, as `forward_observations` does. A claim
  the server refuses is an error that names it, and a machine with no NIC on
  the segment is refused. The NAT engine binds the address to that NIC's MAC,
  as for a declared forward. The proof test failed before the fix (the SYN
  went to `10.0.0.10`) and passes after it. A second test,
  `a_script_forward_targets_the_nic_on_its_own_segment`, covers the second
  fault.

### F2. Dropping a deep value overflows the native stack and aborts the daemon (crash)

- Where: `src/scripting/runner.rs:134-151` and `:189-205` (the script runs on
  a `spawn_blocking` thread of the lab daemon), `wscript-vm/src/ops.rs` at
  `faf09e6` (the test `deep_acyclic_data_faults_not_aborts` says: "Value's Rc
  drop recurses (a separate, known limitation — the ops guard walks, not
  drops)").
- Why it is a fault: the VM limits call depth (10,000 frames) and the depth
  of a value it compares, clones or prints. It does not limit the depth of a
  value it frees. A struct that holds an `Option` of itself can be made any
  depth in a loop. When the last reference goes, the drop recurses once per
  level on the native stack of a tokio blocking thread (2 MiB). A stack
  overflow is not a panic. It ends the process. The process is the lab
  daemon, which is the only manager of the lab's machines
  (`src/labd/mod.rs:231-234`). No error reaches `up`. A provision script, a
  handler and a first-boot script from a trusted template all reach it.
- Measured on helios: 3,000,000 nodes, debug build, 2 MiB thread: SIGABRT.
- Proof: test `a_deep_value_faults_instead_of_aborting` in
  `src/scripting/runner.rs`, run in a child process so that the abort is an
  exit status and not the end of the test binary. Script:

  ```wscript
  struct Node { next: Option[Node] }
  fn main() {
      let cur = Node { next: None }
      let i = 0
      while i < 3000000 {
          cur = Node { next: Some(cur) }
          i = i + 1
      }
  }
  ```

  Expected: the run returns, with or without a fault. Today: the child dies
  on SIGABRT.

### F3. A provision cancelled inside `m.restore()` leaves the workspace syncer stopped (wrong result)

- Where: `src/scripting/cancel.rs:71-86` (`block_on` drops the future it
  waits on when the token fires), `src/scripting/mod.rs:968-995` (`restore`,
  `restore_discarding_workspace`), `src/labd/lab.rs:3666` (`suspend`) and
  `:3721-3723` (`resume`), `src/labd/workspace/syncer.rs:598-609` and
  `:652-655`.
- Why it is a fault: `up` cancels every provision still running when one step
  fails (§7.2). A cancelled script stops at its host call by dropping the
  future of that call. `LabRuntime::restore` is written to run to its end: it
  takes the machine's syncer out of `running`, notes the rewind on the ledger,
  restores, and "puts back on both paths". A drop between `:3666` and `:3722`
  is a third path. The `Bracket` has no `Drop`, so the syncer is not started
  again. The comment at `:3718-3720` names the result: "A workspace whose
  syncer quietly never came back looks exactly like a machine with nothing to
  sync". Host and guest changes stop crossing until the machine is started
  again, and nothing says so. The machine's own restore is dropped at the
  same point, between its steps.
- The same drop reaches every other host call (`snapshot`, `start`,
  `restart`, `usb_attach`, `copy_from`). I followed only `restore`.
- Proof: test `a_cancelled_restore_puts_the_syncer_back` in
  `src/labd/lab.rs`: a lab with one `@dev` machine that has a workspace and a
  recorded snapshot, on a machine double whose `restore` waits on a gate. Run
  `fn main(lab: Lab) { let r = lab.this_vm().expect("vm").restore("s") }`
  with `ScriptOwner::provision("dev").cancelled_by(token)`. When the double
  has reached the gate, cancel the token and await the run
  (`ScriptCancelled`). Expected: `lab.workspaces.get("dev")` is `Some`.
  Today: `None`.

### F4. A first-boot script outlives its timeout (wrong result)

- Where: `src/labd/lab.rs:2909-2922` (`run_first_boot`: the script future
  under `tokio::time::timeout(1800 s)`), `:3986-4009` (`FirstBootClaim`),
  `src/scripting/runner.rs:52-59` (`ScriptOwner::first_boot` has no cancel
  token) and `:134-152`.
- Why it is a fault: the timeout drops the future that waits for the script.
  It does not stop the script. The script is on a blocking thread, a
  first-boot owner has no token, and `cancel::block_on` without a token is a
  plain `block_on`. So after `first-boot <vm>: timed out after 1800s` the
  script goes on: it types keys, runs commands and restarts the guest that
  the doc comment says is left "running for inspection" (`:2874-2875`). The
  `FirstBootClaim` is dropped with the future, and `firstboot.done` is not
  written, so the next `vm start` or `up` runs the first-boot script a second
  time while the first is still running against the same guest. The comment
  at `:2906-2907` gives the purpose of the timeout: "a hung guest must not
  wedge `up` forever". It frees `up` and leaves the script. The recordings of
  the abandoned run are not finalised either (`runner.rs:162` is not
  reached).
- The same happens when `up` aborts a machine that is in its first boot
  because another machine failed (`abandon_up`, `src/labd/lab.rs:354-370`,
  `starting.abort_all()`).
- Proof: test `a_first_boot_past_its_timeout_stops_its_script` in
  `src/labd/lab.rs`, with the tokio clock paused: a machine double with a
  pending first boot whose script is
  `fn main(lab: Lab) { let m = lab.this_vm().expect("vm")  let w = m.wait_shutdown(100000)  lab.log("still here")  let s = m.start() }`.
  Advance the clock past 1800 s. `run_first_boot` returns the timeout error.
  Advance past 100,000 s. Expected: no `still here` line and no `start` on
  the double. Today: both happen.

### F5. A script that does not call the host cannot be stopped (wrong result, hang)

- Where: `src/scripting/runner.rs:140` and `:194` (`Vm::new(&ctx)`: the
  default configuration, `fuel: None`), `src/scripting/cancel.rs:10-16` (the
  module says it: "A script spinning without ever calling the host runs on
  until it does", and a `vmlab script` run or a handler has no token at all),
  `src/labd/lab.rs:354-370` (`abandon_up` joins every provision),
  `src/labd/mod.rs:98-101` and `:231-238` (the SIGTERM path returns from
  `block_on` and drops the runtime).
- Why it is a fault: the wscript VM can meter a run (`Vm::set_fuel`,
  `VmConfig::fuel`). vmlab runs every script unmetered and checks the cancel
  token only inside a host call. Three results:
  1. `while true { }` in one provision, a failure in another: `abandon_up`
     cancels, then waits for the spinning script, which never looks at the
     token. `up` does not return. PRD §7.2 says the first failed step fails
     `up`.
  2. A lab daemon that gets SIGTERM while such a script runs (or a handler in
     `vmlab::sleep_ms` of a day, which without a token is one
     `std::thread::sleep`) finishes its teardown, returns from `run_async`
     and then waits in the drop of the tokio runtime for the blocking thread.
     The `shutdown` and `destroy` verbs call `process::exit` and are not
     affected (`src/labd/mod.rs:612-615`, `:1289-1292`).
  3. `vmlab script` has no cancel path: when the CLI is interrupted, the
     script goes on in the daemon and keeps driving the machines.
  The same missing limit lets a script grow a list until the daemon is out of
  memory.
- Proof: test `a_cancelled_script_stops_without_a_host_call` in
  `src/scripting/runner.rs`: run
  `fn main(lab: Lab) { let i = 0  while i < 200000000 { i = i + 1 }  lab.log("done") }`
  through `run_script_source` with an owner whose token is cancelled before
  the run starts. Expected: `ScriptCancelled`, and no `done` line. Today: the
  loop runs to its end, `done` is logged (`lab.log` does not look at the
  token either, `src/scripting/mod.rs:659-661`), and the run is `Ok`. The
  loop is bounded, so the test ends in both cases.

### F6. A terminal's buffer has no limit and `expect` is quadratic (security, hostile guest)

- Where: `src/scripting/terminal.rs:88-115` (`expect`) and `:145-185`
  (`pump`, `st.buf.extend(b)` at `:164`).
- Why it is a fault: PRD §2 says "Guest-to-host channels must validate
  messages and bound resource use". A collected `exec` obeys it: it stops at
  `EXEC_OUTPUT_LIMIT`, 64 MiB (`src/labd/vm_agent.rs:299`, `:1090-1101`). A
  script terminal does not. While `t.expect(pattern, timeout)` waits, every
  byte the guest's shell writes is appended to `buf`, and nothing but a match
  or `read()` empties it. A guest that never prints the pattern and writes as
  fast as the channel's window allows grows the lab daemon's memory for the
  whole timeout. Each pass of the loop also copies the whole buffer
  (`String::from_utf8_lossy(&st.buf).into_owned()`, `:93`) and runs the regex
  over all of it from the start, so the work is quadratic in the bytes
  received. A first-boot script may wait 1800 s.
- Proof: test `a_terminal_that_floods_is_ended_at_a_limit` in
  `src/scripting/terminal.rs`, with an agent double that answers
  `open_terminal` and sends `Data` frames as long as it has window (the
  flood peer of the `vm_agent.rs` tests): `t.expect("never", 3600)` under a
  paused clock. Expected: an error that names a limit, with
  `state.buf.len()` under that limit. Today: the buffer holds everything
  that was sent, and the call ends only at the timeout.
- Fixed by card #667. `pump` ends the session when the buffer would pass
  `TERM_BUFFER_LIMIT`, 16 MiB, closes the channel, and the waiting `expect`
  fails with an error that names the limit. `expect` scans a buffer once:
  it scans again only after new output arrived, and it borrows the buffer
  instead of copying it while the output is valid UTF-8. One correction to
  the text above: `pump` returns at the end of its window (500 ms), so the
  scan ran once per window, not once per chunk. The work of one `expect` is
  now at most one scan of 16 MiB per window that brought new output. The
  proof test needs no clock: its flood is twice the limit and then stops.
  Before the fix it failed with "the buffer holds 33554432 bytes, past the
  limit of 16777216".

### F7. A very large timeout panics the script thread, or sleeps for ever (crash of the script run)

- Where: `src/scripting/mod.rs:407` (`wait_for_text`), `:587` (`wait_for`),
  `:922` (`wait_rebooted`), `src/scripting/terminal.rs:90` (`expect`),
  `src/scripting/cancel.rs:94` (`sleep`, reached from `vmlab::sleep_ms`,
  `mod.rs:633-635`).
- Why it is a fault: each computes `Instant::now() + Duration::from_secs(n)`
  with `n` straight from the script (`timeout_secs.max(0) as u64`). For a
  large `n` the addition panics (measured). The panic unwinds the script
  thread, and the run ends with `script thread panicked`
  (`src/scripting/runner.rs:157`), with no word about the argument. A script
  author who writes `9223372036854775807` for "no timeout" gets that.
  `vmlab::sleep_ms` of the same size panics when the script has a cancel
  token and is one uninterruptible `std::thread::sleep` when it has none.
  The methods that pass the duration on (`wait_ready`, `wait_shutdown`,
  `exec_timeout`) were not followed into `src/labd`.
- Proof: test `a_huge_timeout_is_an_error_not_a_panic` in
  `src/scripting/terminal.rs`: a `TerminalHandle` whose state has
  `session: None` and `ended: Some("closed")`, then
  `t.expect("x", i64::MAX)`. Expected: `Err`. Today: a panic. The same test
  for `cancel::sleep(Duration::from_millis(i64::MAX as u64))` inside
  `enter(Some(token))`.

### F8. `Term.expect` destroys a character split across two reads (wrong result)

- Where: `src/scripting/terminal.rs:93-97`.
- Why it is a fault: `expect` matches on `String::from_utf8_lossy(&st.buf)`.
  On a match it writes the rest of that lossy text back as the new buffer
  (`st.buf = text.as_bytes()[found.end()..].to_vec()`). When the buffer ends
  in the first bytes of a multi-byte character (the read cut it), the lossy
  text holds U+FFFD there, and that replacement is what is kept. The rest of
  the character arrives with the next read and becomes a second U+FFFD. The
  next `expect` or `read` sees two replacement characters where the guest
  wrote one character. Output in a language that is not ASCII, and box
  drawing, hit it by chance.
- Proof: test `expect_keeps_a_character_split_across_reads` in
  `src/scripting/terminal.rs`: state `buf = b"ok\xe2\x82"`,
  `ended: Some("closed")`, `session: None`. `t.expect("ok", 1)` returns
  `"ok"`. Append `0xac` to `state.buf`. Expected: `t.read()` is `"€"`.
  Today: `"\u{fffd}\u{fffd}"`.

### F9. `vmlab validate` accepts a script with no entry point (wrong result)

- Where: `src/scripting/mod.rs:1381-1390` (`check_script_source`),
  `src/cli/validate.rs:98-101`, `src/scripting/runner.rs:149` and `:203`
  (`call_unit(&unit, "main" | "handle", …)`).
- Why it is a fault: validation compiles the script and does not look for the
  function vmlab will call. `call_unit` then checks the name and the number
  of arguments, not their types. So these pass `vmlab validate` (PRD §5.1
  has the scripts type-checked there) and fail at `up`, after the machines
  have booted:
  - no `main` (a typo, or a handler file given to `provision {}`): `the
    script does not define a function named main`,
  - `fn main()`: `main takes 0 argument(s), 1 were supplied`,
  - `fn handle(lab: Lab, event: Event)` (the two swapped): a `type
    confusion` fault at the first use (measured for `fn main(x: int)`),
  - `fn main(lab: Lab) -> int`: the script runs to its end, and then the run
    fails with `type mismatch at host boundary: expected unit, got int`
    (measured). `up` reports a failed provision whose work is done.
- Proof: test `a_script_without_its_entry_point_fails_validation` in
  `src/scripting/mod.rs`: `check_script_source("fn mian(lab: Lab) { }")` and
  `check_script_source("fn main(n: int) { }")`. Expected: `Err` for both.
  Today: `Ok` for both. The check needs to know whether the file is a
  provision or a handler, which `check_script` is not told.

### F10. `print` and `println` go to the daemon's log file (wrong result)

- Where: `src/scripting/runner.rs:140`, `:194` (`Vm::new`: `VmConfig::out`
  is `stdout_sink()`), `src/supervisor/mod.rs:597-605` (the lab daemon's
  stdout is `labd-<lab>.log`).
- Why it is a fault: `println` is in the language's prelude, and the examples
  of wscript use it. In a vmlab script it compiles, runs, and writes into the
  daemon's log file between the tracing lines. It does not reach the output
  of `up` or `vmlab script`, the lab log, or the event history, where
  `lab.log` goes. A script that reports with `println` looks silent.
- Proof: test `println_reaches_the_script_output` in
  `src/scripting/runner.rs`: `run_script_source` with a sink that collects,
  and `fn main(lab: Lab) { println("hello") }`. Expected: the sink holds
  `hello`. Today: it is empty.

### F11. A recursion fault prints 10,000 function names (cosmetic)

- Where: `src/scripting/runner.rs:242-261` (`run_error` joins every trace
  frame with ` <- `).
- Why it is a fault: a script that recurses without end stops at the VM's
  call depth, 10,000, and the fault carries one frame per call (measured).
  `run_error` puts them all on one line: `stack overflow: too many nested
  calls (at f <- f <- f …)`, about 50 KiB for a one-letter name. The line
  goes to the `up` output, the lab log and the error of the verb.
- Proof: extend `result_expect_reports_the_err_payload`'s helper `fault_of`
  (`:438`) with
  `fn f(n: int) -> int { f(n + 1) }  fn main() { let x = f(0) }` and assert
  that the message is shorter than 1,000 bytes.

### F12. A region number above `u32::MAX` wraps (cosmetic)

- Where: `src/scripting/mod.rs:546-558` (`match_opts`),
  `src/scripting/ocr.rs` (`parse_opts`, `let at = |v: i64| v.max(0) as u32`).
- Why it is a fault: a region element is clamped at zero and then cast with
  `as u32`, which keeps the low 32 bits. `[4294967301, 0, 10, 10]` searches
  at x = 5 instead of being refused or giving no match. The same form is in
  `m.ip_nic(n)` (`as usize`, harmless on 64 bit) and in `seg.unblock(id)` and
  `seg.dns_clear(id)` (`as u64`: `-1` asks for rule 18446744073709551615 and
  returns `false`).
- Proof: test `a_region_past_u32_is_refused` in `src/scripting/mod.rs`:
  `MachineHandle::match_opts(0.9, vec![4294967301, 0, 10, 10])`. Expected:
  `Err`, or a region whose x is `u32::MAX`. Today: `Ok` with x = 5.

## Notes

These have no proving test, or they are by design, or they belong to another
review.

- N1. What a script can do on the host beyond its lab. This is the card's
  first question, and the answer is: everything the host user can do with
  files. The wscript standard modules for files and processes are not
  registered (`Context::new()` has no `wscript-std`, `src/scripting/mod.rs:
  1361-1378`), so the whole reach is the `vmlab` module:
  - `m.copy_to(local, guest)` reads any host file the user can read: an
    absolute path is taken as it is, and a relative one may hold `..`
    (`resolve_ref`, `:500-507`, `:1188-1199`).
  - `m.copy_from(guest, local)` creates the parent directories and writes any
    host path (`:1202-1216`). `m.screenshot(path)` and `m.record(path, …)`
    write a PNG or a video to any host path (`:1073-1088`, `:439-455`).
    Writing `~/.bashrc` or `~/.ssh/authorized_keys` is code execution as the
    host user.
  - `vmlab::env(name)` reads any variable of the lab daemon's environment
    (`:640-642`), which may hold tokens.
  - `seg.forward` opens a host port on every interface, `m.usb_attach_force`
    takes a host USB device that is in use, and `lab.machines()` reaches
    every machine of the lab.
  For a template this is gated: `trust::scripts` refuses a first-boot script
  from a repository that is not trusted. A trusted repository is therefore
  trusted with the host account, not with a guest. For a lab file there is
  no gate: `vmlab up` in a directory somebody else wrote runs its provision
  scripts with these rights, and `vmlab validate` only compiles them. I did
  not find a sentence in the manual's wscript reference that says so
  (searched for "trusted", "absolute", "runs on the host"). Whether to
  confine script paths to the lab root is a design decision, not a bug, so
  it has no test here.
- N2. `copy_from` through a symlink a guest planted. `pull_file_as` writes
  through a symlink at the destination on purpose
  (`src/labd/vm_agent.rs:1242-1252`, "written through whatever symlink names
  it"). If a script pulls into a directory that the same guest can write
  through a virtiofs share, the guest can put a symlink at the destination
  name first, and the pull then overwrites the symlink's target with bytes
  the guest chose. I did not check that the in-process virtiofs server lets
  a guest create a symlink with a target outside the share, so this is not a
  finding.
- N3. Handler dispatch (`src/labd/mod.rs:205-226`): each matching event
  starts a run with no limit on the runs in flight, a `Lagged` receiver
  drops events without a log line, and a handler that causes its own event
  (a restart inside a `vm.started` handler) loops for ever. Each run builds
  the context and compiles the script again (`runner.rs:190-193`).
- N4. Guest text reaches the operator's terminal unchanged. `lab.log(r.stdout)`
  sends what the guest printed to the `up` stream. I found no filter for
  escape sequences on that path by search (`src/cli`, `src/proto`,
  `src/labd/provisioning.rs`). `vmlab exec` has the same property, so it is
  not a script fault.
- N5. `seg.forward` returns the forward's id, not its port
  (`runner.rs:392-412`). With `host_port = 0`, or with a port shifted under
  worktree isolation, the script cannot learn the port (the shift is only in
  the `forward.installed` event). No script method removes a forward, so the
  id has no use. A script forward does not follow the machine to a new
  address after a restart, as a declared forward does.
- N6. `m.logs(n)` reads the whole console log and fails on one byte that is
  not UTF-8 (`src/labd/vm.rs:2736-2745`). This is
  `docs/reviews/containers.md` F4. It holds for a VM's serial log too.
- N7. `record` options: `compress_idle` is parsed as `f64`, so `nan` and
  `inf` are accepted (`src/scripting/record.rs`). I did not follow them into
  `src/record`.
- N8. `wait_for_image` loads and decodes each reference image from disk on
  every poll (`mod.rs:560-576`). It costs time, and a file that changes
  under the wait changes the match.
- N9. `RecordingHandle::stop` and `TerminalHandle::close` use a plain
  `block_on` and are not cancellable (`record.rs`, `terminal.rs:135-141`). A
  cancelled script that is inside one of them stops only when it returns.
- N10. A cancelled script unwinds while `Term` holds its state mutex, which
  poisons it (`terminal.rs:91`, `expect("term state")`). Nothing reads it
  after, since the run is over.

Checked and found right:

- No file, process or network module of wscript is reachable from a script.
- The compiler limits nesting (`MAX_EXPR_DEPTH`), and the VM limits call
  depth and host re-entry, so source text alone does not overflow the stack
  (F2 is about values, at run time).
- A panic in a host function is not caught by the VM, so `cancel::stop`
  unwinds to the runner as intended, and `Entered` clears the thread's token
  on that path too (`cancel.rs:48-54`).
- `exec` and `exec_timeout` pass an argument vector, not a shell line, so a
  script value cannot add a command on the vmlab side. Output is limited to
  64 MiB.
- The regex in `expect` and `wait_for_text` is the `regex` crate, which has
  no exponential case.
- A first-boot script that restarts its own machine does not start a second
  first boot (`FirstBootClaim`).
- `this_vm()` outside a provision is an error, not a guess.

## What I read and what I did not

Read in full: `src/scripting/mod.rs`, `runner.rs`, `cancel.rs`,
`terminal.rs`, `record.rs`, `ocr.rs`, `surface.rs`, `src/template/trust.rs`.

Read in part: `src/labd/mod.rs` (handler dispatch, the `Run`, `Shutdown` and
`Destroy` verbs, `teardown`, `run`), `src/labd/lab.rs` (`run_step`,
`run_first_boot`, `restore`, `abandon_up`, `until_cancelled`,
`FirstBootClaim`, `forward_observations`, the test
`a_forward_targets_the_dhcp_lease_not_the_guest_claim`),
`src/labd/vm_agent.rs` (`exec`, `pull_file_as`, `AgentSession::recv`),
`src/labd/machine.rs` (`guest_ips`, `guest_ip`), `src/labd/container.rs`
(`guest_ips`), `src/labd/netservices.rs` (`add_forward`),
`src/labd/isolation.rs` (`forward_bind`), `src/labd/workspace/syncer.rs`
(`suspend`, `resume`), `src/labd/vm.rs` (`tail_file`),
`src/vision/matching.rs` (the region crop), `src/cli/validate.rs`
(`check_script`), `src/supervisor/mod.rs` (the lab daemon's spawn), PRD §2,
§6.1, §6.2 and §10, and the manual's page on `vmlab::env`.

Read of the language, at `faf09e6`: `wscript/src/lib.rs` (`Context`, `Vm`,
`call_unit`), `wscript-vm/src/lib.rs` (`VmConfig`, the limits, the print
sink), the head of `wscript-vm/src/builtins.rs` and `ops.rs`, and the file
list of `wscript-std`. The compiler and the rest of the VM were searched for
limits and not read.

Not read: the `Display` methods behind `send_keys`, `type_text`, the mouse
calls and `screenshot` (`src/labd/display.rs`, covered by
`docs/reviews/display.md`). The snapshot, start, restart and USB code a
script calls, beyond `restore` (`docs/reviews/snapshots.md`,
`docs/reviews/lab-lifecycle.md`). The guest agent's side of `exec`,
`fileops` and the terminal (`docs/reviews/guest-agents.md`). The playbook
runner. `src/scripting/fixtures`. The examples and the e2e scripts. The
manual, beyond the searches named in N1. No lab was started and no vmlab
test was run for this review, apart from `just ci::check`.
