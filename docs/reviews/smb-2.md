# Second review: the SMB server for guests

Card #915. A second bug-hunting pass over how vmlab serves `share {}` blocks
and container volumes to guests over SMB. It lists faults and makes no fix.
Reviewed at `next` commit `ab017e1b`. All paths are from the repository root,
and all line numbers are from that commit.

The first pass is `docs/reviews/smb.md` (card #594, findings F1 to F11, notes
N1 to N11). This document continues its numbering, so a code names one
finding across both: F12 to F20 and N12 to N18 are new here. A finding of the
first pass is named again only where its fix is incomplete, and the text says
which.

As in the first pass, vmlab has no SMB server of its own: it writes an
`smb.conf`, creates one passdb account and runs a stock Samba `smbd`, now one
per sharing machine. This review covers what vmlab controls: the
configuration, the mount steps it runs in guests, the server lifecycle, the
gateway route, and validation. It does not review Samba.

Severity words are those of the first pass: **security**, **data loss**,
**crash**, **wrong result**, **cosmetic**.

## How the findings were checked

Every finding is from reading the code. Two were also reproduced on helios
with a scratch `smbd` (Samba 4.25.1) run through `stc run card-915`, with the
directives `SmbConfig::render_conf` writes, written by hand into a scratch
directory under the branch checkout, on a random loopback port, with
`smbclient` as the client. No vmlab code ran in these checks, no lab was
started, and the scratch directory was removed after each run:

- F14: two sections `[Data]` (read only, directory `a`) and `[data]`
  (writable, directory `b`). `smbclient //127.0.0.1/Data -c 'get which.txt -'`
  printed the file of `b`, and a `put` through `Data` wrote into `b`.
  `testparm -sv` listed one section `[Data]` with `b`'s path and
  `read only = No`.
- F17: `include = <dir>/x%Uy/inc.conf`, with an `inc.conf` that adds a share
  `[inc]` in each of `x%Uy`, `xy` and `x<user>y`. The `[inc]` that answered
  was the one of `x<user>y`.

The other findings were not run. Each finding names the test that would
prove it. A finding whose test fails to reproduce is wrong and should be
dropped. No test below uses a wall-clock limit.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F12 | wrong result | A `windows-9x` machine is given the modern Windows mount plan (`cmdkey`, PowerShell, `reg`): its SMB share fails after five minutes and `up` refuses to run its provisions |
| F13 | wrong result | The XP-era plan of `windows-xp` and `windows-legacy` machines is one argv element holding the whole `net use` line, so the agent looks for a program by that name and every SMB share fails after five minutes |
| F14 | wrong result | Two shares of one machine whose names differ only in case become one Samba section: the first name serves the second directory with the second's `readonly` |
| F15 | wrong result | A container rebuilt by a reload loses its SMB coordinates while the lab's servers keep running, and its next start fails with "volumes declared but the lab SMB server is not up" |
| F16 | wrong result | The gateway route to a machine's `smbd` is keyed by the MAC it had at `up`; a reload that changes the NIC's `mac` keeps the old key, and the machine's shares stop mounting |
| F17 | wrong result | A `%` in the lab's own path is expanded by Samba in `include`, `log file` and the other `[global]` paths (F6 fix incomplete) |
| F18 | wrong result | A container volume's host path is not checked for `%` or a line break at validation; at `up` the render refuses it and no machine of the lab gets any SMB share |
| F19 | wrong result | A dead `smbd` is restarted only at the next machine start: a running guest keeps a dead share until then (F9 fix incomplete) |
| F20 | wrong result | Starting the servers blocks a runtime worker thread for up to tens of seconds: `std::thread::sleep` polling and child waits inside `async fn ensure_smb` |

### F12. A `windows-9x` machine gets the modern Windows plan (wrong result)

