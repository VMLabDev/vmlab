# Review: guest agents and the agent protocol

Card #438, 2026-11-22. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Paths are relative to the repo root. Line numbers are those of commit
`5cccddb6` (`next` at the start of the card).

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. "Confidence" says how sure the reading is.

Two directions were read, as the card asks:

- What a hostile guest agent can send the host: frame lengths, record
  lengths, names, sizes, encodings and floods.
- What a host request can make the guest agent do outside its intent.

Severity words are those of the card: data loss, crash of the VM or host
process, security against a hostile guest or peer, wrong result, cosmetic.

PRD §1.2 says vmlab is not a security boundary between the host user and the
lab file. The findings below marked "security" are about a different line: a
guest that turned hostile after it was started, and another local user of the
host.

## What was read and what was not

Read in full, without the test modules:

- `guest/agent-proto/src/lib.rs`, `guest/agent-proto/src/fileops.rs`
- `guest/cinit-proto/src/lib.rs`
- `guest/agent/src/mux.rs`, `exec.rs`, `terminal.rs`, `tail.rs`,
  `metrics.rs`, `fileops.rs`, `unix/in_root.rs`
- `guest/agent-legacy/src/agent.c`, `wire.c`, `wire.h`, `json.c`, `json.h`,
  `plat.h`
- `src/labd/container_ctl.rs`, `src/labd/one_shot.rs`
- `src/labd/vm_agent.rs` lines 86-1000, 1179-2340 and 2340-2680 (the
  connection, the reader, sessions, `exec`, the file session, the watch
  session)

Read in part:

- `guest/agent-legacy/src/plat_win32.c` lines 36-106 and 205-700 (text
  conversion, argument quoting, the environment block, pipes, the stdin
  writer)
- `guest/agent/src/unix/login.rs` lines 418-490 (the `su` script)
- `guest/agent/src/unix/mod.rs` (the path resolver and `app_home` only)
- `guest/cinit/src/ctl.rs` lines 1-200
- `src/labd/mod.rs` lines 449-477 and 760-1045 (the exec, shell, cp, tail
  and event-log verbs)
- `src/cli/lab.rs` lines 2040-2145 (`vmlab cp`)
- `src/labd/machine.rs` lines 1478-1520 (the ping verdict)
- `guest/agent-proto/src/watch.rs` (the constants only)

Not read:

- `guest/agent/src/spawn.rs`, `logon.rs`, `app.rs`, `clipboard.rs`,
  `rdp.rs`, `rtc.rs`, `svc.rs`, `darwin.rs`, `main.rs`
- `guest/agent/src/watch/` (the watch backends and the dirty set)
- `guest/agent/src/windows/` (ConPTY, logon tokens, the port adapter, the
  event log, the clipboard, RDP)
- `guest/agent/src/unix/` apart from the parts named above: `linux.rs`,
  `macos.rs`, `container.rs`, `shepherd.rs` and most of `mod.rs` and
  `login.rs`. So the PTY code, `setns`, the `setuid` route and the process
  trees were not checked.
- `guest/agent-proto/src/app.rs` and the body of `watch.rs`
- `guest/cinit/src/` apart from `ctl.rs`: mounts, users, networking, the
  container start, health checks
- `guest/agent-legacy/src/plat_dos.c`, `plat_posix.c`, and the service,
  install and shutdown parts of `plat_win32.c`
- `guest/agent-templeos/VmlabAgt.HC`
- `src/labd/app_share.rs`, `agent_repair.rs`, `boot_clock.rs`, `boot_id.rs`,
  `identity.rs`, `agent_conformance.rs`, `legacy_agent_tests.rs`
- `src/labd/vm_agent.rs` lines 1000-1178 (clipboard, time, metrics calls)
- `src/appshare/`, `src/cli/tty_attach.rs`, `src/cli/app.rs`,
  `src/scripting/` (only the call sites were looked at)
- `src/labd/workspace/`: the syncer has its own review
  (`docs/reviews/labd-sync.md`).
- every test module, apart from the names of fixtures and tests

## Findings, worst first

### F1. A pull truncates the host destination before the guest file is known to exist

- Where: `src/labd/vm_agent.rs:918-937` (`pull_file_as`). The callers are
  `machine.pull_file` at `src/labd/mod.rs:995`, which `vmlab cp <vm>:<path>
  <dest>` uses (`src/cli/lab.rs:2118-2145`), and wscript's pull at
  `src/scripting/mod.rs:1211`.
