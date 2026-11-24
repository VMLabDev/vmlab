# Review: guest agents and the agent protocol, second pass

Card #933, 2026-11-24. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

The first pass is `docs/reviews/guest-agents.md` (card #438). Its findings
F1 to F17 are fixed. This pass reads the code as it is on `next` now, looks
for faults the first pass missed and for faults its fixes brought in, and
does not repeat a first-pass finding.

Paths are relative to the repo root. Line numbers are those of commit
`cc001bbc` (`next` at the start of the card). Finding codes start at G1 so
they do not collide with the first pass.

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. No proposed test depends on a wall-clock limit: where a test has
to wait, it waits for an event (an exit, a file, a reaped process), and the
assertion is about state, not about time.

Severity words are those of the first pass: data loss, crash of the VM or
host process, security against a hostile guest or peer, wrong result,
cosmetic. Two findings are about privilege inside the guest: an account that
a `login {}` declares, or any local account of a Windows guest, becomes root
or SYSTEM. PRD §1.2 does not cover that line, because it is between two
accounts of the guest and not between the host user and the lab file.

## What was read and what was not

Read in full, without the test modules:

- the code each first-pass fix changed: `src/labd/vm_agent.rs` lines
  100-560 (`SendCredit`, `RecvCredit`, `SessionQueue`, the pull sinks and the
  pull temp file), 840-915 (`open`), 1077-1335 (`exec`, the push and pull
  entry points), 1540-1690 (`AgentSession`, `SessionSender`), 1762-2680
  (`FileOps`, the pump, the watchdog), 2714-3080 (`WatchSession`, the reader,
  `handle_ctrl`, `deliver`, `close_session`, `expose_terminal_socket`)
- `guest/agent/src/mux.rs` (without tests), `exec.rs`, `terminal.rs`,
  `fileops.rs`, `spawn.rs`
- `guest/agent/src/unix/login.rs` lines 40-720 (accounts, both routes, the
  cache, the thread identity)
- `guest/agent/src/unix/container.rs` (the jail and the `--nsexec`
  trampoline)
- `guest/agent/src/app.rs` lines 300-1100 (records, connection pumps, the
  Linux parking endpoint, X11 and waypipe setup)
- `src/labd/agent_repair.rs` lines 1-402 (the plans and the push)
- `src/labd/boot_id.rs` (without tests)
- `src/template/bootstrap/install.cmd`
- `guest/agent-proto/src/lib.rs` lines 190-400 (frames, the decoder,
  `RecvWindow`) and `guest/agent-proto/src/fileops.rs` lines 330-400 (the
  record decoder)
- `src/labd/container_ctl.rs` lines 60-140 and 280-330 (the reader task and
  `read_ctl_line`)
- the legacy agent part of commit `5ca3154b` (the F9 fix in
  `guest/agent-legacy/src/agent.c`)

Read in part:

- `guest/agent/src/unix/mod.rs` lines 397-700 (`app_home`, the routes, the
  exec and shell plans, `spawn_piped`) and 1110-1115 (`process_killer`)
- `guest/agent/src/unix/linux.rs` lines 100-145 and 280-515 (app
  directories, clipboard, clock, runtime directory, `ThreadIds`)
- `guest/agent/src/windows/proc.rs` lines 1-330 (environment block, piped
  spawn, `create`)
- `guest/agent/src/watch/mod.rs` lines 1-140 (open and the drain loop)
- `guest/agent/src/tail.rs` (open and `open_regular`)
- `guest/agent/src/metrics.rs` (the interval clamp only)
- `src/labd/mod.rs` lines 470-528 (`pump_session`)
- `src/labd/playbook.rs` lines 1180-1235 (`exec_streaming`)
- `src/labd/app_share.rs` lines 249-300 (the socket bind)
- `src/labd/workspace/syncer.rs` lines 1405-1435 (the select of the loop)
- `guest/agent/src/sessions_test.rs` and `guest/agent/src/app_test.rs`:
  names and the helpers a proposed test would use

Not read:

- `guest/agent/src/windows/` apart from `proc.rs`: ConPTY, the logon tokens
  and their cache, the port adapter and its reset, the event log, the
  clipboard, RDP preparation, the service, the job-object trees
- `guest/agent/src/unix/macos.rs`, `shepherd.rs`, `darwin.rs`, most of
  `unix/mod.rs` (the PTY child, the terminal fork path, the path resolver)
- `guest/agent/src/watch/` backends (`inotify.rs`, `rdcw.rs`) and the dirty
  set, and the body of `guest/agent-proto/src/watch.rs` and `app.rs`
- `guest/agent/src/clipboard.rs`, `rdp.rs`, `rtc.rs`, `svc.rs`, `logon.rs`,
  `main.rs`
- `guest/cinit/src/`: mounts, users, networking, the container start, health
  checks
- `guest/agent-legacy/src/` apart from the F9 diff, and
  `guest/agent-templeos/`
- `src/labd/boot_clock.rs`, `identity.rs`, `agent_conformance.rs`,
  `legacy_agent_tests.rs`, most of `app_share.rs`, `src/appshare/`,
  `src/cli/tty_attach.rs`
- `src/labd/workspace/` apart from the select named above: the syncer has its
  own reviews

## Findings, worst first

### G1. An X11 app session lets its account turn any guest file into its own, as root

- Where: `guest/agent/src/app.rs:898-936` (`listen`) and `938-943`
  (`give`), after `guest/agent/src/unix/linux.rs:120-138` (`make_app_dir`)
  has chowned the private directory to the session's account
  (`guest/agent/src/unix/mod.rs:403-413`).
- Fault: `make_app_dir` creates `/run/vmlab-app/<agent pid>-<channel>`,
  mode 0700, and hands it to the session's account. Only after that does
  `listen` write into it as root. For X11 it calls
  `std::fs::write(home.dir/Xauthority, …)` (line 911), which follows a
  symlink and truncates its target, and then `give` on the same path
  (line 912), which runs `chown` and `chmod 0600`, both of which follow a
  symlink. The account owns the directory, so it can put a symlink named
  `Xauthority` there first. The directory name is the agent's pid and the
  channel id, both easy to guess, and `/run/vmlab-app` is 0711, so the
  account can `stat` the name in a loop until the directory appears. With
  `Xauthority -> /etc/shadow`, root overwrites `/etc/shadow` with the cookie
  bytes and then gives the file to the account with mode 0600. The account
  then writes a root password. The waypipe path has the same shape with a
  race in place of a plant: between `UnixListener::bind(wp.sock)` and
  `give(wp.sock)` (lines 902-903) the account can replace the socket with a
  symlink, and `give` then chowns the symlink's target.
- Severity: security inside the guest. Any process running as the declared
  login account becomes root the next time the host runs `vmlab app run`
  for that login in X11 mode. The account needs no other right.
- Confidence: high for the order of the calls and for `fs::write`, `chown`
  and `chmod` following symlinks. The waypipe race was not timed.
- Test: `an_app_session_does_not_write_through_a_planted_link` in
  `guest/agent/src/app_test.rs` (Linux). Make a tempdir as the private
  directory, owned by the test's own uid, with a symlink `Xauthority` that
  points to a file holding `keep me` with mode 0644. Build an `AppHome` with
  that `dir`, a tempdir `x11_unix`, and `owner: Some((own uid, own gid))`,
  and call `listen` with `Protocol::X11`. Assert that the target still reads
  `keep me` and still has mode 0644. The test runs without root because the
  chown is to the test's own ids. It fails today.

### G2. Every local account of a Windows guest can replace the SYSTEM agent

- Where: `src/template/bootstrap/install.cmd:14-16` creates
  `C:\ProgramData\vmlab` with a plain `mkdir` and copies the agent there.
  `src/labd/agent_repair.rs:163-200` (the Windows repair plan) stages the
  new binary in the same directory and moves it into place.
- Fault: a directory made with `mkdir` under `C:\ProgramData` inherits that
  directory's access list, in which `BUILTIN\Users` may create files and
  folders and `CREATOR OWNER` has full control of what it creates. So every
  local account of the guest can create files in the agent's directory. Two
  routes to SYSTEM follow:
  - DLL planting, with no vmlab action at all. The directory of the service
    executable is the first place Windows looks for a DLL that is not a
    known DLL. A standard user who drops such a DLL beside
    `vmlab-agent.exe` has it loaded into the SYSTEM service at the next
    start of the service or the guest.
  - The staged binary. Before an agent refresh (which `up` does by default
    on a stale agent) or a `vmlab machine repair-agent`, a standard user
    creates `vmlab-agent.new.exe`. The push opens it with create and
    truncate (`FileOps::push` through `open_for_push`), which keeps the
    file's owner and access list, and writes the shipped bytes. The digest
    matches. `move /y` then makes that user-owned file the service
    executable. The user still has full control of it, renames it aside and
    puts their own program at the install path, which the service's restart
    actions or the next boot run as SYSTEM.
- Severity: security inside the guest (any local account to SYSTEM). The
  win3d installer already protects its own directory under `ProgramData`
  against the same inheritance (`guest/win3d/installer/vmlab-win3d.ps1`
  around line 163), so the risk is known in the project.
- Confidence: high for the inherited access list of a `mkdir` under
  `ProgramData` on Windows 7 and later. The DLL names that the agent loads
  from its own directory were not listed.
- Test: a live check on a Windows template, because no unit test sees an
  access list. As a standard account (`vmlab exec --user <standard>
  --password …`), run `cmd /c "echo x > C:\ProgramData\vmlab\probe.dll"`.
  Assert a non-zero exit. Today it exits 0. A unit-level guard for the fix:
  a test that the repair plan refuses a staging file that exists before the
  push, and that `install.cmd` sets an explicit access list (no inheritance,
  SYSTEM and Administrators full, Users read and execute) on the directory.

### G3. Closing an exec kills only its first process, and on two routes not even that

- Where: `guest/agent/src/spawn.rs:313` (`process_killer(child.id())`) and
  `guest/agent/src/unix/mod.rs:1110-1115` (`process_killer` sends SIGKILL to
  that one pid). The routes: `guest/agent/src/unix/mod.rs:512-530` (PAM, the
  child is `su`) and `484-505` with `guest/agent/src/unix/container.rs:315-345`
  (container, the child is the trampoline, which forks the real command and
  waits in `mirror_child_exit`).
- Fault: the kill hook of an exec, run on a host `close`, signals the pid that
  `Command::spawn` returned and nothing else. There is no process group and
  no tree.
  - Agent route: `sh -c 'long_step; next_step'` loses `sh`, and `long_step`
    keeps running, orphaned.
  - PAM route: the pid is `su`. `su` forks the command and waits for it, and
    a SIGKILL of `su` does not reach the child. The command keeps running.
  - Container route with namespaces: the pid is the trampoline's outer
    process, which only waits. The command is its child inside the PID
    namespace and keeps running.
  The F3 fix of the first pass relies on the opposite: "the session is
  dropped, which closes the channel and ends the command in the guest". A
  command that writes dies later of SIGPIPE when its output pump ends. A
  command that does not write (a `sleep`, a busy loop, a server that logs to
  a file) is not stopped. The same holds for every timed-out collected exec,
  for a `vmlab exec` that the user interrupts, and for a cancelled provision
  step, which then races its own retry.
- Severity: wrong result (commands that the host ended keep running in the
  guest, and keep their files and ports).
- Confidence: high for the agent route and the trampoline. High for `su`
  from how util-linux and shadow `su` wait for their child.
- Test: `exec_close_kills_what_the_command_started` in
  `guest/agent/src/sessions_test.rs`. Run
  `sh -c 'sleep 300 & printf "PID=%s CHILD=%s END\n" $$ $!; wait'`, read
  the line, send `Close`, wait until the shell's pid is gone (the existing
  `assert_process_gone` loop, or a wait on the reaper), then assert that
  `kill(child, 0)` fails with `ESRCH`. Today the `sleep` is still there. The
  PAM and container routes need a guest with `su` and PAM, or root for the
  namespaces, and belong in `src/labd/agent_conformance.rs`.