- Where: `src/smb/steps.rs:110-117` (`guest_os_hint`: `windows-9x` falls
  through to `p.starts_with("windows")` and gets `OsHint::Windows`),
  `src/smb/mount.rs:138-220` (`windows_mount_cmds`: `cmdkey /add`, `net use
  ... /user: ... /persistent:yes`, a PowerShell `-EncodedCommand` link step),
  `src/smb/mod.rs:444-485` (the desktop script and the `reg add` logon hook),
  `src/labd/vm.rs:2854-2858` (every share of an `OsHint::Windows` plan is
  `require_for_provisioning`), `src/labd/mount_report.rs:85-101`
  (`check_provisioning_mounts` refuses the provisions).
- Why it is a fault: the `windows-9x` profile (`src/profiles/shipped.wcl:96`)
  is for DOS, Windows 3.x to ME and 2000-era PCs, which carry the legacy
  agent and serve `exec`. None of them ships `cmdkey` or PowerShell, Windows
  9x has no `reg.exe` and its `net use` knows no `/user:` or `/persistent:`.
  The first step of each share is `cmdkey`, so every attempt fails, the step
  gives up after `RetryPolicy::MOUNT.deadline` (five minutes) and the share is
  `share.unmountable`. Because the plan is `OsHint::Windows`, the share was
  marked required for provisioning, so `up` then fails with "cannot start
  provisions or playbooks because SMB share ... did not mount". A 9x machine
  that declares an SMB share (with `smb1 = true`, which it needs) can never
  be provisioned.
- Proof: unit test `a_windows_9x_share_gets_no_modern_windows_step` in
  `src/smb/mod.rs`: `LabSmb::plan` for one machine with one share, then
  `mount_plan(vm, guest_os_hint(Some("windows-9x")))`. Assert that no step's
  `command` is `cmdkey`, `powershell` or `reg`. Today the first step is
  `cmdkey`. A lab test beside `a_failed_windows_smb_mount_blocks_provisioning`
  in `src/labd/lab.rs` shows the effect on `up`: a machine whose resolved
  profile is `windows-9x` and whose exec fails for `cmdkey` (as the program is
  absent), on the paused clock; `up` returns the provisioning refusal.

### F13. The XP-era plan is one argv element (wrong result)

- Where: `src/smb/mod.rs:429-441` (the `OsHint::WindowsXp` step: `command` is
  the whole string of `xp_net_use_string`, `args` is empty),
  `src/smb/mount.rs:352-366` (`xp_net_use_string`),
  `src/labd/mount_retry.rs:127-128` (`run_plan` makes the argv
  `[command, args...]`), `src/labd/vm.rs:2889-2905` (every step goes to
  `agent.exec(argv, ...)`), `guest/agent-legacy/src/plat_win32.c:293-330` and
  `:540-554` (the legacy agent quotes each argv element with a space as one
  argument and passes the line to `CreateProcessA(NULL, ...)`),
  `guest/agent/src/spawn.rs:290` (the Rust agent runs `Command::new(argv[0])`).
- Why it is a fault: the plan was written for screen automation, where the
  string is typed at a console, and `src/labd/vm.rs:2830-2831` still says
  that XP-era guests are "not attempted here". They are attempted: since the
  legacy agent tier, a `windows-xp` machine has an agent that serves `exec`,
  so `mount_shares` gets a handle and runs the step. The argv is
  `["net use Z: \\<gw>\<share> /user:<u> <password> /persistent:yes"]`. The
  legacy agent quotes it as one argument, so `CreateProcessA` looks for a
  program whose name is the whole line, and fails. The step is retried until
  the five-minute deadline and the share ends as `share.unmountable`. The
  same holds for `windows-legacy` (Vista, 7, 2008), which `guest_os_hint`
  also maps to `OsHint::WindowsXp` although that profile keeps the
  virtio-serial Rust agent (commit `5e621e60`), wherever its agent answers:
  `Command::new` on that line finds no program either. Those guests have
  `cmdkey` and `mklink` and would mount with the modern plan.