- Fault: `File::create(local)` at line 929 runs first. It empties an existing
  file. Only after that does the function check the `fileops` feature, open
  the session, `stat` the guest path and open it. Every failure after line
  929 leaves the host file empty or partly written: a guest path that does
  not exist, a directory, an agent without `fileops`, a login that cannot be
  minted, a dropped connection, a digest mismatch.
- Severity: data loss on the host. `vmlab cp vm:/etc/typo ./notes.txt`
  destroys `notes.txt` and then reports that the guest file is missing.
- Confidence: high.
- Test: `a_failed_pull_leaves_the_host_file_as_it_was` in
  `src/labd/vm_agent.rs`. Write `keep me` to a temp file. Call `pull_file`
  against the mock agent for a path that the mock file system does not hold.
  Assert the error, and assert that the file still reads `keep me`. Repeat
  with the mock agent of
  `a_transfer_against_an_agent_without_the_feature_is_refused`.
- Fixed in card #463. `pull_file_as` opens the file session first, pulls
  into a new file beside the destination and renames it over the destination
  only after the digest matched. A failed pull removes that file. An existing
  destination keeps its mode and is written through a symlink. A destination
  that exists and is not a regular file (a device, a FIFO) is written
  directly, as before. The test failed before the fix (the file read empty)
  and passes after it. Not fixed here: `FileOps::pull_to`, the syncer's pull,
  still creates its destination first. The syncer gives it a temp path.

### F2. An inline push writes its bytes to one predictable file in the shared temp directory

- Where: `src/labd/mod.rs:962-967`.
- Fault: the daemon writes the pushed bytes to
  `<temp_dir>/vmlab-cp-<daemon pid>-<machine>` with `std::fs::write`, then
  pushes that file and removes it. Three faults follow from the name and the
  call:
  - The name is the same for every inline push to one machine. Two pushes
    that run at the same time share the file. One overwrites the bytes of the
    other, or removes the file while the other still reads it. The result is
    a digest mismatch, an open error, or the wrong content in the guest when
    both writes finish before either push reads.
  - `std::fs::write` follows a symlink and does not use `O_EXCL`. On a host
    with other users, a user who creates a symlink with that name in `/tmp`
    makes the daemon overwrite the file the link points to with the pushed
    bytes.
  - The file is created with the default mode. Other local users can read the
    pushed bytes while the push runs.
- Severity: security against a hostile local peer (file overwrite as the
  daemon user, disclosure of pushed content), and wrong result for concurrent
  pushes.
- Confidence: high for the shared name and the symlink. The window for the
  wrong content is small.
- Test: `inline_pushes_to_one_machine_do_not_share_a_temp_file`. Send two
  `machine.push_file` requests with different `data` and different `to` to
  one machine at the same time, with a mock agent that delays its first
  `open` reply. Assert that both succeed and that each guest path holds its
  own bytes. Second test: create a symlink at the temp name that points to a
  file with known content, run one inline push, and assert that the target
  file did not change.
- Fixed in card #469. An inline push no longer writes a host file: the
  daemon sends the decoded bytes from memory over the file session
  (`AgentHandle::push_bytes_as`), makes missing guest directories as a push
  from a file does, and verifies the guest's digest against the bytes. The
  handler's inline branch is the function `push_inline` in `src/labd/mod.rs`,
  and the two tests in `src/labd/vm_agent.rs` call it against the mock
  agent: `inline_pushes_to_one_machine_do_not_share_a_temp_file` and
  `an_inline_push_does_not_write_through_a_planted_symlink`. Both failed
  before the fix (the first push sent the bytes of the second, and the
  symlink target held the pushed bytes) and pass after it.

### F3. A collected exec holds all guest output in host memory without a limit

- Where: `src/labd/vm_agent.rs:801-840`, lines 824-825. Callers:
  `machine.exec` at `src/labd/mod.rs:766-794`, and the internal probes
  (`boot_id.rs:170`, `app_share.rs:235`, `agent_repair.rs:323`,
  `playbook.rs`, `lab.rs:234`).