### G4. `AgentSession::recv` is not safe to cancel, and four `select!` loops cancel it

- Where: `src/labd/vm_agent.rs:1555-1583` (`AgentSession::recv`). The
  callers that put it in a `select!`: `src/labd/mod.rs:477-520`
  (`pump_session`, against a 500 ms timer), `src/labd/vm_agent.rs:3044-3065`
  (`expose_terminal_socket`, against the client's input),
  `src/labd/app_share.rs:383-384` and `441` (the app relay), and
  `src/labd/workspace/syncer.rs:1416-1422` (through `WatchSession::recv`,
  against host events and the debounce timer).
- Fault: `recv` pops an event from the queue, and when the consumed bytes
  reach `WINDOW_REPLENISH` it adds the grant to `RecvCredit` and then awaits
  `send_msg(WindowAdjust)`, which waits for the writer lock and for room in
  the socket. When another branch of the `select!` wins during that wait,
  the future is dropped. Three things are lost: the popped event (up to a
  frame, or more where the queue joined frames), the `window_adjust` (the
  host counted the grant, the guest never gets it, so the guest has
  `WINDOW_REPLENISH` less credit for the rest of the channel, and a second
  loss stops the channel for good), and possibly the middle of a frame,
  because `send_msg` can be dropped inside `write_all`, which tears the
  stream for every channel. The `SessionQueue` documentation promises that
  nothing is lost on cancel, which is true of the queue and not of
  `AgentSession::recv` above it. The wait happens when another task holds
  the writer, for example a file push or another session on the same
  connection while the guest drains its port slowly.
- Severity: data loss in streamed output (`vmlab tail`, the event log, a
  terminal), a channel that stops, and for the syncer a watch channel that
  fails and forces a stat-walk.
- Confidence: high for the cancel points. How often the timer meets a
  contended writer was not measured.
- Test: `a_cancelled_recv_loses_neither_bytes_nor_credit` in
  `src/labd/vm_agent.rs`. The mock agent opens an exec channel and sends
  three frames of 64 KiB, each holding its index. The test takes
  `handle.inner.writer.lock()` and holds it, then calls
  `session.recv().now_or_never()` until one call returns `None` (the second
  frame crosses the replenish point and the call waits for the writer).
  Release the lock, read the rest, and assert that all three frames arrived
  in order and that the mock agent received a `window_adjust`. Today the
  second frame is missing and no grant reaches the agent.

### G5. An exec's background child is killed by SIGPIPE two seconds after the exec ends

- Where: `guest/agent/src/exec.rs:120-135` (the reaper's drain) with
  `guest/agent/src/mux.rs:900-920` (`pump_out_with`). Brought in by the F9
  fix (`5ca3154b`).
- Fault: when the pumps have not ended within `DRAIN` after the exit, the
  reaper stops forwarding and closes the credit. A pump that is blocked in
  `read` stays there. When the background child next writes, the pump reads
  that chunk, `credit.take` returns 0, and `pump_out_with` returns, which
  drops the read end of the pipe. The child's next write to stdout or stderr
  raises SIGPIPE, which kills it unless it ignores the signal. F9 named
  `vmlab exec vm -- sh -c 'mydaemon &'` as the case to fix. That command now
  returns code 0, and `mydaemon` dies at its second log line after the
  first two seconds (a `python3 -m http.server &` dies at its second
  request). Before the fix the exec hung, which was at least visible.
- Severity: wrong result (a daemon that a provision step started dies
  later, with no error on the host).
- Confidence: high.
- Test: `a_background_child_survives_its_execs_drain` in
  `guest/agent/src/sessions_test.rs`. Run
  `sh -c '(while [ ! -e "$1/go" ]; do sleep 0.05; done; head -c 1048576 /dev/zero; echo $? > "$1/status") & exit 0' sh <tmp>`.
  Wait for `Exited`. Create `<tmp>/go`. Wait until `<tmp>/status` exists,
  and assert that it reads `0`. One MiB is more than the pipe buffer and a
  pump read together, so a write after the pump has gone is certain. Today
  the status is 141.

### G6. An honest guest loses an exec channel when stdout and stderr interleave behind a slow consumer

- Where: `src/labd/vm_agent.rs:247-271` (`SessionQueue::push`, line 264).
  Brought in by the F5 fix (`2d06574f`).
- Fault: past `SESSION_QUEUE` (2048) events a frame joins the last event only
  when that event is of the same stream. A frame of the other stream returns
  `false`, and `deliver` closes the channel. The receive window bounds bytes,
  not frames, so a guest that stays well inside its 256 KiB of credit can
  reach 2048 events: a program that writes short lines to stdout and stderr
  in turn produces one small frame per line. The agent behaves correctly in
  every respect. The consumer only has to be behind, which
  `exec_streaming` (`src/labd/playbook.rs:1192-1234`, a playbook run, whose
  `on_line` writes each stderr line to the client) can be. F5's text names
  this case as the intended outcome. It is still a fault, because the guest
  broke no rule and the command's result is lost: the stream could be kept
  with a per-stream join or with a bound in bytes.
- Severity: wrong result (a playbook or streamed exec fails with "the
  consumer of this channel fell behind").
- Confidence: high for the rule. How often a playbook run is that far behind
  was not measured.
- Test: `interleaved_streams_inside_the_window_keep_the_channel` in
  `src/labd/vm_agent.rs`. The mock agent opens an exec channel and sends
  1100 pairs of one-byte `Data` and `DataErr` frames, 2200 bytes in all,
  then `eof` and `exited 0`, while the test does not call `recv`. Then read
  the channel to its end and assert 1100 bytes on each stream and exit code
  0. Today the channel ends with the queue error.

### G7. The Windows agent does not refuse a NUL in an argument, a variable or a working directory

- Where: `guest/agent/src/spawn.rs:211-262` (`checked_command_line`,
  `render_command_line`), `guest/agent/src/windows/proc.rs:66-90`
  (`EnvBlock::from_vars`, `with_overrides`), and `wide` at
  `guest/agent/src/windows/port.rs:54`. Used by every exec with a logon or a
  tree (`windows/proc.rs:203`) and by every terminal
  (`windows/conpty.rs:107`).
- Fault: `wide` turns a NUL into a 0 unit, and `CreateProcessW` reads the
  command line up to the first one. An argument `a\0b` therefore cuts the
  command line, and everything after the NUL is not run. In the environment
  block a NUL ends one entry early and starts another, so a value
  `x\0PATH=C:\evil` adds a `PATH`, and a value that starts with a NUL ends
  the whole block. The same NUL in the working directory names another
  directory. The plain agent-identity exec goes through
  `std::process::Command`, which refuses a NUL, and the legacy agent refuses
  it too (`src/labd/legacy_agent_tests.rs:430-433`). Only these routes do
  not.
- Severity: wrong result (a command other than the one asked for runs as
  the declared login). The host is trusted, so this is not a boundary.
- Confidence: high.
- Test: in `guest/agent/src/spawn.rs` tests, which build on Linux too:
  `assert!(checked_command_line(&["prog".into(), "a\0b".into()]).is_err())`.
  Today it returns `Ok`. For the environment, a Windows-only test that
  `env_block` or `with_overrides` refuses a key or value with a NUL or a key
  with `=`.

### G8. A pull fails for a destination whose name is near the file name limit

- Where: `src/labd/vm_agent.rs:492-511` (`create_pull_temp`, line 497).
  Brought in by the F1 fix (`12cfd73e`).
- Fault: the temporary file is named
  `.<name>.vmlab-pull.<pid>.<n>`, about 22 bytes longer than the
  destination's own name. A destination name of more than about 233 bytes
  gives a temporary name past the 255-byte limit of most Linux file systems,
  and the pull fails with "File name too long" before any byte is read. The
  same pull worked before the fix.
- Severity: wrong result.
- Confidence: high.
- Test: `a_pull_onto_a_long_name_succeeds` in `src/labd/vm_agent.rs`. Pull a
  file from the mock agent to `<tmp>/` followed by 250 `a` characters.
  Assert success and the content. Today it fails at the create.

### G9. A pull needs write access to the destination's directory, not only to the destination

- Where: `src/labd/vm_agent.rs:1279-1302` (`pull_file_as`). Brought in by
  the F1 fix (`12cfd73e`).
- Fault: an existing regular destination is now replaced by a rename of a new
  file beside it. That needs the right to create and rename in the
  directory. A file the user may write in a directory they may not write in
  could be pulled onto before the fix and now fails with "permission denied"
  at the create of the temporary file. The rename also gives the file a new
  inode, so a hard link to the old file keeps the old bytes, and the owner,
  the access control list and the extended attributes of the old file are
  not kept (only the mode is).
- Severity: wrong result.
- Confidence: high.
- Test: `a_pull_onto_a_writable_file_in_a_read_only_directory_succeeds`
  in `src/labd/vm_agent.rs` (as a user that is not root, which the merge
  bar is). Make `<tmp>/ro/f` with mode 0644, set `<tmp>/ro` to 0555, pull
  onto `<tmp>/ro/f`, assert success and the content, then put the mode of
  the directory back. Today it fails.

## Notes

These have no test that proves a fault, or they are limits that the code
states. They are listed so that the lead can decide.

- N1. The free-space bound of the F7 fix (`pull_room`,
  `src/labd/vm_agent.rs:404-413`) keeps a twentieth of the file system free,
  at most 1 GiB. On a small file system that is nearly full (a tmpfs, a
  small home partition) the room is 0 and every pull fails, even one of a
  few bytes that would fit. The bound also does not count the space that
  replacing an existing destination frees. And when a later measurement
  returns `None` (`fstatvfs` fails), `FileSink::write` drops the bound for
  the rest of the pull. A test needs a file system of a chosen size.
- N2. When the destination of a pull is a dangling symlink,
  `tokio::fs::metadata` fails, the target is the link itself, and the rename
  replaces the link with a file (`src/labd/vm_agent.rs:1265-1275`). Before
  the F1 fix `File::create` created the link's target. Which is wanted is a
  decision.
- N3. On the `setuid` route the working directory is entered as root:
  `std::process::Command` runs `chdir` in the child before the `pre_exec`
  hook that drops the ids (`guest/agent/src/unix/mod.rs:670-690`,
  `spawn_piped_with`). A session can start in a directory it could not enter.
  The kernel still checks every later lookup, so nothing is read that the
  account could not read.
- N4. On the PAM route an account that resolves by a uid that has no
  passwd entry gets the name `"4242"` (`resolve_in`,
  `guest/agent/src/unix/login.rs:331-345`), and `su -l 4242` then fails
  because `su` takes names. First-pass N3 (no `--` before the account) is
  still open (`login.rs:474-481`).
- N5. `Op::Setstat` with a time or a size opens the path for writing
  (`guest/agent/src/fileops.rs:359-374`). That fails for a directory, and for
  a read-only file that the session owns, although `utimensat` would succeed
  for both.
- N6. The ctl reader stops for good on a `boot` event with another protocol
  version at any time, not only at boot (`src/labd/container_ctl.rs:117-123`).
  A replayed or forged line after the start has the effect that F11 removed
  for bad bytes. Whether a workload can write the ctl port was not checked.
  `read_ctl_line` also skips a line whose content is exactly
  `MAX_CTL_LINE` bytes, because the newline is counted (line 295).
- N7. The Windows boot identity falls back to `LastBootUpTime` when the
  `BootId` registry value is missing (`src/labd/boot_id.rs`, the `WINDOWS`
  probes). That value is the clock less the uptime, so a `set_time` (which
  `boot_clock.rs` sends on a fresh handshake past 2 s of drift) changes it
  and `wait_rebooted` can see a reboot that did not happen. Which Windows
  releases lack `BootId` was not checked.
- N8. First-pass N7 is still open: `ReadDir` skips an iterator error with
  `continue` (`guest/agent/src/fileops.rs:407`), and an entry whose
  `metadata` fails is skipped too (line 419). A syncer walk then sees the
  entry as absent.
- N9. `app_share.rs:279-290` binds the control and connection sockets and
  then sets mode 0600, which leaves a moment with the process umask. It is
  the same shape as first-pass N6 for the terminal socket. Whether other
  local users can reach the run directory was not checked.

## Other work seen

- The F5 limit stays: `expose_terminal_socket` does not read terminal output
  while it waits for guest credit (`src/labd/vm_agent.rs:3044-3052`).