- Proof: unit test `every_mount_step_is_an_argv` in `src/smb/mod.rs`: for each
  `OsHint`, build `mount_plan` for a machine with a drive-letter share and
  assert that no step's `command` contains whitespace, because `run_plan`
  passes it as `argv[0]`. Today the `WindowsXp` step fails it. To show the
  effect without Windows: the POSIX build of the legacy agent in
  `src/labd/legacy_agent_tests.rs`, sent an exec of the one-element argv
  `["true now"]`, answers with a spawn error, where `["true", "now"]` exits 0.

### F14. Share names that differ only in case become one section (wrong result)

- Where: `src/labd/share_plan.rs:520-547` (`check_collisions` keys names as
  written), `src/smb/config.rs:418-445` (`share_name_problem` compares only
  the reserved names without case), `src/smb/config.rs:348-349` (each share
  is written as a section `[<name>]`).
- Why it is a fault: Samba compares section names without case, and a second
  section whose name matches an earlier one adds its parameters to that
  section. `share { name = "Data" host = "keep" readonly = true }` and
  `share { name = "data" host = "scratch" }` on one machine pass validation
  and the plan. The server then has one share, `Data`, on `scratch`, writable.
  The guest that maps `\\<gw>\Data` expecting a read-only view of `keep` gets
  a writable `scratch`, and `keep` is not served at all. Since card #609 each
  machine has its own server, so the collision is within one machine. The
  author's own data in `keep` is not exposed, but the share the guest sees is
  not the one declared.
- Reproduced on helios, see "How the findings were checked".
- Proof: unit test `share_names_that_differ_only_in_case_collide` in
  `src/labd/share_plan.rs`: one VM with the two shares above, both on SMB.
  Assert that `plan` returns `SharePlanError::Collision`. Today it returns a
  plan with both. The real-`smbd` form is the reproduction: a test beside
  `real_smbd_serves_a_share` in `src/smb/server.rs` spawns a server with the
  two `ShareDef`s and asserts that `smbclient //.../Data -c 'get which.txt -'`
  prints the file of `keep`. Today it prints the file of `scratch`.

### F15. A rebuilt container loses its SMB coordinates (wrong result)

- Where: `src/labd/lab.rs:1077-1112` (`ensure_smb` returns early while a
  `RunningSmb` exists), `src/labd/lab.rs:1203-1211` (the only call of
  `smb_ready`, in the branch that starts the servers),
  `src/labd/container.rs:455` (a new instance has `smb: None`),
  `src/labd/container.rs:647-663` (`spec_for_guest` fails without it),
  `src/labd/lab/reload.rs:96-105` and `:250-287` (a reload, and
  `settle_pending` before a boot, replace the machine with a new instance),
  `src/labd/lab/reload.rs:389-410` (`reshare` keeps the servers when the
  exports did not change).
- Why it is a fault: a container whose volumes ride CIFS (no virtiofs for
  it) gets the gateway and the credential once, when the lab's servers start.
  The coordinates live in the container instance. A reload that changes
  anything else about the container (an `env` value, its `cpus`) builds a new
  instance, at once when it is stopped or before its next boot when it runs.
  The volumes are unchanged, so `reshare` keeps the servers, and the next
  `ensure_smb` returns at its first branch. Nothing calls `smb_ready` on the
  new instance, and its start fails with "volumes declared but the lab SMB
  server is not up". Only a full `down` and `up` brings it back. The CIFS
  fallback for containers is rare today (vmlab-hv serves virtio-fs itself),
  which is why this is ranked below F14.
- Proof: lab test in `src/labd/lab.rs`, beside the `smb_lifecycle_*` tests: a
  lab with one container with one host volume, a `FakeMachine` that records
  `smb_ready` calls and reports no virtiofs, `ensure_shares`; then
  `reload_with` the same lab with one `env` value added, then
  `ensure_shares` again. Assert that the rebuilt instance has received
  `smb_ready`. Today it has not.

### F16. The gateway route keeps the MAC of the first `up` (wrong result)

