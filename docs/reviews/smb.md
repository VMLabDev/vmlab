# Review: the SMB server for guests

Card #594. A bug-hunting review of how vmlab serves `share {}` blocks and
container volumes to guests over SMB. It lists faults and makes no fix.
Reviewed at `next` commit `e1016c05`. All paths are from the repository root,
and all line numbers are from that commit.

**The card's premise is wrong in one point.** vmlab has no SMB server of its
own. `src/smb` writes an `smb.conf`, creates one passdb account and runs a
stock Samba `smbd` (PRD §7.5, strategy 2). SMB1 and SMB2 parsing,
authentication, path resolution, locks, oplocks and file name encoding are
Samba's code. This review covers what vmlab controls: the configuration it
writes, the credential, the process, the gateway redirect, and what reaches
the logs. It does not review Samba.

Severity words: **security** (against a hostile guest or a peer on the host),
**data loss**, **crash**, **wrong result**, **cosmetic**.

## How the findings were checked

Each finding is from reading the code. Six of them (F1, F2, F4, F5, F6, F11)
were also reproduced on helios with a scratch `smbd` (Samba 4.25.1, built with
`WITH_SMB1SERVER`). The scratch server used the directives of
`SmbConfig::render_conf`, written by hand into a temporary directory, on a
random loopback port, with `smbclient` and `rpcclient` as the client. No
vmlab code ran in these checks, no lab was started, and the scratch files were
removed. F3, F7, F8, F9 and F10 were not run. Each finding names the test
that proves it. A finding whose test does not reproduce is wrong and should
be dropped.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | security | A writable share that holds the lab's `.vmlab` lets the guest rewrite `smb.conf` and reach any directory of the host user (fixed, card #606) |
| F2 | security | One SMB account serves the whole lab, so any sharing guest reads and writes every other machine's shares (fixed, card #609) |
| F3 | security | The lab's SMB password is written to the event history, the daemon log and the `up` output when a mount command fails to run (fixed, card #615) |
| F4 | security | One `smb1` share relaxes every share, and SMB1 clients can plant symlinks to any host path (fixed, card #610) |
| F5 | security | Any guest can change the lab's SMB password over RPC and lock the other guests out (fixed, card #609) |
| F6 | wrong result | A `%` in a share's host path is expanded by Samba, so the share serves another directory |
| F7 | wrong result | The gateway redirect and the exposed loopback port stay after `smbd` stops |
| F8 | wrong result | The start check takes any listener on the port for the lab's `smbd` |
| F9 | wrong result | Nothing notices that `smbd` died, and nothing starts it again |
| F10 | wrong result | A credential file that cannot be parsed gives a new password at every start |
| F11 | cosmetic | Files a guest creates are executable on the host |

### F1. A writable share that holds `.vmlab` gives the guest the whole host account (security)

- Where: `src/config/validate.rs:317-346` (the share checks: the host path
  must be a directory, the name must be plain, the path must hold no line
  break), `src/smb/config.rs:272-293` (the share section: no `veto files`,
  no check on the path), `src/labd/share_plan.rs:499-511`
  (`resolve_share_host`), `src/smb/config.rs:156-164` (`smb.conf`, the
  passdb and the log are in `<lab>/.vmlab/smb`).
- Why it is a fault: nothing refuses a share whose host directory contains
  the lab's own state directory. `share { host = "." guest = "/mnt/lab" }`
  is valid, and the validation tests use that form
  (`src/config/validate.rs:3615`, `:3666`). The guest then sees
  `.vmlab/smb/smb.conf`. `smbd` runs as the host user, the file belongs to
  the host user, and the share is writable, so the guest can overwrite it.
  `smbd` reads a changed configuration again by itself. The guest adds a
  section such as `[esc] path = /home/<user>` with `read only = no`, and
  connects to it with the credential it already has. PRD §2 says "Shares
  must confine guest access to the declared host directory". The same share
  also exposes every other machine's disk image, TPM state and firmware
  variables under `.vmlab`, and those stay readable when the share is
  `readonly = true`. A share of a parent directory of the lab, or of `~`,
  has the same effect.
- Reproduced on helios: a share on a scratch lab root, `smbclient -c 'put
  new.conf .vmlab/smb/smb.conf'` with a section `[esc]` on a directory
  outside the share. One second later `smbclient //127.0.0.1/esc -c 'get
  secret.txt -'` printed the file. `ls .vmlab/smb/*` listed `passdb.tdb`
  and the other state files.
- Proof:
  1. Validation test `a_share_over_the_lab_state_directory_is_refused` in
     `src/config/validate.rs`: a lab with `share { host = "." guest =
     "/mnt/x" transport = "smb" }`. Today it validates with no issue.
  2. A test beside `real_smbd_serves_a_share` in `src/smb/server.rs`: the
     share's `host_path` is the parent of `lab_dir`, `readonly = false`.
     With `smbclient`, `put` a copy of `smb.conf` that has one more section
     on a directory outside the share, then `ls` that new share. Today the
     listing succeeds. The test waits on the listing in a retry loop bounded
     by attempts, not by time.
- Fixed by card #606. Validation refuses a `share {}` host directory, and a
  container volume's host directory, that holds the lab's state directory or
  lies inside it, on every transport (`share_over_lab_state` in
  `src/config/validate.rs`). `SmbConfig::render_conf` refuses a share over
  `<lab>/.vmlab/smb` or over the SMB runtime directory, so `smbd` does not
  start with one (`share_state_problem` in `src/smb/config.rs`). Both compare
  resolved paths, so a symlink or a `..` does not pass. Named volumes under
  `.vmlab/volumes` are still served. Proof tests:
  `a_share_over_the_lab_state_directory_is_refused` and
  `a_share_that_holds_the_smb_state_cannot_reach_outside_it`
  (`src/smb/server.rs`). Both failed before the fix on helios. A share of
  another directory that vmlab keeps state in (`~/.local/share/vmlab`,
  another lab's root) is not refused.

### F2. One account for the whole lab (security)

- Where: `src/smb/config.rs:61-67` and `:76-103` (one credential per lab,
  the username is the host user), `src/smb/mod.rs:108-113` (every machine
  gets a clone of it), `:170-176` and `src/smb/config.rs:280` (every share
  has the same `valid users`), `src/labd/lab.rs:1069-1092` (the same `smbd`
  answers at every sharing segment's gateway).
- Why it is a fault: PRD §7.5 says "a share is mappable only with its owning
  VM's credential, scoping shares to their declaring VM even on a shared
  segment". The code comment at `src/smb/config.rs:55-60` records the
  deviation. In effect a guest that has one read-only share can read and
  write every writable share of every other machine in the lab, also of a
  machine on another segment, because all of them are sections of one
  server behind one account. The share list is open to it too
  (`browseable = yes`, `:281`).
- Reproduced on helios: two sections with the rendered directives,
  `vma_ro` and `vmb_rw`. `smbclient -L` with the one credential listed
  both, and `smbclient //127.0.0.1/vmb_rw -c 'put /etc/hostname
  from-a.txt'` wrote the file.
- Proof:
  1. Unit test `each_machine_has_its_own_credential` in `src/smb/mod.rs`:
     `LabSmb::plan` for two machines, assert that
     `credentials("web") != credentials("xp")` and that the two
     `ShareDef::allowed_user` values differ. Today both are equal
     (`credentials_present_per_vm_and_stable`, `src/smb/mod.rs:378`, asserts
     the equality).
  2. Real-`smbd` test: a config of two machines, connect to the second
     machine's share with the first machine's credential, expect
     `NT_STATUS_ACCESS_DENIED`. Today the connect succeeds.
- Fixed by card #609. An unprivileged `tdbsam` takes no account without a
  Unix user (`pdbedit -a -u <name>`: "Failed to add entry"), so one `smbd`
  cannot hold an account per machine without root. vmlab now runs one `smbd`
  for each sharing machine (`LabSmb::start` in `src/smb/mod.rs`): its own
  state directory `.vmlab/smb/machines/<machine>/` with its own passdb and
  `creds`, its own password, its own port, and that machine's shares alone.
  The gateway rule for port 445 is per machine, chosen by the source MAC of
  the frame (`RuleSet::add_redirect_from` in `src/net/rules.rs`,
  `ensure_smb` in `src/labd/lab.rs`). The MAC only picks the server. A guest
  that forges it reaches a server whose password it was not given. The user
  name is still the host user's for every machine, so `ShareDef::allowed_user`
  does not differ as proof 1 asked: the passwords do. The port range grew
  from 10 to 100 (14450 to 14549), one port per sharing machine. Proof tests:
  `each_machine_has_its_own_credential` and
  `one_machines_credential_does_not_open_anothers_share` (`src/smb/mod.rs`,
  the second with a real `smbd`). Both failed before the fix on helios. Side
  effect on F4, point 1: `smb1` on a share now relaxes the server of its own
  machine only. Not checked with a Windows or macOS guest.

### F3. The SMB password reaches the event history, the log and the `up` output (security)

- Where: `src/smb/mount.rs:37` (`password=` in the `mount` options),
  `:75-80` (in the `mount_smbfs` URL), `:151-157` and `:173-182` (`cmdkey
  /pass:` and `net use ... <pass>`), `:341` (the `reg add` value);
  `src/labd/vm_agent.rs:1064` (`display = argv.join(" ")`), `:1096-1100`,
  `:1112`, `:1114`, `:1129` (four error messages that hold `display`);
  `src/labd/mount_retry.rs:88-91` (the error text is passed on), `:133-134`
  (`tracing::debug!`), `:159-173` (`tracing::warn!`, the
  `share.unmountable` payload, the mount report);
  `src/labd/events.rs:41-46` (every event goes to the log and to the event
  history file).
- Why it is a fault: every SMB mount step carries the password in its argv.
  When the exec itself fails, the agent client builds the error from the
  whole argv: "exec `net use Z: \\10.0.0.1\data /user:u <password>
  /persistent:yes` timed out after 30s", "agent channel closed during exec
  `...`", "exec `...`: <agent error>". The retry loop puts that text into
  the debug log at each attempt. When the step gives up, the text is in the
  `share.unmountable` event (written to the lab's event history and shown by
  `vmlab events`), in a `warn` line of the daemon log, and in the mount
  report that `vmlab up` prints. This happens on an ordinary Windows boot: early after
  it the agent fails to spawn children for a short time
  (`src/smb/steps.rs:33-35`), and each such failure is an agent error with
  the argv in it. The password opens every share of the lab
  (F2). `smbd` listens on `127.0.0.1` (`src/smb/config.rs:218-219`), so any
  local user of the host who reads a pasted `up` output, a bug report or a
  log can map the shares as the host user.
- Checked and sound here: a step that succeeds logs its command name only
  (`src/labd/mount_retry.rs:140-143`), `pdbedit` gets the password on stdin
  (`src/smb/server.rs:374`), and cinit keeps it out of the console log
  (`guest/cinit/src/mounts.rs:179-180`).
- Proof: unit test `a_failed_exec_does_not_put_the_credential_in_the_event`
  in `src/labd/mount_retry.rs`: a plan with one step whose args hold
  `username=u,password=S3CRET`, an `exec` that returns the error
  `vm_agent.rs:1129` builds (`exec \`{argv joined}\` timed out after 30s`),
  on the paused clock that module's tests use. Assert that the payload
  handed to `unmountable` and the report's reason do not contain `S3CRET`.
  Today both contain it.

- Fixed by card #615: a `MountStep` names the password its command line
  carries (`secret`), and `run_plan` passes every text it reports about the
  step through `MountStep::scrub`: the attempt errors, the reason of the
  event, the log lines and the mount report. The password, as written and
  percent-encoded, becomes `[redacted]`. The agent client's `exec` errors
  still name the whole argv for its other callers.

### F4. One `smb1` share relaxes every share, and SMB1 lets a guest plant symlinks (security)

- Where: `src/smb/mod.rs:166-169` and `src/smb/config.rs:206-211`,
  `:244-253` (`any_smb1` sets the protocol floor, the signing and the NTLM
  level of the whole server), `src/smb/config.rs:214-268` (no `smb1 unix
  extensions = no`), `src/smb/mount.rs:21-23` (the comment says the server
  does not advertise Unix extensions).
- Why it is a fault: two things.
  1. `smb1 = true` on one share of one machine sets `server min protocol =
     NT1`, `server signing = auto` and `ntlm auth = ntlmv1-permitted` for
     every share of every machine. A modern guest's share can then be
     reached unsigned with NTLMv1. The PRD describes the relaxation as a
     property of the share.
  2. Samba's SMB1 server offers the CIFS Unix extensions by default. A
     client that negotiates them creates a symlink with any target inside a
     writable share. `smbd` does not follow a link that leaves the share
     (checked, see "Checked and found sound"), but the link is now a file on
     the host. Any host program that reads the shared directory follows it:
     an editor, `cp -r`, `tar`, a build. A link `notes.txt -> ~/.ssh/id_ed25519`
     turns "copy the shared folder somewhere" into a copy of the key. Because
     of point 1, every guest of the lab can do this as soon as one share asks
     for SMB1.
- Reproduced on helios: a server with the `any_smb1` directives.
  `smbclient -m NT1 -c 'posix; symlink /etc/passwd planted'` printed
  "Server supports CIFS extensions 1.0", and the host directory then held
  `planted -> /etc/passwd`.
- Proof:
  1. Real-`smbd` test `an_smb1_client_cannot_create_a_symlink`, skipped when
     `check_nt1_supported()` is false: config with `any_smb1 = true` and one
     writable share, run the `smbclient` command above, assert that the host
     directory holds no symlink. Today it holds one.
  2. Unit test in `src/smb/config.rs`: a config with one `smb1` share and
     one modern share. Assert that the modern share cannot be mapped with
     NT1. This cannot pass with one `smbd`, which is the fault.
     `conf_protocol_floor_toggles_on_smb1` (`:504`) shows the global switch.
- Fixed by card #610, with one `smbd` and one remainder.
  1. `smb1 unix extensions = no` is written for every server, so no SMB1 client
     is offered the extensions and the symlink call fails.
  2. NT1 is refused on every share that did not ask for it: such a share
     gets a `preexec` (a `/bin/sh -c` line) that fails unless `%R` (the dialect of the session) is
     SMB2 or newer, with `preexec close = yes`. `ntlm auth` is
     `ntlmv2-only` in `smb.conf`. The NTLMv1 and LM acceptance moved to
     `smb.NT1.conf` beside it, read through `include = .../smb.%R.conf`:
     `smbd` reads its configuration again after the dialect is chosen, so
     only an NT1 session gets it. Proof 2 was written as "cannot pass with
     one `smbd`". It does pass: the three directives were tried on helios
     (Samba 4.25.1) before the fix was written.
  3. Remainder: `server signing = auto` still holds for every session of a
     server with an `smb1` share. Since card #609 that is the server of one
     machine, not of the lab. `smbd` fixes signing before the dialect is
     negotiated, so the include cannot restore `mandatory` for SMB2 and
     newer (tried: the NT1 client with signing off was refused under
     `mandatory` plus the include). A modern share of such a machine is
     therefore reached with NTLMv2 only and never over NT1, but a client
     that does not ask for signing is not made to sign. This now concerns
     only the modern shares of a machine that also declares an `smb1`
     share. Closing it needs proof that XP-era clients sign reliably under
     `mandatory`.
  The tests: `an_smb1_client_cannot_create_a_symlink` and
  `an_smb1_share_does_not_relax_the_share_beside_it` in `src/smb/server.rs`
  (real `smbd`), `an_smb1_share_relaxes_only_itself` and
  `conf_never_offers_the_smb1_unix_extensions` in `src/smb/config.rs`.

### F5. Any guest can change the lab's SMB password (security)

- Where: `src/smb/config.rs:241-243` (user-level security with one account),
  `src/smb/server.rs:118-121` (the account is created at each start),
  `src/smb/config.rs:76-87` (the password the guests are given comes from
  the `creds` file).
- Why it is a fault: the account every guest logs in with is an ordinary
  Samba account, and Samba lets an account change its own password over the
  SAMR pipe on `IPC$`. One guest that does so breaks every later mount and
  reconnect of every other guest: vmlab keeps handing out the password in
  `.vmlab/smb/creds`, and the stored Windows credentials hold the old one.
  The lab recovers only when `smbd` starts again, where `pdbedit -a` writes
  the password of the file back. A guest with nothing but a read-only share
  can do this.
- Reproduced on helios: `rpcclient -p <port> -U user%old 127.0.0.1 -c
  'chgpasswd3 user old new'`. After it, `smbclient` with the old password
  was refused and with the new password it listed the share.
- Proof: real-`smbd` test `a_guest_cannot_change_the_lab_password`, skipped
  when `rpcclient` is absent: run the command above against a spawned
  `SmbServer`, then connect with the original password and assert that the
  listing succeeds. Today it fails with `NT_STATUS_LOGON_FAILURE`.
- Fixed by card #609. The fault was wider than the finding says: an
  unprivileged `smbd` runs every session with the uid it started with, and
  Samba's RPC servers take that uid for root. So `setuserinfo2 <user> 23
  <new>` set the password too, with no old password, and an account policy
  ("minimum password age") stopped `chgpasswd3` only. Reproduced on helios.
  `smb.conf` now holds `rpc start on demand helpers = no`
  (`SmbConfig::render_conf`): `smbd` serves a pipe only through
  `samba-dcerpcd`, and this keeps it from starting one, so `samr`, `lsarpc`
  and `srvsvc` are refused and shares still mount. The share list (`net
  view`, `smbclient -L`) is empty as a result. The minimum password age is
  set as well (`forbid_password_change` in `src/smb/server.rs`), for SMB1's
  `SamOEMChangePassword`, which is no RPC pipe. That path was not run.
  With F2 fixed, the account is one machine's own in any case. Proof test:
  `a_guest_cannot_change_the_lab_password` (`src/smb/server.rs`), which runs
  both `chgpasswd3` and `setuserinfo2`. It failed before the fix on helios.
  Open: Samba before 4.16 does not know the directive and ignores it, and a
  Windows guest was not run against a server without `srvsvc`.

### F6. A `%` in a host path is expanded (wrong result)

- Where: `src/smb/config.rs:274` (`path = <host path>` written as is),
  `:350-354` (`share_host_problem` refuses line breaks only),
  `src/config/validate.rs:344-346`.
- Why it is a fault: Samba expands `%U`, `%u`, `%h`, `%m`, `%d` and other
  variables in `path`. A share whose host directory has such a sequence in
  its name is served from another directory, or fails every tree connect
  when the expanded path does not exist. The lab file names one directory
  and the guest gets another one.
- Reproduced on helios: a share on the directory `a%Ub`, beside a directory
  `a<host user>b`. `smbclient -c 'get which.txt -'` returned the file of the
  second directory.
- Proof: unit test `a_percent_in_a_host_path_is_not_a_samba_variable` in
  `src/smb/config.rs`: a `ShareDef` with `host_path = "/home/u/100%U"`.
  Assert that `render_conf` refuses it, or that the rendered line cannot be
  expanded. Today it renders `path = /home/u/100%U`. The real-`smbd` form is
  the reproduction above.
- Not checked: the lab directory is written the same way into the
  `[global]` section (`:225-237`, `:255`), so a `%` in the lab's own path
  may move the log, the state directories or the passdb.

### F7. The gateway redirect and the exposed port outlive `smbd` (wrong result)

- Where: `src/labd/lab.rs:1077-1090` (`add_redirect` returns a `RuleId` that
  is dropped, `expose_host_service` has no opposite),
  `src/labd/lab/reload.rs:406-408` and `src/labd/lab.rs:2986-2990` (`smbd`
  stops, the rule and the port stay), `src/net/rules.rs:509-534`
  (`best_redirect`: of two equal rules the earlier one wins),
  `src/net/nat/guard.rs:69-80` (an exposed port admits a guest flow to
  `127.0.0.1`).
- Why it is a fault: a reload that changes the shares while no machine runs
  stops `smbd` (`reshare`). The redirect `gateway:445 -> 127.0.0.1:<port>`
  and the entry in `host_services` stay in the segment. Two effects.
  1. Until the next start, every guest of the segment can reach whatever
     process binds that loopback port next. That can be another lab's
     `smbd`, which takes the first free port of the same ten
     (`src/labd/share_plan.rs:27-28`).
  2. The next start adds a second redirect. If the old port was taken in
     between, the new `smbd` has another port, the old rule still wins, and
     the lab's guests are sent to the other process. Their mounts fail with
     a wrong password, or reach the wrong server.
- Proof: steps, in one lab daemon. Start a lab with one SMB share and stop
  its machines by name, so that the daemon stays. Change the share's name in
  `vmlab.wcl` and reload. Bind the old port with another process. Start the
  machine. Then list the segment's rules (`RuleSet::list`): today two
  redirects from `gateway:445` exist and the first names the old port, and
  `host_services` holds both ports. As a daemon test: after the reload,
  assert that the segment has no redirect from `gateway:445` and that
  `host_services` is empty. Today both remain.

### F8. The start check takes any listener for `smbd` (wrong result)

- Where: `src/smb/server.rs:216-235` (`wait_for_start`),
  `src/labd/share_plan.rs:40-44` (the port is probed before, not held).
- Why it is a fault: `smbd` counts as started when the child is alive after
  300 ms and a TCP connect to the port succeeds, or when the child is alive
  after 10 s. The connect does not show who listens. When another process
  took the port between the probe and the bind, `smbd` aborts with a core
  dump. On helios the abort took less than 300 ms. The test helper at
  `:433-438` records that a core dump can keep it alive past the wait. In
  that window the connect reaches the other process and the start is
  reported as good. The gateway redirect then sends every guest's SMB
  session to that process. The ports are ten fixed numbers on loopback, so
  another user of the host can hold one on purpose and receive the guests'
  NTLM exchanges. The second branch has the same weakness: an `smbd` that
  hangs for 10 s without opening its port is also reported as started.
- Proof: unit test `a_listener_that_is_not_the_child_is_not_a_start` in
  `src/smb/server.rs`: bind and listen on a loopback port, spawn `sleep 60`
  as the child, call `wait_for_start(&mut child, port)`. Today it returns
  `None` (started). The test kills the child at its end and has no limit of
  its own.

### F9. A dead `smbd` is not noticed (wrong result)

- Where: `src/labd/lab.rs:992-994` (`ensure_smb` returns when a `LabSmb` is
  held), `src/smb/server.rs:76-79` and `:189-196` (the child is only looked
  at by `stop`).
- Why it is a fault: after the start, nothing waits on the `smbd` child. If
  it dies (an abort, the kernel's OOM killer, a user's `pkill smbd`), the
  lab keeps its `LabSmb`, so every later `ensure_smb` returns at once.
  Mounts of machines started later fail for the whole retry window with a
  network error, and `up` reports the share as unmountable without saying
  that the server is gone. Only a full `down` and `up` brings it back.
- Proof: steps. Start a lab with one SMB share. Kill the lab's `smbd`
  parent process. Restart the machine. Today no second `smb.started` event
  appears and the share ends as `share.unmountable`. As a daemon test with a
  real `smbd`: kill the child, call `ensure_shares`, assert that the port
  accepts again.

### F10. An unreadable credential file means a new password at every start (wrong result)

- Where: `src/smb/config.rs:76-103` (`load_or_create`).
- Why it is a fault: the file is created with `create_new` and written in a
  second step, and both errors are ignored. A daemon that dies between the
  two leaves an empty file. An empty file, a file with another content, or a
  file whose user name is not the current one fails the read check, a new
  password is generated, and `create_new` fails because the file exists. The
  password is used and never stored. Every later start does the same. The
  comment at `:71-75` gives the cost: each remembered guest mapping fails
  with "user name or password is incorrect" after the next `up`. Nothing
  warns. The line `up` prints (`src/labd/lab.rs:1062`) points the user at
  the file, which then holds no valid credential.
- Proof: unit test `a_credential_file_that_cannot_be_parsed_is_replaced` in
  `src/smb/config.rs`: write an empty `creds` file, call `load_or_create`
  two times. Assert that both calls return the same password and that the
  file holds it. Today the two passwords differ and the file stays empty.

### F11. Files a guest creates are executable on the host (cosmetic)

- Where: `src/smb/config.rs:272-293` (no `create mask`, no `map archive =
  no`).
- Why it is a fault: with Samba's defaults a file that a client creates gets
  mode 0744. Source files and documents written from a guest are executable
  on the host, and `git` reports a mode change for each of them.
- Reproduced on helios: `smbclient -c 'put /etc/hostname from-a.txt'`, then
  `ls -l` on the host showed `-rwxr--r--`.
- Proof: real-`smbd` test: `put` a file into a writable share and assert
  that its host mode has no execute bit.

## Notes

No proving test, or not a fault by itself.

- N1. Oplocks against host edits. The server grants oplocks (Samba's
  default; only `smb2 leases = no` is set, `src/smb/config.rs:263`) and
  `kernel oplocks` is off by default. A host program that writes a shared
  file does not break a guest's oplock, so a guest that holds the file open
  can keep reading its cached content. Shares exist so that the host edits
  and the guest reads, so this may matter. I did not prove it. To try: in a
  Linux guest hold a file of the share open and read it, change the file on
  the host in place, read again through the same descriptor.
- N2. Ten fixed ports (`src/labd/share_plan.rs:27-28`). The eleventh lab
  with SMB shares on one host gets no share server, and any process of any
  user that binds 14450 to 14459 on loopback denies shares to every lab.
  The error text says so. It is a limit, not a wrong result.
- N3. `ensure_smb` checks `self.smb` and releases the lock before it starts
  the server (`src/labd/lab.rs:992`, `:1107`). Two starts at the same time
  can both spawn an `smbd` on one state directory. I expect the second to
  fail on the first one's pid file and to print "SMB server failed to
  start" although shares work. Not proven.
- N4. `current_unix_user` (`src/smb/config.rs:358-379`) trusts `$USER`
  before it asks `id`. A daemon started with a `$USER` that is not its uid
  writes that name into `force user` and `valid users` unchecked
  (`:280-284`). The value is the host user's own, not a guest's.
- N5. `share_host_problem` (`:350-354`) refuses line breaks only. A host
  path that ends in `\` continues onto the next line of `smb.conf`, and
  leading or trailing spaces are dropped by Samba. Both lose the share, not
  the confinement.
- N6. `lanman auth = yes` and `client lanman auth = yes` (`:249-250`) do
  nothing useful. The password has 24 characters, so no LM hash exists, and
  the second directive is a client setting.
- N7. Inside the guest the password is in the `HKLM\...\Run` value, in
  `C:\Users\Public\Desktop\vmlab-shares.cmd`, and in the argv of `mount` and
  `mount_smbfs` while they run (`src/smb/mount.rs`). Every user of the
  guest can read it. With F2 that was every share of the lab. Since card
  #609 it is the shares of that machine.
- N8. `remove_run_dir` (`src/smb/server.rs:263-272`) keeps the runtime
  directory while a pid file exists. After a failed start whose diagnostic
  re-run came up (`:279-300`), the re-run's pid file is left and the
  directory stays until the next start. Cosmetic.
- N9. Not checked: what a guest can change on host files beyond their
  content. Samba's defaults store DOS attributes in `user.*` extended
  attributes and map ACL changes to `chmod`.
- N10. Not checked: host file names that Windows cannot show (a `:`, a
  trailing dot) and two host names that differ only in case. Samba mangles
  the first and shows one of the second to a Windows client.
- N11. Sessions are bounded by the NAT engine, not by `smbd`: 256 guest TCP
  flows per engine and 128 per guest address (`src/net/nat/mod.rs:173-176`),
  so one guest can make at most 128 `smbd` children per segment. No `max
  smbd processes` is set.

## Checked and found sound

- A symlink in the share that points outside it is not served, as a file
  and as a directory, and a `..` path out of the share is refused (run on
  helios).
- An anonymous session cannot connect to a share (run on helios).
- `SmbServer::stop` kills the parent only (`src/smb/server.rs:189-193`).
  That is enough: `notifyd`, `cleanupd` and the client's child exited within
  one second of a SIGKILL of the parent (run on helios). I first took this
  for a fault. It is not one.
- A share name or a host path cannot add a section or a directive
  (`src/smb/config.rs:180-194`, `:319-354`), and two shares with one name
  are refused (`src/labd/share_plan.rs:514-545`).
- The credential file is created 0600 and does not follow a symlink
  (`src/smb/config.rs:91-101`). The runtime directory is private and its
  owner is checked (`src/paths.rs:291-322`).
- The orphan reaper recognises the lab's `smbd` by its `-s` path
  (`src/qemu/process.rs:488-493`).

## What I read and what I did not

Read in full: `src/smb/mod.rs`, `config.rs`, `server.rs`, `mount.rs`,
`steps.rs`.

Read in part: `src/labd/lab.rs` (`ensure_smb`, `share_plan`, the end of
`down`, `ensure_shares`, `smb_mount_plan`), `src/labd/lab/reload.rs`
(`reshare`, `smb_exports`), `src/labd/share_plan.rs` (the plan, the port
probe, the collision check, not its tests), `src/labd/mount_retry.rs`
(without tests), `src/labd/vm.rs` (`mount_shares`), `src/labd/vm_agent.rs`
(`exec`), `src/labd/mount_report.rs` (`attempt_failed`, `gave_up`),
`src/labd/events.rs` (`emit`), `src/labd/mod.rs` (`teardown`),
`src/labd/netservices.rs` (the host services and the guard),
`src/net/rules.rs` (`add_redirect`, `best_redirect`),
`src/net/nat/guard.rs` (the head), `src/supervisor/mod.rs`
(`release_lab_locked`), `src/qemu/process.rs` (the orphan matcher),
`src/config/validate.rs` (the share checks), `src/paths.rs`
(`ensure_private_dir`, `runtime_dir_in`), `guest/cinit/src/mounts.rs`
(`mount_volume`), PRD §2 and §7.5.

Not read: Samba's source. The gateway and NAT code that carries the
redirect, beyond the lines named (it is in `docs/reviews/networking.md`).
The container volume code in `src/labd/container.rs` beyond its use of the
SMB coordinates. The guest agent's side of the Windows logon hook
(`guest/agent/src/windows/logon.rs`). `src/cli/lab_move.rs`. The manual and
the e2e suite. No fuzzing of the SMB protocol was done, and no lab was
started for this review.