- Fault: `out.extend(b)` and `err.extend(b)` have no ceiling. The loop grants
  the guest new credit as fast as it reads, so the guest can send for the
  whole timeout. `machine.exec` then copies the output again into a JSON
  reply. A guest that turned hostile answers any internal probe (the boot
  identity read, for example) with an endless stream. A user command such as
  `vmlab exec vm cat /dev/zero` does the same by accident.
- Severity: crash of the host process (the lab daemon runs out of memory and
  takes every machine of the lab with it). Security against a hostile guest.
- Confidence: high.
- Test: `a_collected_exec_refuses_output_past_its_ceiling`. A mock agent
  answers `open_exec` with `opened` and then sends 64 KiB `Data` frames
  without end. Assert that `exec` returns an error that names the limit
  after a bounded number of bytes, and does not run to the timeout.
- Fixed in card #483. `AgentHandle::exec` holds at most `EXEC_OUTPUT_LIMIT`
  (64 MiB, stdout and stderr together). The event that would pass it makes
  the exec an error that names the limit, and the session is dropped, which
  closes the channel and ends the command in the guest. The limit covers
  every caller, `machine.exec` and the internal probes. The test
  `a_collected_exec_refuses_output_past_its_ceiling` in
  `src/labd/vm_agent.rs` runs a mock exec that sends 128 MiB within the
  credit the host grants and never exits. It failed before the fix (the exec
  ran to its 20 s timeout with all of it held) and passes after it. Not fixed
  here: `machine.exec` still copies the collected output into its JSON reply,
  so one exec can hold a small multiple of the limit for a moment.

### F4. The host does not enforce the receive window, so one channel can queue 2048 full frames

- Where: `src/labd/vm_agent.rs:155-158` (`SESSION_QUEUE`), `2518-2527`
  (`deliver`), `1191-1203` (`recv`).
- Fault: the protocol gives the guest 256 KiB of credit per channel
  (`INITIAL_WINDOW`). The host counts what it consumed (`RecvWindow`) but
  never checks what arrived against what it granted. The only bound is the
  queue depth of 2048 events. A frame holds up to 64 KiB, so a guest that
  ignores its credit puts about 128 MiB into the queue of one channel whose
  consumer is slow. It can do that on every open channel: the watch, each
  file session, each tail, each terminal.
- Severity: crash of the host process by memory use. Security against a
  hostile guest.
- Confidence: high for the missing check. The total depends on how many
  channels are open.
- Test: `a_guest_that_ignores_its_credit_loses_the_channel`. A mock agent
  answers an open and sends 300 KiB on that channel while the test does not
  call `recv`. Assert that the session ends with an error that names the
  window, and that no more than `INITIAL_WINDOW` bytes sit in the queue.