- Where: `src/labd/lab.rs:1168-1194` (`ensure_smb` reads the machine's first
  MAC once and installs `add_redirect_from(mac, gateway:445 ->
  127.0.0.1:<port>)`), `src/labd/lab.rs:54-72` (`SmbRoute::install`),
  `src/labd/lab/reload.rs:421-428` (`smb_exports`: machine, gateway and
  shares, no MAC), `:398-400` (equal exports keep the servers and the
  routes), `src/labd/machine_build.rs:195-213` (an explicit `nic { mac }` wins
  over the persisted one), `src/labd/lab/reload.rs:103` and `:275` (a rebuild
  renames the DHCP leases to the new MAC, and nothing renames the route).
- Why it is a fault: since card #609 the redirect picks a machine's server by
  the source MAC of the frame. A reload that changes a NIC's explicit `mac`
  while the daemon keeps running (the machine stopped by name) rebuilds the
  machine with the new MAC. The exports are equal, so the old route stays.
  The machine's SMB connections to `gateway:445` no longer match the rule and
  reach nothing, and every mount fails for the five-minute window. If another
  machine of the lab is given the old MAC in the same reload, its frames go
  to the first machine's server, which refuses its password.
- Proof: lab test in `src/labd/lab.rs`: `smb_lab` with an explicit `mac` on
  the NIC, `ensure_shares`, `reload_with` the same lab with another `mac`,
  `ensure_shares` again. Assert that the segment's rule set redirects
  `gateway:445` for a frame from the new MAC (`RuleSet` lookup with that
  source). Today the rule names the old MAC only.

### F17. A `%` in the lab's path is expanded (wrong result, F6 fix incomplete)

- Where: `src/smb/config.rs:284-297` (`state directory`, `cache directory`,
  `passdb backend` from `lab_dir`), `:328` (`include = {dir}/smb.%R.conf`),
  `:331` (`log file`), `src/smb/config.rs:449-461` (`share_host_problem` is
  applied to share paths only).
- Why it is a fault: the first pass noted that the lab directory is written
  into `[global]` unchecked (F6, "Not checked"). The fix of F6 refuses a `%`
  in a share's host path and leaves the lab's own path as it was. A lab under
  a directory such as `/home/u/100%Done/lab` (`%D` is a Samba variable) with
  shares on absolute paths elsewhere passes validation. Samba expands the
  variables in `include` and `log file`. The include of `smb.%R.conf` then
  names another directory, `smb.NT1.conf` is not read, NTLMv1 is never
  accepted, and an XP-era guest's `smb1` share does not log on. Where a
  directory of the expanded name exists, the server reads `smb.NT1.conf` from
  there.
- Reproduced on helios for `include`, see "How the findings were checked".
- Proof: unit test `a_percent_in_the_lab_path_is_not_a_samba_variable` in
  `src/smb/config.rs`: `cfg(true)` with `lab_dir = "/home/u/100%Done/.vmlab/smb"`.
  Assert that `render_conf` refuses it. Today it renders
  `include = /home/u/100%Done/.vmlab/smb/smb.%R.conf`.

### F18. A container volume path is not checked for `%` (wrong result)

- Where: `src/config/validate.rs:534-548` (a host volume is checked for being
  a directory and against the lab state, not with `share_host_problem`;
  compare the VM share check at `:347-357`), `src/smb/config.rs:224-246`
  (`render_conf` refuses the share), `src/smb/mod.rs:284-313` (`start` stops
  every server when one fails).
- Why it is a fault: `container "c" { volume { host = "a%Ub" target = "/data" } }`
  validates. When the container's volumes ride CIFS, the render refuses the
  share at `up`, `LabSmb::start` stops the servers it had started and fails,
  and no machine of the lab, VM or container, gets an SMB share. The `up`
  output says so, but validation said nothing. A line break in the path takes
  the same route.
- Proof: validation test `a_percent_in_a_volume_host_path_is_refused` in
  `src/config/validate.rs`: a container with the volume above and the
  directory `a%Ub` created. Assert an issue that names the `%`. Today the lab
  validates with no issue.

### F19. A dead `smbd` waits for the next start (wrong result, F9 fix incomplete)

