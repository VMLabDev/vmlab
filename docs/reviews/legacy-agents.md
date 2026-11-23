# Review: the legacy guest agents

Card #701, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Paths are relative to the repo root. Line numbers are those of commit
`87e46fba` (`next` at the start of the card).

## How the review was done

The review is by reading. Five findings were also run against the POSIX
build of the C agent (`cc` of `json.c`, `wire.c`, `agent.c`, `plat_posix.c`,
started with `--listen`) with a scratch client on the build host. Those say
"Proved" and give what was seen. The scratch client is not in the repo. Every
other finding is a claim from the code, and the proposed test is what proves
or refutes it. No DOS, Windows 9x, Windows NT or TempleOS guest was started,
and the Win32 build was not run under Wine. "Confidence" says how sure the
reading is.

Three directions were read, as the card asks:

- What a host request makes the old agent do in the guest beyond its intent.
- What a guest can send the host over the serial line.
- The handshake rules on ISA serial (#678) and the token barrier.

What the host sends today matters for reach. No call site was found that
sends `env`, `cwd` or stdin on an exec: `vmlab exec`, the wscript `exec`
and the daemon's own execs pass an argument vector only
(`src/cli/mod.rs:217-226`, `src/scripting/mod.rs:536`,
`src/labd/mod.rs:805`, `src/labd/vm.rs:2737`, `src/labd/lab.rs:249`). The
wire carries all three, and the agents implement them. A finding that needs
one of them says "wire only today".

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. The findings are ordered by
that class first and by how easily the fault is reached second.

`docs/reviews/guest-agents.md` (card #438) read `agent.c`, `wire.c`,
`json.c` and the host's connection, reader and sessions before this card.
Its findings are not repeated here. That covers the guest-to-host direction
of frame lengths and floods on every transport, the serial one included
(its F3, F4, F5, F17, N1, N8, N9, N10). This review found nothing in that
direction that is special to the serial line. See N10 and N13.

## What was read and what was not

Read in full:

- `guest/agent-legacy/src/agent.c`, `agent.h`, `wire.c`, `wire.h`, `json.c`,
  `json.h`, `plat.h`, `plat_dos.c`, `plat_posix.c`, `plat_win32.c`
- `guest/agent-legacy/README.md`, `guest/build-agent-legacy.sh`
- `guest/agent-templeos/VmlabAgt.HC`
- `src/labd/legacy_agent_tests.rs`
- `src/template/bootstrap/INSTALL.BAT`, `install-9x.bat`, `install-nt.cmd`
- `hv/crates/vmlab-hv-devices/src/serial/socket.rs`

Read in part:

- `src/labd/vm_agent.rs` lines 1-85, 295-312, 535-800, 840-960, 1380-1431,
  1575-1667, 2755-2910 (the serial greeting, the handshake, ping, open,
  `set_time`, `shutdown_guest`, the send path, the reader)
- `src/labd/machine.rs` lines 1060-1640 (`AgentSlot`, `dial`, `probe`,
  `check_cached`, `FallbackHello`, `cached_verdict`)
- `hv/crates/vmlab-hv-devices/src/serial.rs` lines 185-300 and 335-372
  (receive, transmit, the socket service)
- `hv/crates/vmlab-hv-vmm/src/pm.rs` (the `PM1a_CNT` port and S5 only)
- `src/agent_asset.rs` lines 160-235 (`templeos_typescript`)
- `src/template/agent_install.rs` lines 20-60 (`is_legacy_agent`)
- `src/labd/agent_conformance.rs` lines 60-135 and 405-452 (the process
  fixture and the shutdown check)
- `src/labd/boot_clock.rs` (which features the clock check needs)

Not read:

- `src/labd/agent_repair.rs`, `boot_id.rs`, `src/template/build.rs`,
  `src/template/bootstrap.rs`, `src/scripting/` (only the call sites of the
  legacy tier were looked at)
- `src/labd/agent_conformance.rs` apart from the lines above
- `guest/agent-proto/src/lib.rs` (the host decoder; card #438 read it)
- the rest of `hv/crates/vmlab-hv-devices/src/serial.rs` and its tests
- `src/config/validate.rs`, `src/profiles/` (which profiles select
  `isa-serial`)
- the OpenWatcom and mingw runtimes. Where a finding depends on what a
  runtime call does (`putenv`, `system`, `spawnvp`, `WideCharToMultiByte`),
  the confidence says so.

## Findings, worst first

### F1. On Windows a character outside the ANSI code page becomes `?`, which is a wildcard (fixed, card #715)

- Where: `guest/agent-legacy/src/plat_win32.c:97`
  (`WideCharToMultiByte(CP_ACP, 0, ..., NULL, NULL)`), used for every
  argument (`457`), the `cwd` (`470`) and every environment entry (`319`).
- Fault: with flags 0 and no `lpUsedDefaultChar` the call replaces each
  character the guest's code page lacks with the default character `?` and
  reports success. The comment at `318` says an entry that does not convert
  is left out, but a lossy conversion is not seen as a failure. The command
  runs with other arguments than the host sent. `?` matches any one
  character in `cmd.exe` and `command.com` built-ins, so
  `del` of a two-character name outside the code page becomes `del ??.txt`.
- Severity: data loss.
- Confidence: high for the substitution (documented behaviour of the call),
  medium for the reach, because it needs such a name in a destructive
  command.
- Test: on a `windows-xp` guest with a Western code page, create
  `C:\t\ab.txt` in an empty directory `C:\t` and run
  `vmlab exec <vm> -- cmd /c del C:\t\日本.txt`. Today `ab.txt` is
  deleted. Assert that the exec is refused with an error that names the
  argument. The same steps run under Wine with the `windows-nt-x86` build.
- Fixed by card #715. `utf8_to_ansi` converts its result back to UTF-16 and
  compares it with what it converted. Text that does not survive the round
  trip fails the exec before anything runs, for an argument, the `cwd` and an
  environment entry alike. That also refuses a best-fit look-alike (a
  full-width quote that would arrive as `"`) and bytes that are not UTF-8,
  which the decoder turned into `?` itself. The error names the string by
  position and gives the first code point that did not convert, for example
  `exec: argument 3 has the character U+65E5, which the guest's code page
  1252 cannot represent; nothing was run`. An environment entry that does
  not convert was left out before and now fails the exec. The proof is
  `legacy_win32_exec_refuses_text_outside_the_code_page` in
  `src/labd/legacy_agent_tests.rs`. It builds the NT agent with mingw and
  runs it under Wine with COM1 on a pty, and it is skipped on a host that
  has neither. Not run on a real Windows guest.

### F2. On DOS an exec with a `cwd` that does not exist runs in the agent's directory (fixed, card #721)

- Where: `guest/agent-legacy/src/plat_dos.c:211-215`. The results of
  `_dos_setdrive` and `chdir` are not looked at.
- Fault: the command runs in whatever directory the agent has, and the exec
  reports success. The Win32 build fails the exec (`CreateProcessA` with a
  bad directory, `plat_win32.c:506`) and the POSIX build exits 127
  (`plat_posix.c:229`). A command that names files by a relative path or a
  wildcard (`DEL *.TMP`, `COPY`) then works on the wrong directory. A
  `cwd` on a drive letter outside `A`-`Z` gives `_dos_setdrive` a number
  from arithmetic on the character.
- Severity: data loss. Wire only today: no host caller sends a `cwd`.
- Confidence: high for the ignored results.
- Test: `dos_exec_refuses_missing_cwd` in `src/labd/agent_conformance.rs`
  opens `CD` with a missing directory on a FreeDOS guest through the
  fixture's external socket. It requires an error reply or nonzero exit,
  with a diagnostic naming the directory. Before the fix, the agent
  printed its current directory and exited 0.
- Fixed by card #721. The DOS adapter validates the drive letter, verifies
  the selected drive and checks `chdir` before launching a child. It restores
  the original directory and returns an exec error naming the requested path
  when the directory change fails.

### F3. The NT install puts a SYSTEM service binary in a directory with inherited permissions

- Where: `src/template/bootstrap/install-nt.cmd:7-11` (`mkdir C:\vmlab`,
  copy, `--install`), `guest/agent-legacy/src/plat_win32.c:1062-1064` (the
  service runs as LocalSystem and starts at boot).
- Fault: the directory gets the permissions of the root of `C:`. On NT4 and
  Windows 2000 that is full control for Everyone, as far as the reviewer
  knows the defaults. A user of the guest without administrator rights can
  then replace `vmlab-agent-legacy.exe`, or on XP add a DLL beside it, and
  have it run as SYSTEM at the next boot. The install sets no ACL.
- Severity: security, inside the guest (a local user of the guest becomes
  SYSTEM). It is not against the host.
- Confidence: medium. The default ACLs are from memory and were not read
  off a guest.
- Test: on a `windows-legacy` guest of Windows 2000 after the install, log
  on as a member of Users only and run
  `copy /y %SystemRoot%\system32\cmd.exe C:\vmlab\vmlab-agent-legacy.exe`.
  Assert that the copy is refused.
- Fixed by card #716. The Win32 agent has `--secure-dir <dir>`
  (`guest/agent-legacy/src/plat_win32.c`, `secure_dir`): it makes
  Administrators the owner of the directory and of every entry in it and
  sets a DACL with full control for Administrators and SYSTEM and no other
  entry, inheriting on the directory and protected on Windows 2000 and later.
  The SIDs are written out, so a localised group name does not matter.
  `install-nt.cmd` runs it from the ISO before the copy and again after it,
  and stops when it fails. On a FAT volume there is nothing to set: it warns
  and the install goes on. The proof in the repo is
  `the_nt_install_secures_its_directory_before_the_service_binary_lands` in
  `src/template/bootstrap.rs`, which reads the staged script. The call itself
  was run under Wine with the `windows-nt-x86` build: the entry for Everyone
  left the directory and the file in it. Wine derives a file's ACL from its
  Unix mode, so that run does not show the owner or inheritance. The test
  above on a Windows 2000 guest was not run.

### F4. A second connect while a DOS or TempleOS command runs cuts the first command's session after 22 s

- Where: `src/labd/machine.rs:1297` (`dial` calls `check_cached` with
  `keep_slow = false`), `1549-1575` (`cached_verdict`: 2 s, then
  `PING_GRACE` 20 s, then `Gone`), `guest/agent-legacy/src/plat_dos.c:8-14`
  and `guest/agent-templeos/VmlabAgt.HC:13-16` (the agent answers nothing
  while a command runs).
- Fault: both agents run a command synchronously and answer no ping until it
  ends. Their files say "the host sees latency, never a lost agent". The
  host's rule says otherwise: any verb that connects while the command runs
  pings the cached connection, waits 22 s, and shuts the connection down.
  The running exec loses its session, its output and its exit code. The new
  handshake then fails too, because the guest stays silent, and the
  connection that was cut can leave a half frame behind (F6).
- Severity: wrong result.
- Confidence: high for the host path, by reading.
- Test: `a_connect_keeps_a_legacy_connection_that_runs_a_command` in the
  tests of `src/labd/machine.rs`, with `mock_agent` extended to answer the
  hello as `agent-legacy=test` and to hold every pong back. Open an exec
  session on the slot's handle, call `connect` again from a second task with
  a short grace (`with_grace`), and assert that the first handle is still
  open and that the mock accepted one connection.

### F5. On DOS `opened` is sent after the command has ended, so a command longer than 60 s is reported as not started

- Where: `guest/agent-legacy/src/plat_dos.c:227-238` (`proc_start` runs the
  child to its end), `guest/agent-legacy/src/agent.c:428-439` (`opened` is
  sent after `proc_start` returns), `src/labd/vm_agent.rs:307` and
  `873-896` (`LEGACY_OPEN_TIMEOUT`, then `Close` and "agent did not open the
  channel in time").
- Fault: the host waits 60 s for `opened`. The DOS agent sends it only when
  the command is over. A command that runs 61 s has run to its end in the
  guest with all its effects, and the host reports a failure, forgets the
  session, and drops the output and the exit code that arrive later. A
  caller that retries runs the command twice. The TempleOS agent sends
  `opened` before it runs the command (`VmlabAgt.HC:557`) and has no such
  fault.
- Severity: wrong result.
- Confidence: high.
- Test: `a_legacy_exec_that_outlasts_the_open_timeout_still_returns_its_output`
  beside `legacy_open_survives_slow_uart_but_modern_open_stays_bounded` in
  `src/labd/vm_agent.rs`, with the paused test clock. The mock answers the
  hello as `agent-legacy=test` and answers `open_exec` after 61 s of test
  time with `opened`, one DATA frame, `eof` and `exited` code 0. Assert that
  `exec` returns that output and code 0.

### F6. A host frame that is cut short makes the agent deaf for up to 64 KiB of later bytes, hellos included (fixed, card #722)

- Where: `guest/agent-legacy/src/wire.c:93-94` (`wire_next` waits for the
  whole claimed payload), `guest/agent-legacy/src/agent.c:473-480` (a hello
  can only be seen as a whole frame), `guest/agent-templeos/VmlabAgt.HC:89`
  (the same wait, byte by byte).
- Fault: a serial line gives the agent no sign that the host hung up. When
  the host cuts a connection while a frame is in transit, the hypervisor
  drops the unread rest (`hv/.../serial/socket.rs:48-55`), and the decoder
  keeps the header and waits for the claimed length. Every byte of the next
  connection is taken as payload of the old frame. The host's hello is about
  65 bytes, and the host sends one hello per handshake and backs off 30 s
  after a failure (`machine.rs:1305-1314`). How long the agent stays deaf
  depends on the frame that was cut. Today the host sends control frames
  only (no caller sends stdin), so the frame is an `open_exec` of the size
  of its argument vector: a few hellos for a short command, up to about
  250 hellos, which is hours, for one near the agent's 16 KiB string pool.
  A stdin frame may claim 64 KiB (`SendCredit::take` caps at `MAX_PAYLOAD`)
  and takes about 6 seconds to cross a 115200 baud UART. The first caller
  that sends stdin to a legacy guest makes a cut frame the usual result of
  a cut connection, and the agent then stays unreachable until the guest
  reboots. The cuts that exist today: `drop` at an
  online snapshot restore, a `Gone` verdict (F4), a restart of the lab
  daemon. An online snapshot taken while a frame is in transit restores the
  half frame with the guest. `wire.h:6-8` says the decoder recovers from
  this case "without a reset". It does not, for a frame cut short.
- Severity: wrong result (the machine reports no agent until the claimed
  length is filled or the guest reboots).
- Confidence: high. Proved on the POSIX build: after a header that claims
  60000 bytes with 100 bytes behind it, 923 hello frames were sent before
  one was answered.
- Test: `legacy_hello_after_a_truncated_frame_is_answered` in
  `src/labd/legacy_agent_tests.rs`. On one raw connection to the fixture,
  write a DATA header with length 60000 and 100 payload bytes, then one
  hello, then one ping. Assert that the agent's hello arrives. Today neither
  reply arrives. The same test with a mock for the TempleOS agent is not
  possible; there the steps are the same on a guest.
- Fixed by card #722. Both decoders look inside a frame that is still open
  for a whole control frame on channel 0, of at most 1024 payload bytes,
  whose message is a hello. On one they drop what came before it and answer
  it. A hello starts every connection, so it is the one message that says
  the host connected again. In the C agent this is `restart_inside` in
  `wire.c`, with `is_hello` of `agent.c` as its test, and each byte is
  looked at once. In `VmlabAgt.HC` it is in `VaReadFrame`. The C agent also
  steps past the magic only, not past the whole frame, when a control
  payload does not parse (`wire_reject`): that is the cut frame whose
  missing rest is shorter than a hello, which the hello's first bytes
  complete. Proof tests in `src/labd/legacy_agent_tests.rs`:
  `legacy_hello_after_a_truncated_frame_is_answered` (the steps above, then
  filler up to the claimed length and a second ping, so that the order of
  the replies decides and no time limit does) and
  `legacy_hello_that_straddles_the_end_of_a_truncated_frame_is_answered`.
  Both failed before the fix: the agent answered the last ping first. What
  stays open:
  - The TempleOS change was not run on a guest. No TempleOS guest was
    started for this card, and no test in the repo compiles HolyC.
  - A cut DATA frame that lacks fewer bytes than a hello has is completed
    by the hello's first bytes when the two arrive in separate reads. That
    hello is lost, and the next one is answered. The TempleOS agent has
    this for a cut control frame too.
  - Payload bytes that hold a complete, valid hello frame are taken for a
    new connection. No caller sends stdin to a legacy guest today.

### F7. `VmlabAgentInstall` on a TempleOS guest that already runs the agent starts a second agent on the same UART

- Where: `guest/agent-templeos/VmlabAgt.HC:665-668` (`VmlabAgentStart`
  spawns a task each time), `673-692` (the install guards the write of
  `MakeHome.HC.Z` with `StrFind`, but calls `VmlabAgentStart` in every
  case), `src/agent_asset.rs:199-201` (the typed script always ends with the
  include and `VmlabAgentInstall`).
- Fault: the comment at `670-672` says a rebuild that already registered the
  agent changes nothing. It does start a second task. Both tasks poll the
  same receive register, so each takes part of every frame. The include
  also defines the globals again (`va_pay`, `va_out`, `va_cap`, `va_task`),
  and each task allocates and stores its own buffers in them, so the capture
  hook claims output for the newer task only. No command is answered
  reliably until the guest reboots. A template built on a template that
  holds the agent reaches this.
- Severity: wrong result.
- Confidence: high for the second task, by reading. Not run.
- Test: on a `templeos` guest whose agent answers, type
  `VmlabAgentInstall;` at the shell once more, then run
  `vmlab exec <vm> -- '"ok\n";'` ten times. Assert ten answers of `ok`.

### F8. `close` and `hello` kill only the direct child of an exec; what it started keeps running

- Where: `guest/agent-legacy/src/plat_posix.c:379-383` (`kill` of one pid,
  and the child is not made a group leader at `216-233`),
  `guest/agent-legacy/src/plat_win32.c:664-668` (`TerminateProcess` of one
  process), reached from `agent.c:280-287` (`drop_session`) for `close`, for
  every `hello` (`479`) and for a port failure.
- Fault: `cmd /c tool.exe` or `sh -c 'tool & wait'` leaves `tool` running
  after the host closed the channel, with nothing on the host that knows of
  it. Each new connection sends a hello, so every reconnect leaves the
  children of all running commands behind (F4 makes reconnects common). On
  NT the survivors run as SYSTEM.
- Severity: wrong result.
- Confidence: high. Proved on the POSIX build: after `open_exec` of
  `sh -c 'sleep 31337 & wait'` and `close`, the `sleep` was still running.
- Test: `legacy_close_ends_what_the_exec_started` in
  `src/labd/legacy_agent_tests.rs`. A background child waits for EOF on the
  exec's stdin. A second exec reads the child's FIFO and must see EOF
  without the marker the child writes if it survives the close.
- Fixed by card #723. Cancellation signals the exec's process group on
  POSIX and terminates its job on Windows with job-object support. NT4
  and Windows 9x retain direct-child cancellation because they lack jobs.

### F9. A `shutdown` request ends the agent even when the guest does not shut down

- Where: `guest/agent-legacy/src/agent.c:506-510` (the ack goes out first,
  and a `plat_shutdown` that returns 0 ends the loop),
  `guest/agent-legacy/src/plat_posix.c:471-478` (returns 0 after `fork`,
  whatever `/sbin/shutdown` then does), `530-533` (`main` returns),
  `guest/agent-legacy/src/plat_win32.c:950-953` (`serve` returns),
  `guest/agent-legacy/src/plat_dos.c:413-417` (returns 0 after the reset
  pulse) and `480-481`.
- Fault: "initiated" is taken as "will happen". Where the shutdown program
  is missing, takes no `-H` (old sysvinit), is refused, or is aborted in the
  guest, the machine stays up and its agent has exited. On DOS the agent
  returns to the prompt. Nothing brings it back before a reboot, and the
  host was told `shutting_down`. A failed `reboot` leaves a running machine
  that reports no agent.
- Severity: wrong result.
- Confidence: high for POSIX and DOS by reading, medium for Win32, where it
  needs a shutdown that the guest aborts.
- Test: `legacy_agent_survives_a_shutdown_that_did_not_happen` in
  `src/labd/legacy_agent_tests.rs`, with `Fixture::process` (bubblewrap, no
  capabilities, so `/sbin/shutdown` cannot act). Send
  `shutdown_guest(Reboot)`, then `ping`. Assert a pong. Today the agent has
  exited and the connection is closed.

### F10. On DOS an environment override deletes the variable for every later command

- Where: `guest/agent-legacy/src/plat_dos.c:207-208` (`putenv` into the
  agent's own environment), `249-263` (after the command, `NAME=` is set,
  which removes the variable).
- Fault: the value the variable had before the exec is not kept. After one
  exec with `PATH` in its `env`, the agent has no `PATH`, and every later
  exec of a program by name fails. After one with `COMSPEC`, the
  `COMMAND.COM` fallback (`system`, line 234) has no shell to start. A name
  of 255 characters or more is not removed at all (`256`). The agent stays
  in this state until the guest reboots.
- Severity: wrong result. Wire only today: no host caller sends `env`.
- Confidence: high for the missing restore. Medium for what OpenWatcom's
  `putenv("NAME=")` does, which the comment at `253` states and which was
  not run.
- Test: a check `an exec's env does not outlive it` in
  `src/labd/agent_conformance.rs`, run against the agent socket of a
  FreeDOS guest: exec `SET`, exec `VER` with `env` `PATH=C:\`, exec `SET`
  again. Assert that the first and the third output are equal.

### F11. A NUL in an argument or an environment key changes the command without an error

- Where: `guest/agent-legacy/src/json.c:287` (`\u0000` is stored as a NUL
  byte), `guest/agent-legacy/src/agent.c:357` (the argument ends at the
  NUL), `388-392` (the `=` is written at the first NUL of the key, and the
  value is decoded after the key's real end).
- Fault: the argument `one\u0000two` reaches the program as `one`. The
  environment entry `["A\u0000B", "value"]` becomes `A=B` and the value is
  lost. No error is sent. The argument case needs a caller that can put a
  NUL into a string (a wscript string, not checked). The environment case
  is wire only today.
- Severity: wrong result.
- Confidence: high. Proved on the POSIX build: the child printed `[B]` for
  `$A` and `one` for the argument.
- Test: `legacy_exec_refuses_a_nul_in_argv_and_env` in
  `src/labd/legacy_agent_tests.rs`. Open an exec with such an argument, and
  one with such a key. Assert an error reply for each.

### F12. The TempleOS agent cuts captured output at 65535 bytes without a sign

- Where: `guest/agent-templeos/VmlabAgt.HC:402-409` (`VaCapPut` drops what
  does not fit), `573-583` (the exit code is not changed).
- Fault: output past the buffer is dropped, the exec ends with its normal
  exit code, and nothing tells the host that the output is short.
- Severity: wrong result.
- Confidence: high.
- Test: on a `templeos` guest,
  `vmlab exec <vm> -- 'I64 i; for (i=0;i<70000;i++) "a";'`. Assert 70000
  bytes of output, or an error that names the limit.

### F13. On old Linux a `cwd` that does not exist, or a missing program, gives `opened` and exit 127 with no text

- Where: `guest/agent-legacy/src/plat_posix.c:229-232`.
- Fault: the child exits 127 for a failed `chdir` and for a failed `execvp`
  and writes nothing. The host has sent `opened` already and cannot tell
  either case from a program that ran and returned 127.
- Severity: cosmetic.
- Confidence: high. Proved on the POSIX build for both cases: `opened`,
  `eof`, `exited` with code 127, no stderr byte.
- Test: `legacy_exec_names_a_missing_cwd` in
  `src/labd/legacy_agent_tests.rs`: exec `pwd` with `cwd`
  `/nonexistent-dir`, assert stderr or an error reply that names the
  directory.

## Notes

These have no test that proves a fault, or they are design limits that the
code states. They are listed so that the lead can decide.

- N1. The idle announcement that #678 relies on is counted in sleeps, not in
  time (`agent.c:662-673`: 200 sleeps of 5 ms). Where a 5 ms sleep takes one
  timer tick of 10 to 15.6 ms (NT, 2000, XP), the period is 2 to 3 s. A
  probe waits 2 s for the guest's first byte (`machine.rs`, `probe`), on a
  fresh connection each time, and the hypervisor drops guest bytes while no
  client is connected (`serial/socket.rs:84-87`). So a probe of an idle NT
  guest can miss. Only a measurement on a guest proves it.
- N2. `guest/build-agent-legacy.sh` links the DOS build with
  `wlink system dos32a` and no `option stack`. With N8 of card #438 (the
  JSON parser recurses up to 256 levels) the question there stays open:
  which stack the DOS build has was not found out. The host sends no deep
  message.
- N3. The `COMMAND.COM` fallback on DOS (`plat_dos.c:147-170`, `232-237`):
  the exit code is that of `system`, which for MS-DOS `COMMAND.COM` is 0
  also for "Bad command or file name". Only spaces are quoted, so `>`, `<`
  and `|` in an argument redirect, and a `"` is not escaped. The tail is
  checked against 1024 bytes, and DOS takes 126. What OpenWatcom's `system`
  does with a longer one was not checked.
- N4. `reclaim_port_from_serial_mouse` (`plat_win32.c:855-885`, called at
  `944`) runs when the port open fails with "access denied" for any reason,
  also a second agent instance or another program on COM1. It deletes every
  serial mouse Setup detected, on whatever port, and restarts Windows with
  `EWX_FORCE`. It does so once per installation.
- N5. `stop_enumerating_port` (`plat_win32.c:910-922`) reads `PortName` and
  `Driver` without a check of the type or of the terminating NUL and then
  uses them as C strings. The data is the guest's own registry.
- N6. Any `mode` that is not `reboot` powers the guest off
  (`plat_dos.c:411-420`, `plat_win32.c:782-802`, `VmlabAgt.HC:625-627`), and
  the agent echoes the string it got in `shutting_down`. The host sends
  three fixed modes.
- N7. `INSTALL.BAT` (line 22-23) relies on the exit code of `FIND`. MS-DOS
  before version 6 may not set it. The line would then never be added to
  `AUTOEXEC.BAT`. Not checked on such a guest.
- N8. `install_9x` (`plat_win32.c:1099-1106`) starts a new agent in every
  case. Where one runs already, the new one cannot open the port and tries
  again every 2 s without end (`942-947`).
- N9. The NT service stop handler (`plat_win32.c:977-985`) closes the port
  from the SCM's thread while the loop uses it, and exits the process
  without `drop_all_sessions`. Running commands stay behind (see F8).
- N10. On ISA serial any byte from the guest counts as the guest speaking
  (`vm_agent.rs:2765`, `616-635`), and the host then sends its hello. A
  guest system that writes to COM1 for its own reasons, with no agent, gets
  the hello that #678 holds back from serial mouse detection. No guest that
  does both was found.
- N11. `VaStr` in the TempleOS agent (`VmlabAgt.HC:267-274`) turns `\b`,
  `\f` into the letters `b`, `f`, and every `\u` escape into `?`, also one
  for an ASCII character. serde_json writes `\b`, `\f` and `\u00XX` for
  control characters only.
- N12. The TempleOS agent says the power-off port is that of PIIX4 on
  i440fx (`VmlabAgt.HC:626`), and the profile text says i440fx. x86 guests
  run on vmlab-hv's q35 now. The port is right there too
  (`hv/crates/vmlab-hv-vmm/src/pm.rs:18`). Only the words are old.
- N13. The agent socket of vmlab-hv is bound with the process umask and
  serves the first client that connects (`serial/socket.rs:19-37`). On the
  legacy tier that client can run commands as SYSTEM, or in ring 0 on
  TempleOS, with no check of the peer. Whether another local user can reach
  it depends on the mode of the run directory, which was not checked (the
  same open point as N6 of card #438).
- N14. A hello kills every running command (`agent.c:479`). Proved on the
  POSIX build. This is the design (both sides drop channel state). It is
  listed because it is what makes F4 and F8 costly.
- N15. The DOS power-off and reset (`plat_dos.c:385-420`) flush no disk
  cache first. A guest with a write-behind cache (SMARTDRV) could lose
  writes. Not checked.
- N16. `VaJsonParse` (`VmlabAgt.HC:222-227`) accepts bytes after the JSON
  value. `json_parse` refuses them (`json.c:163`).
- N17. `proc_free` on Win32 waits up to 2 s for the stdin writer
  (`plat_win32.c:685`). The loop serves nothing in that time. It happens
  only where a grandchild holds the stdin pipe and does not read.
- N18. On DOS a spawn that fails for another reason than "not found" gives
  exit 127 and no text (`plat_dos.c:232-237`, `265`).
- N19. `guest/agent-legacy/README.md:10` says the agent advertises one
  feature. The hello carries two, `exec` and `set_time`
  (`agent.c:154`).
- N20. The ISA UART of vmlab-hv does not overrun: it takes host bytes only
  while the receive FIFO has room (`serial.rs:191-197`, `251-262`). So the
  polling agents lose no host byte while a command runs. The comment in
  `plat_dos.c:5-6` credits QEMU for this, which runs no x86 guest any more.
  No finding.