- Fixed in card #484. The host now counts the credit it granted on each
  channel (`RecvCredit` in `src/labd/vm_agent.rs`): the reader takes from it
  for every data frame, and the consumer adds each `window_adjust` before it
  sends it. A frame past the credit is dropped, the reader closes that one
  channel and tells the agent, and the consumer reads an error that names
  the window after what was queued. The connection and the other channels
  stay. The queue of one channel therefore holds at most `INITIAL_WINDOW`
  bytes. The test `a_guest_that_ignores_its_credit_loses_the_channel` failed
  before the fix (the host kept the channel) and passes after it. The mock
  agent of the tests in that file now sends its data under the credit too.
  Not fixed here: the 10 s wait of F5 (fixed in card #506), and a queue of
  2048 small frames inside the window.

### F5. One channel with a slow consumer stops the host reader for 10 seconds

- Where: `src/labd/vm_agent.rs:2518-2537` (`deliver`), called from the one
  reader task at `2370-2383`. The ping verdict is at
  `src/labd/machine.rs:1490-1503`.
- Fault: `deliver` waits up to `STALL_TIMEOUT` (10 s) for room in the queue
  of one session. During that wait the reader task reads no frame for any
  other channel and no control message, so no `pong`, no `opened` and no
  `window_adjust` is seen. A ping with its 2 s budget fails, and an open
  with its 15 s budget can fail. After the wait, `deliver` takes the writer
  lock and writes a `close` frame with no time limit (lines 2533-2535). When
  the guest does not read, the reader task stays there until the connection
  is shut down. A hostile guest needs F4 only to fill one queue. An honest
  guest triggers it when a consumer on the host stops reading: for example
  `expose_terminal_socket` (lines 2562-2586) waits inside `session.send` for
  guest credit and does not read terminal output during that wait.
- Severity: wrong result (every session and the liveness check of one
  machine stall, and sessions can be cut). Security against a hostile guest
  as a stall of that machine's agent client.
- Confidence: high for the 10 s stall. Medium for the terminal case.
- Test: `a_full_session_queue_does_not_delay_a_pong`. A mock agent opens two
  channels, sends more than `SESSION_QUEUE` frames on the first while the
  test does not read it, and answers pings. Assert that `ping(2 s)` returns
  true while the first channel is full.
- Fixed in card #506. The reader task no longer waits on a session. The
  queue of one channel is now `SessionQueue` in `src/labd/vm_agent.rs`, and
  `deliver` puts an event into it without a wait. Up to `SESSION_QUEUE`
  events each frame is its own event. At that depth the payload of a frame
  joins the last queued event of the same stream (stdout or stderr), which
  the receive window of F4 bounds in bytes, so a consumer that is behind
  loses nothing: every consumer reads a channel as a byte stream. An event
  that cannot join (stdout and stderr in turn past the depth, for example)
  closes that one channel at once, and its consumer reads an error that
  names the queue after what was queued. One `Eof` and the event that ends
  a session are always taken, so a command that ends while its consumer is
  behind keeps its exit code. The `close` frame the reader sends for a channel it ended is
  written by a task of its own with a 10 s limit on the writer lock and the
  write, so a guest that does not read cannot hold the reader there. A write
  cut off inside the frame marks the connection as over, as a ping does. The
  tests `a_full_session_queue_does_not_delay_a_pong` and
  `a_channel_that_overfills_its_queue_is_closed_without_a_wait` failed
  before the fix (the ping got no pong in 2 s) and pass after it. Not fixed
  here: `expose_terminal_socket` still does not read terminal output while
  it waits for guest credit, which now costs only that terminal. The bounded
  `close` write has no test of its own.

### F6. A full input queue of one session blocks the whole guest agent

- Where: `guest/agent/src/mux.rs:494-504` (`route_input`), queue depth at
  line 197. The file session adds a second blocking stage at
  `guest/agent/src/fileops.rs:121` (`WORK_QUEUE` of 64).
- Fault: `route_input` runs on the agent's one reader thread and calls the
  blocking `tx.send(input)`. The queue holds 512 entries. The comment at
  lines 194-196 says the queue never fills while the host respects the
  window. That holds only for frames of 512 bytes or more: the window counts
  bytes and the queue counts frames. A host that sends small frames within
  its 256 KiB of credit fills the queue when the session's pump does not
  drain it:
  - A terminal whose foreground program does not read its input. Each
    keystroke is one frame (`expose_terminal_socket` forwards each read).
    After the PTY buffer is full, 512 more keystrokes block the reader.
  - A file session with four slow requests in the workers (four digests of
    large files) and more than 64 + 512 small requests behind them.
  While the reader thread is blocked the agent serves nothing: no ping, no
  other channel, no `close` for the stuck channel. The host then counts the
  agent as silent.
- Severity: wrong result. One stuck program in one terminal makes the agent
  of the machine deaf until that program reads or dies.
- Confidence: high for the block. Medium for how often a real session
  reaches 512 small frames.
- Test: `a_session_that_stops_reading_does_not_block_other_channels` in
  `guest/agent/src/mux.rs`. Register channel 9 and never read its input.
  From a second thread call `route_input(9, …)` 600 times with one byte
  each, then `handle_msg(Ping)`. Assert that the pong arrives within one
  second.
- Fixed in card #507. A session's input queue is now bounded by the bytes
  it holds, the unit the window counts in, and `route_input` never waits on
  it. Small frames are joined in the queue, so a full window of one-byte
  frames costs a window of memory. A host that respects its window cannot
  fill the queue. A host that sends past it loses that channel with an
  error, which is what the host does to a guest in the other direction
  (F4, card #484). The test failed before the fix (the reader blocked at
  frame 513 and no pong came within one second) and passes after it. The
  second stage in `fileops.rs` (`WORK_QUEUE`) still blocks, but only the
  session's own `read_loop` thread: its unread bytes wait in the input
  queue and the reader thread goes on.

### F7. A pull trusts the size in the guest's `stat`, so a guest can fill the host disk

- Where: `src/labd/vm_agent.rs:1561-1647`, lines 1584 and 1594.
- Fault: the comment at line 1582 says a dishonest guest must not turn a pull
  into an endless disk write. The limit is `attrs.size` plus one chunk, and
  `attrs.size` comes from the same guest. A guest that answers `stat` with a
  size of `u64::MAX` and every read with a full chunk makes `pull_to` and
  `pull_file_as` write until the disk is full. The existing test
  `hostile_pull_cannot_write_past_stat_allowance` covers only a guest whose
  `stat` is honest. Each record also renews the watchdog, so no timeout ends
  the pull.
- Severity: security against a hostile guest (the host disk fills). The
  inline form (`pull_bytes`) is safe: `MemorySink` has its own limit.
- Confidence: high.
- Test: `a_guest_cannot_choose_how_much_a_pull_writes`. The mock file system
  answers `stat` with `size: u64::MAX` and each `read` with 64 KiB. Assert
  that `pull_to` fails after a bounded number of bytes. The fix card must
  decide where the bound comes from (a caller's ceiling, or free space).
- Fixed in card #468. The bound is free space: a pull into a host file may
  write the free space of the destination's filesystem less a reserve (a
  twentieth of the filesystem, at most 1 GiB). The host measures it when the
  file is opened and again after every 64 MiB written. A `stat` size past
  that room is refused before the first read, and the sink refuses each
  chunk past it, so `pull_to` and `pull_file_as` both stop there. A
  destination that is not a regular file (a device, a FIFO) has no bound, as
  it fills no disk. The test failed before the fix (the pull wrote 16 MiB
  and ended only where the mock ends its file) and passes after it.

### F8. A file request on a FIFO or a device takes a guest worker thread for good

- Where: `guest/agent/src/fileops.rs:783-798` (`digest_file`), `524-550`
  (`open_file`), and the workers at `148-167`.
- Fault: `tail` refuses everything that is not a regular file
  (`tail.rs:open_regular`, code review F61c). The file session has no such
  check. `Op::Open` for reading and `Op::Digest` on a FIFO block in `open(2)`
  until a writer comes. `Op::Digest` on `/dev/zero` reads without end and
  uses one CPU. A worker has no cancel: closing the channel does not stop
  it. The host gives up after its watchdog time, but the thread stays for the
  life of the agent. A push to a device path ends in such a digest:
  `vmlab cp file vm:/dev/zero` opens, writes, then asks for the digest of
  `/dev/zero`. Four such requests on one session stop that session.
- Severity: wrong result in the guest (a leaked thread, and for a device a
  CPU that stays busy until the agent restarts). A host request makes the
  agent do work outside its intent.
- Confidence: high for the FIFO and `/dev/zero` paths on Linux. Windows
  device names were not checked.
- Test: `a_digest_of_a_fifo_is_refused` in
  `guest/agent/src/sessions_test.rs`. Make a FIFO with `mkfifo`, send
  `Op::Digest` and `Op::Open` for it, and assert an error reply within one
  second for each.
- Fixed in card #508. On Unix every open the file session makes for a path
  (`Op::Open`, `Op::Digest`, and the open of a size or time `Op::Setstat`)
  refuses a FIFO, a character device and a socket with "not a regular
  file". It checks the path first, opens with `O_NONBLOCK`, and checks the
  open handle again. A block device is finite and stays allowed, so
  `vmlab cp file vm:/dev/null` is now refused and a push to a disk device is
  not. The test also asks for `/dev/zero` and for more requests than the
  session has workers. It failed before the fix (the digest of the FIFO got
  no reply in 10 s) and passes after it. Not fixed here: Windows device
  names, which stay unchecked.

### F9. An exec does not report its exit while a background child holds its output pipe

- Status: fixed in card #526. The Rust agent gives both output pumps one
  two-second drain window after process exit and stops forwarding before
  sending EOF and the exit code. The legacy agent drains the bytes buffered
  at exit, regardless of whether a descendant keeps the pipes open.
- Where: `guest/agent/src/exec.rs:100-114` (the reaper joins both output
  pumps before it sends `exited`). The legacy agent has the same rule at
  `guest/agent-legacy/src/agent.c:590`.
- Fault: the reaper learns the exit code from `wait()` and then waits for
  the end of stdout and stderr. A process that the command started in the
  background keeps those pipes open, so `eof` and `exited` do not go out
  until that process ends too. `vmlab exec vm -- sh -c 'mydaemon &'` returns
  only at the timeout, with an error and without the exit code, although the
  command ended at once with code 0. The terminal path has a 2 s drain for
  the same case (`terminal.rs:DRAIN`). The exec path has none.
- Severity: wrong result. A provision step that starts a daemon fails on its
  timeout.
- Confidence: high.
- Tests: `an_exec_reports_its_exit_while_a_background_child_holds_the_pipe`
  in `guest/agent/src/sessions_test.rs` and
  `legacy_exec_reports_exit_with_inherited_output_pipes` in
  `src/labd/legacy_agent_tests.rs`. Assert stdout, stderr and exit code 3
  while a descendant still holds the pipes open.

### F10. An exec with stdin can deadlock, and the timeout does not cover it

- Status: fixed in card #509. Collected exec drains output while sending
  stdin under the same deadline, and the agent credits discarded input.
- Where: `src/labd/vm_agent.rs:811-815` and
  `guest/agent/src/exec.rs:66-78`.
- Fault: two faults on one path.
  - The host sends all of `stdin` before it reads any output, and the
    timeout wraps only the read loop (line 820). A command that writes more
    than its 256 KiB of credit before it has read all of its input (`cat`
    with 1 MiB of stdin) stops: the guest waits for output credit, the host
    waits for input credit, and `exec` never returns.
  - In the agent, after the child closed its stdin, the pump runs
    `continue` at line 70 and skips the credit grant for every later chunk.
    The host then waits for credit until the channel closes, and `exec`
    returns "agent channel closed" in place of the exit code.
- Severity: wrong result (a hang with no timeout, or a lost exit code). No
  caller in the tree passes stdin today except the conformance check with
  six bytes, so the fault is not reached yet.
- Confidence: high for both readings.
- Test: `a_large_stdin_through_cat_comes_back_whole` runs against a real
  agent with 1 MiB of stdin and a 30 s timeout. The command fills its output
  window on stderr before running `cat`. The test checks both streams.
  `stdin_after_the_child_closed_it_is_still_credited` keeps the child alive
  after it closes stdin, sends 600 KiB, and checks that `window_adjust`
  grants cover all of it.

### F11. The ctl reader ends for good on one line that is too long or not UTF-8

- Where: `src/labd/container_ctl.rs:77` and `178-196`.
- Fault: a line that is not JSON is skipped (line 83-87). A line of more than
  64 KiB, or a line with one byte that is not UTF-8, makes `read_ctl_line`
  return `Err`, and `while let Ok(Some(line))` ends the reader task. The
  watch senders drop, so every later `wait_started` and `wait_exited`
  reports "ctl channel closed", and no `net_up`, `health` or `exited` event
  of that container is seen again. The agent stream is built to survive
  bytes that an online restore replays or drops. This channel is not: one
  bad byte ends it. A container workload that can write the port can do the
  same.
- Severity: wrong result (the state of a container stays as it was for the
  life of the micro-VM).
- Confidence: high.
- Test: `a_line_that_is_not_utf8_is_skipped` in
  `src/labd/container_ctl.rs`. The mock writes `b"\xff\n"`, then a `started`
  event. Assert that `wait_started` succeeds. The same with a line of
  70 000 bytes before the event.
- Fixed in card #510. `read_ctl_line` no longer returns an error for a bad
  line. It reports the line as skipped, and the reader task warns and reads
  on, as it does for a line that is not JSON. A line past the 64 KiB cap is
  dropped through its newline without being held in memory. Only EOF and a
  socket error end the reader. The tests
  `a_line_that_is_not_utf8_is_skipped` and
  `a_line_that_is_too_long_is_skipped` failed before the fix ("ctl channel
  closed before the container started") and pass after it.

### F12. File names that are not UTF-8 are changed in a directory listing

- Where: `guest/agent/src/fileops.rs:405` (`ReadDir`), and line 465
  (`Readlink`).
- Fault: `to_string_lossy` replaces each byte that is not UTF-8 with
  U+FFFD. The host gets a name that does not exist in the guest. Two files
  whose names differ only in such bytes get the same name. Every later
  request for that name fails with "no such file", or acts on another file
  when one with the replaced name exists. A link target loses bytes in the
  same way, and the syncer copies link targets as text.
- Severity: wrong result. For the workspace syncer it can become data loss
  (a path that looks absent). That path was not followed in this review.
- Confidence: high for the listing. The effect on the syncer is not checked.
- Test: `a_name_that_is_not_utf8_is_refused_or_round_trips` in
  `sessions_test.rs` (Linux). Create a file named `b"a\xffb"`, list the
  directory, then `stat` the name from the listing. Assert that the listing
  reports an error for that entry, or that the `stat` succeeds.
- Fixed in card #811. `ReadDir` reports an error when an entry name cannot
  be represented as UTF-8. The directory handle keeps returning that error
  on retry. `Readlink` reports an error for targets with the same limitation.

### F13. `realpath` in a container answers a path of the micro-VM, not of the container

- Where: `guest/agent/src/fileops.rs:449-453` and `763-781`.
- Fault: the request path is first resolved inside the container root
  (`unix/in_root.rs`), which puts the rootfs in front. `realpath` then
  returns `canonicalize` of that. The host gets
  `<rootfs>/etc/hosts` where the container knows the file as `/etc/hosts`.
  A host that uses the answer in a later request gets the rootfs put in
  front a second time. The failure messages of every file operation name
  the micro-VM path for the same reason (`context(&path)` gets the resolved
  path).
- Severity: wrong result. No host code uses `Realpath` today apart from the
  conformance check, so only the messages are seen.
- Confidence: high.
- Test: in `guest/agent/src/unix/mod.rs`, next to the path resolver test at
  line 1453: open a file session on `container_platform`, send
  `Op::Realpath { path: "/srv/link" }`, and assert `/etc/sentinel`.
- Fixed in card #525. `realpath` in a session whose paths are resolved inside
  a root answers the resolved path without that root in front
  (`seen_from` in `guest/agent/src/fileops.rs`), so the answer is good for a
  later request. The test is
  `realpath_in_a_container_answers_a_container_path` in
  `guest/agent/src/unix/mod.rs`. It fails before the fix (the answer was
  `<tempdir>/rootfs/etc/sentinel`) and passes after it. Not fixed here: the
  failure messages of the file operations still name the micro-VM path.

### F14. A tail or event-log stream damages a UTF-8 character that crosses two frames

- Where: `src/labd/mod.rs:457-459` (`pump_session`).
- Fault: each `Data` frame is turned into text with `from_utf8_lossy` by
  itself. The agent cuts frames at byte counts (`tail.rs` reads 16 KiB), so
  a character of more than one byte can be split. Both halves become U+FFFD.
- Severity: wrong result (damaged text in `vmlab tail` output for logs that
  are not ASCII).
- Confidence: high.
- Test: `a_character_split_across_two_frames_survives`. A mock agent sends
  the two bytes of `é` in two `Data` frames on a tail channel. Assert that
  the streamed chunks join to `é`.

### F15. Any record on a file channel renews the watchdog, also one that answers nothing

- Where: `src/labd/vm_agent.rs:2176-2183` and `2026-2068`.
- Fault: the pump sets `last_reply` for every record that decodes, before it
  looks for a waiter. A record with an id that nobody waits for counts as
  progress. An agent that sends such a record once a minute keeps every call
  on the channel waiting without end: `vmlab cp` and the syncer hang.
- Severity: wrong result (a hang) against a hostile or faulty guest.
- Confidence: high.
- Test: `replies_to_nobody_do_not_renew_the_watchdog`. The mock answers no
  request and sends `{"id":999999,"reply":"ok"}` every 50 ms. With a stall
  time of 300 ms, assert that `stat` fails within one second.

### F16. A protocol version mismatch is reported as "no agent answered"

- Where: `src/labd/vm_agent.rs:2411-2417` and `392-400`. The same shape for
  cinit at `src/labd/container_ctl.rs:89-96`.
- Fault: a `hello` with another `proto_version` is logged and dropped. The
  handshake then runs to its timeout and fails with "no vmlab-agent answered
  on the agent channel". The user is told the agent is absent when it is
  present and too old or too new. For cinit the mismatch is only logged and
  the host continues to drive the init, although the crate documentation
  says the host refuses.
- Severity: cosmetic (a wrong message), and for cinit a check that does
  nothing.
- Confidence: high.
- Test: `a_version_mismatch_is_named_in_the_error`. The mock answers the
  hello with `proto_version: 1` and the right token. Assert that `connect`
  fails at once and that the error names both versions.

### F17. The legacy agent drops a channel id of 2^31 or more from its replies, and accepts id 0

- Where: `guest/agent-legacy/src/agent.c:78-103` (`send_simple`,
  `send_error` take `long id` and test `id >= 0`), `468` (a missing `id`
  reads as 0), `handle_open_exec`.
- Fault: on the 32-bit targets `(long)id` is negative for ids from 2^31, so
  `opened` and `error` go out without an id and the host cannot match them.
  An `open_exec` without an id, or with id 0, makes a session on the control
  channel number, which the Rust agent refuses (`mux.rs:353`). The host
  counts ids from 1, so neither case is reached today.
- Severity: cosmetic.
- Confidence: high.
- Test: in `src/labd/legacy_agent_tests.rs`, send `open_exec` with
  `id: 0` and assert an error reply. On a 32-bit build, send
  `id: 3000000000` and assert that the `opened` names that id.

## Notes

These have no test that proves a fault, or they are design limits that the
code states. They are listed so that the lead can decide.

- N1. The decoders remove each record from the front of a `Vec` with `drain`
  (`guest/agent-proto/src/lib.rs:329`, `fileops.rs:391`, `watch.rs:166`,
  `app.rs:143`). A 64 KiB read that holds 5000 empty frames costs 5000 moves
  of up to 64 KiB. A hostile guest gets about 100 times its own CPU cost on
  one host thread. It is bounded per read, so it is a slowdown and not a
  stall. A read cursor removes it.
- N2. A watch record may be 32 MiB (`watch.rs:54`), and the host parses it
  into one `Vec` of records (`vm_agent.rs:2297-2328`). That is a bound, but
  a large one per watch channel. The syncer review may already cover it.
- N3. `su_argv` (`guest/agent/src/unix/login.rs:474`) puts the account name
  after `-l` without `--`. The account is looked up first, so a name that
  starts with `-` must exist in the guest to reach `su`. Not followed
  further.
- N4. On the PAM route the script that `su -c` runs holds the environment
  values of the open (`login_script`, `login.rs:443-460`). The script is an
  argument of the `su` process, so every guest user can read those values
  in the process list for the life of the session. No caller passes a secret
  in `env` today.
- N5. On Linux the `secret` of a `Logon` is not checked on the routes that
  were read: the agent is root and `su -l` asks root for no password. So
  `--user dev --password wrong` succeeds. This fits §1.2, but the wire type
  says "the account's password". The `setuid` route was not read.
- N6. The terminal socket (`src/labd/one_shot.rs`, `vm_agent.rs:2551`) is
  bound with the process umask, has a name that can be guessed
  (`term-<channel>.sock`), and serves the first client with no check of the
  peer. Whether another local user can reach it depends on the mode of the
  run directory, which was not checked.
- N7. `ReadDir` in the agent skips an entry error with `continue`
  (`fileops.rs:401`). On Linux the iterator ends after an error. On Windows
  the standard library can return the same `FindNextFileW` error on every
  call, and the loop would then run without end while it holds the cursor
  lock. No way to produce that error in a test was found.
- N8. The legacy JSON parser recurses once per nesting level
  (`json.c:84`). The comment says the depth is three. The real bound is the
  token count of 256. Whether 256 levels fit the stack of the DOS build was
  not checked.
- N9. `json_ulong` (`json.c:313`) and `window_adjust` in the legacy agent
  (`agent.c:491`) wrap on overflow without an error. The host sends no such
  value.
- N10. `utf8_to_ansi` in `plat_win32.c:54-103` accepts overlong UTF-8. The
  bytes `C0 80` become a NUL inside an argument, which cuts the argument.
  serde_json on the host does not produce that encoding.
- N11. `unix/in_root.rs` resolves a path and then acts on it in two steps.
  The file says so and names the micro-VM as the boundary. No finding.
- N12. A second `hello` from the guest with the right token replaces the
  agent's version, features and instance for a live connection
  (`vm_agent.rs:2418-2423`). A hostile guest can lie in the first hello as
  well, so nothing new is gained.
- N13. Guest strings reach the user's terminal as they are: `os_info`
  fields, error messages of the agent, file names in errors. A hostile guest
  can put terminal control sequences into them. `vmlab shell` does that by
  design. The other verbs were not checked for it.