- Where: `src/labd/lab.rs:1247-1259` (`watch_smb` reaps and withdraws the
  route, nothing more), `src/labd/lab.rs:1077-1102` (`restart_missing` runs
  only from `ensure_smb`), `src/labd/lab.rs:4490-4493` (`ensure_shares`, from
  a machine start).
- Why it is a fault: the fix of F9 notices an exit within a second, emits
  `smb.exited` and withdraws the gateway route. It starts the server again
  only when some machine of the lab starts. A guest that is running keeps a
  share whose server is gone: a Windows guest's mapped drive and a Linux
  guest's CIFS mount fail every operation until the user restarts a machine
  or runs `down` and `up`, and nothing tells the user that a machine start is
  what restores it.
- Proof: lab test in `src/labd/lab.rs`, beside
  `smb_lifecycle_watchdog_withdraws_access_after_an_exit`, on the paused
  clock: `kill_for_test("web")`, advance the clock past two watchdog ticks
  with no machine start, then assert that `listen_port("web")` is `Some` and
  that the port accepts. Today the server is gone and stays gone.

### F20. The server start blocks a runtime worker (wrong result)

- Where: `src/labd/lab.rs:1144-1146` (`LabSmb::plan` and `start` called
  inline in `async fn ensure_smb`), `src/labd/lab.rs:1081`
  (`restart_missing`, the same), `src/smb/server.rs:267-305`
  (`wait_for_start` polls with `std::thread::sleep` for up to 10 s),
  `:349-376` (the diagnostic re-run, up to 3 s more), `:409-467` and
  `:472-494` (two `pdbedit` children waited on), `src/smb/mod.rs:316-340`
  (up to `PORT_TRIES` = 10 spawns per machine).
- Why it is a fault: every one of these waits runs on a tokio worker thread
  while it holds the `smb` mutex. A normal start takes at least 300 ms per
  sharing machine, and a server that fails to bind can take 13 s per port
  tried. The lab daemon runs the network fabric of every guest on the same
  runtime. `src/labd/lab.rs:4569-4572` records the same fault fixed for
  `remove_tree`: blocking inline "froze the daemon's whole reactor".
- Proof: lab test in `src/labd/lab.rs` on a `current_thread` runtime with a
  real `smbd`: spawn a task that sets a flag when it runs, then call
  `ensure_shares`. Assert that the flag was set while `lab.smb` was still
  held (the task checks `try_lock` and records the result). Today the task
  first runs after `ensure_shares` returns, and records that the lock was
  free.

## Notes

No proving test, or not a fault by itself.

- N12. `smb3 unix extensions` is `Yes` by default on Samba 4.25.1 (`testparm
  -sv` on helios), and `smb.conf` does not turn it off. The F4 fix covers
  SMB1 only. On helios, `smbclient -m SMB3_11 --option='smb3 unix
  extensions=yes' -c 'posix; symlink /etc/passwd planted'` was refused with
  `NT_STATUS_ACCESS_DENIED` and left no link. A Linux kernel client mounted
  with `vers=3.1.1,posix` and `ln -s` was not tried. vmlab's own Linux mount
  uses `vers=3.0`, which negotiates no POSIX context, but a hostile guest can
  open its own session with its credential.
- N13. The host guard admits a guest flow to `127.0.0.1:<port>` for every
  port exposed on the segment, whatever the source MAC
  (`src/net/nat/guard.rs:69-80`, `src/labd/netservices.rs:223-230`). So a
  guest reaches another machine's `smbd` of the same segment directly, not
  only by forging a MAC for the gateway redirect. The other machine's
  password still stops it, as the first pass said of the forged MAC.
- N14. The XP-era plan maps the first two characters of the guest path
  (`src/smb/mount.rs:359-363`). A folder path such as `C:\share` on a
  `windows-xp` machine becomes `net use C: ...`, which fails. Validation does
  not ask for a drive letter on those profiles. This is moot while F13
  stands.
- N15. After a reload that keeps the old servers because a machine runs, a
  machine the reload added is started with an empty SMB plan
  (`LabSmb::mount_plan` knows no such machine), and its shares are reported
  as "no mount step applies to this guest". The reload message says that
  `down` and `up` serve the new shares, but the share's own reason misleads.
- N16. F8 stays as the first pass left it (partially fixed): the readiness
  probe accepts any listener on the port. Not repeated here.
- N17. F5's open point is still open: the SMB1 `SamOEMChangePassword` path,
  which the minimum password age is to stop, was not run. Since card #609 the
  account is that machine's own, so a guest that changes it locks out only
  itself until the next server start.
- N18. Temporary files of `SmbCredentials::load_or_create`
  (`src/smb/config.rs:115-120`) stay in the machine's state directory when
  the daemon dies between the write and the rename. Cosmetic.

## First-pass fixes I checked

- F1: `render_conf` refuses a share over the machine's state and runtime
  directories, `LabSmb::start` over the lab's `.vmlab/smb`, validation over
  the lab's state directory. Sound for this lab. The remainder the first
  pass named (other vmlab state directories) stands.
- F2, F5: one server, passdb and password per machine, the route by source
  MAC, `rpc start on demand helpers = no`. Sound, with F16 and N13.
- F3: every text `run_plan` reports passes through `MountStep::scrub`, and
  every step that carries the password names it in `secret`. Sound.
- F4: `smb1 unix extensions = no`, the `preexec` on modern shares and the
  `smb.%R.conf` include. Sound, with N12 and F17.
- F6: refused for share host paths of VMs. Incomplete for the lab path (F17)
  and for container volumes at validation (F18).
- F7: the route is withdrawn when the servers are dropped, by `reshare`,
  `down` and the watchdog. Sound.
- F9: noticed, not restarted until the next start (F19).
- F10: the credential is written to a temporary file and renamed, under a
  directory lock, and a file that cannot be read is replaced. Sound.
- F11: `create mask = 0644` and `map archive = no`. Sound.

## What I read and what I did not

Read in full: `src/smb/mod.rs`, `config.rs`, `server.rs` (without its tests),
`mount.rs` (without its tests), `steps.rs` up to the virtiofs half.

Read in part: `src/labd/lab.rs` (`RunningSmb`, `SmbRoute`, `ensure_smb`,
`smb_started`, `reap_smb`, `watch_smb`, `share_plan`, the end of `down`,
`stop_for_exit`, `ensure_shares`, `smb_mount_plan`, the `smb_lifecycle_*`
tests), `src/labd/lab/reload.rs` (`reload_with` around the rebuild,
`settle_pending`, `reshare`, `smb_exports`), `src/labd/share_plan.rs` (the
plan, `resolve_share_host`, `check_collisions`, the ports; not its tests),
`src/labd/mount_retry.rs` (`run_step`, `run_plan`), `src/labd/vm.rs`
(`mount_shares`), `src/labd/mount_report.rs` (`check_provisioning_mounts`),
`src/labd/container.rs` (`set_smb`, `spec_for_guest`, `smb_ready`),
`src/labd/machine_build.rs` (MAC assignment), `src/labd/netservices.rs` (the
host services, the guard, `spawn_nat`), `src/net/nat/guard.rs` (the head),
`src/config/validate.rs` (the share and volume checks),
`src/config/mod.rs` (`declaration_digest`), `src/profiles/shipped.wcl` (the
Windows profiles), `guest/agent-legacy/src/plat_win32.c` (`quote_arg`,
`proc_start`), `guest/agent/src/spawn.rs` and `windows/proc.rs` (how argv
becomes a process), `guest/cinit-proto/src/lib.rs` (`SmbInfo`).

Not read: Samba's source. `guest/cinit/src/mounts.rs` (the first pass read
it). The guest agent's Windows logon hook. The virtiofs half of `steps.rs`.
`src/net/rules.rs` beyond the redirect calls. The manual and the e2e suite.
No lab was started, no guest was run, and no fuzzing was done.
