# Review: appshare and media

Card #725, 2026-11-23. This is a fault list. It changes no code. The lead
makes the fix cards from it, the worst finding first.

Line numbers are those of commit `69892a3a` (`next` at the start of the
card).

## How the review was done

The review is by reading. No test was written or run for a finding, so each
finding is a claim from the code, and the proposed test is what proves or
refutes it. No proposed test uses a wall-clock limit. One command was run on
helios, for note N13.

Severity words are those of the card: data loss, crash, security against a
hostile guest or peer, wrong result, cosmetic. "Confidence" says how sure the
reading is.

## Who can reach what

- The host end of a shared app runs in the `vmlab` CLI process of the person
  at the desktop (`src/appshare/mod.rs:1-11`). The lab daemon only brokers:
  it relays the records of the agent's app channel to the CLI without
  decoding them (`src/labd/app_share.rs:293-311`) and splices each transport
  connection (`src/labd/app_share.rs:470-511`).
- A guest alone controls: every record on the app channel (`ready`,
  `pending`, `log`, `diagnostic`, and any other record it chooses to write),
  the answer to the daemon's `command -v waypipe` probe, how many
  connections it announces, and every byte of the window protocol. The
  window protocol's bytes go to three programs on the host: the user's X
  server (X11), `waypipe client` (waypipe) and FreeRDP (RDP). vmlab parses
  only the X11 connection setup (`src/appshare/x11.rs:322-348`).
- The X11 transport gives the guest program the reach of `ssh -X` into the
  host X server. PRD §1.2 and ADR-0016 state this and accept it, so it is
  not a finding. Finding F2 is about who decides that X11 is used.
- A media image is built from a folder that the lab file or a local template
  definition names (`src/labd/machine_build.rs:220-229`,
  `src/template/build.rs:2405-2430`). No pulled template and no guest names a
  media folder. The folder path may be absolute and may hold links to any
  file the daemon's user can read, and the build follows the links
  (`src/media/iso.rs:27-33`, `src/media/hash.rs:79`). The person who wrote
  the lab file is trusted, so this is a statement, not a finding.
- A built ISO is attached read-only (`src/labd/hypervisor/vmlab_hv/describe.rs:537-545`,
  `src/qemu/cmdline.rs:578-581`). A floppy is attached writable
  (`hv/crates/vmlab-hv-vmm/src/builder.rs:1149`). Finding F1 follows from
  that.
- `eject_cdrom()` is the only media change at run time
  (`src/labd/vm.rs:2360-2368`). It holds no lock and keeps no state. The
  medium is back at the next start of the hypervisor process, which PRD
  §10.3 states ("out until QEMU restarts").

## What was read and what was not

Read in full, without the test modules:

- `src/appshare/mod.rs`, `x11.rs`, `waypipe.rs`, `rdp.rs`
- `src/media/mod.rs`, `cache.rs`, `hash.rs`, `iso.rs`, `floppy.rs`
- `src/labd/app_share.rs`
- `src/cli/app.rs`
- `guest/agent-proto/src/app.rs`

Read in part:

- `src/labd/machine_build.rs`: `build_vm` (150-270), the only caller of the
  media cache.
- `src/labd/lab.rs`: `LabRuntime::build` (495-550).
- `src/labd/lab/reload.rs`: `build_machines`, `settle_pending` (198-285).
- `src/labd/hypervisor/vmlab_hv/describe.rs`: disks, CD-ROMs, the floppy
  (478-556, 826).
- `src/qemu/cmdline.rs`: the CD-ROM and floppy arguments (575-604).
- `hv/crates/vmlab-hv-vmm/src/builder.rs`: the floppy controller
  (1138-1160). `hv/crates/vmlab-disk/src/lib.rs:148-153`.
- `src/labd/vm.rs`: `eject_cdrom` (2360-2368), and the two `Media`
  adapters (`src/labd/hypervisor/qemu.rs:339-343`,
  `src/labd/hypervisor/vmlab_hv/control.rs:350-368`).
- `src/labd/vm_agent.rs`: `open`, `open_app`, `open_app_conn` (840-885,
  1041-1066). `src/labd/mod.rs:904-942`.
- `src/config/validate.rs`: `check_media` (1993-2000).
- `src/template/build.rs`: how a template build carries media (2387-2430).
- `docs/vmlab-prd.md` §1.2, §6.3, §10.3, §20, and ADR-0016.

Not read:

- `guest/agent/src/app.rs`, the guest end of a shared app (the display
  socket, the Xauthority file, the waypipe server, the RDP preparation). It
  runs in the guest.
- `src/labd/identity.rs` (`resolve_app`), which resolves the login.
- The floppy, IDE, AHCI and ATAPI device models of `hv/`, and what a
  snapshot does with an ejected or written medium.
- winpr's `CommandLineParseCommaSeparatedValuesEx`. `freerdp_split`
  (`src/appshare/rdp.rs:180-303`) says it is a transcription of it. The
  review did not compare the two.
- `waypipe`, FreeRDP, `xorriso`, `genisoimage`, `mkisofs` and mtools
  themselves. Statements about what they do are from memory and are marked.
- Every test module. A proposed test may exist already under another name.

## Findings

### F1. A guest writes into the shared media cache through its floppy

- Where: `src/media/cache.rs:44-48` and `:69-71`,
  `src/labd/machine_build.rs:221-243`,
  `src/labd/hypervisor/vmlab_hv/describe.rs:826`,
  `hv/crates/vmlab-hv-vmm/src/builder.rs:1149`.
- Fault: a `media { kind = "floppy" }` block attaches the cache entry
  itself, `.vmlab/media/<digest>.img`, and vmlab-hv opens it with
  `read_only = false`. A guest write to drive A changes the cache entry.
  `MediaCache::ensure` returns an existing entry without a look at its
  content (`dest.is_file()`), so the changed image is what every later start
  attaches, for as long as the source folder stays the same. `vmlab destroy`
  does not remove it. Three results follow.
  - A guest keeps data, or a changed answer file or driver, across its own
    destroy and rebuild. A template build that uses a floppy answer file can
    be handed a file that an earlier guest wrote.
  - Two machines with the same folder and label get the same file. All
    instances of a lab root share `.vmlab` (`src/paths.rs:236`), so this
    holds across instances too. One guest changes what the other reads.
  - vmlab-hv takes no lock on an image
    (`src/config/validate.rs:2003-2005` says so). Two running guests that
    both write the one image corrupt its FAT.
- Proof: two tests.
  - `a_built_floppy_is_not_changed_by_its_reader` in `src/media/cache.rs`:
    `ensure` a floppy, overwrite a sector of the returned file (what
    `write_at` does), `ensure` again, and compare the returned image to one
    built from the same folder in a fresh cache. They must be equal. Today
    they differ.
  - `two_machines_do_not_share_a_writable_floppy` in
    `src/labd/hypervisor/vmlab_hv/describe.rs` or the lifecycle tests: build
    two pc VMs with the same floppy `media {}` block and assert that the two
    descriptions do not name one writable path.
- Severity: security against a hostile guest (it changes host files that
  other machines and later builds trust), and wrong result.
- Confidence: high.

### F2. The guest decides that the session falls back from waypipe to X11

- Where: `src/appshare/mod.rs:243-251` and `:278-290`,
  `src/labd/app_share.rs:226-244`.
- Fault: on a Wayland host, `vmlab app run` without `--via` picks waypipe.
  The daemon then runs `sh -c "command -v waypipe"` in the guest, and a
  non-zero exit or empty output is answered as `unsupported`. The CLI turns
  that one code into a second attempt over X11 through Xwayland, with one
  line on stderr. The guest controls the probe's answer, so a hostile guest
  chooses the transport, and it chooses the one that gives it the reach of
  `ssh -X` into every X client of the host session. PRD §1.2 says that
  "opting into the X11 app-share transport grants the desktop access". Here
  nobody opted in: the user gave no `--via x11`. PRD §20.1 asks for this
  fallback, so the two sections disagree, and the code follows §20.1.
- Proof: `a_guest_refusal_does_not_select_x11_unasked` in
  `src/appshare/mod.rs`: `daemon_fallback(Mode::Waypipe, None, &host(true,
  true, true, true), &unsupported)` must be `None` unless the user allowed
  X11 (with `--via x11` or a host setting). Today it is `Some`. The fix
  needs a decision between §1.2 and §20.1 first.
- Severity: security against a hostile guest.
- Confidence: high for the code path. The size of the reach under Xwayland
  (other X clients only, not Wayland clients) is from memory.

### F3. An RDP session shares the host clipboard with the guest, both ways, with no way to turn it off

- Where: `src/appshare/rdp.rs:71`.
- Fault: `freerdp_args` always passes `+clipboard`. While the session runs,
  the guest's RDP server is told each time the host clipboard changes and
  can ask for its content without a paste by the user, and it can set the
  host clipboard. A password that the user copies for another program
  during the session is readable by the guest. ADR-0016 names the clipboard
  as a benefit of RemoteApp. PRD §1.2 lists only X11 as a transport that
  opens the desktop to the guest, and says guest input is untrusted. No
  flag and no host setting turns the clipboard off or limits its direction.
- Proof: `the_rdp_clipboard_is_not_shared_unasked` in
  `src/appshare/rdp.rs`: `freerdp_args` for a plain request must not hold
  `+clipboard`, or must hold a direction limit, unless the request asks for
  it. Today the argument is always there. That FreeRDP serves the content
  without a paste is from memory of the cliprdr protocol.
- Severity: security against a hostile guest.
- Confidence: high that the argument is unconditional, medium for what
  FreeRDP does with it.

### F4. A changed media folder is not used while the lab daemon runs

- Where: `src/labd/machine_build.rs:221-229`,
  `src/labd/lab/reload.rs:249-256`, `src/labd/reload_plan.rs:40-58`.
- Fault: the image of a `media {}` block is built once, when the machine is
  built: at daemon start, or when a reload finds that the machine's block
  changed. The path of that image is kept in the machine. A start of the
  machine does not hash the folder again. So after an edit of a file in the
  folder, with the lab file unchanged, `vmlab vm restart` and `vmlab up`
  attach the old image until the whole lab goes down and the daemon is
  replaced. PRD §6.3 says an unchanged folder does not rebuild, which
  promises that a changed one does.
- Proof: a lifecycle test `a_changed_media_folder_reaches_the_next_start`:
  build a lab with one stopped VM and an ISO `media {}` block, change a
  file in the folder, run the reload and start the VM, and assert that the
  CD-ROM path of the launch is not the path from before the change.
- Severity: wrong result.
- Confidence: medium. `reload_plan.rs` has no mention of media, but the
  review did not read how it compares two blocks.

### F5. A change of a file's mode does not rebuild an ISO that records modes

- Where: `src/media/hash.rs:3-6` and `:41-58`, `src/media/iso.rs:46`.
- Fault: the digest covers paths and bytes only. The ISO is built with
  Rock Ridge (`-R`), which records each file's mode, owner and times. A
  `chmod +x install.sh` in the folder changes no digest, so the cache
  answers with the old image, and a Linux guest still finds a script that
  is not executable. Nothing short of a content edit or `rm -r
  .vmlab/media` gets the new mode into the guest.
- Proof: `a_mode_change_rebuilds_an_iso` in `src/media/cache.rs`: `ensure`
  an ISO from a folder with `run.sh` at mode 0644, set it to 0755, `ensure`
  again, and extract `run.sh` from the returned image with `xorriso -osirrox
  on`. Its mode must have the execute bit. Today the second call returns the
  first image.
- Severity: wrong result.
- Confidence: high.

### F6. One media folder that cannot be built stops the whole lab from loading

- Where: `src/labd/lab.rs:540-541`, `src/labd/machine_build.rs:227-232`,
  `src/config/validate.rs:1993-2000`.
- Fault: `LabRuntime::build` builds the media of every VM, and returns the
  first error. Validation checks only that the folder exists. So a floppy
  folder that grew past 1.44 MB, a bad label, or a host without `xorriso`
  or mtools is found at daemon start, and it fails the load of the lab. No
  verb that needs the daemon then works on any machine of the lab: not
  `status`, not `down`, not the start of a machine that has no media. The
  media of a machine that is not about to start are not needed for any of
  these. The same load hashes every media folder in full each time, also
  for a verb that starts nothing.
- Proof: `a_lab_loads_with_an_unbuildable_media_folder` beside the tests of
  `LabRuntime::build`: a lab with two VMs, one with a floppy `media {}`
  folder that holds a 2 MB file. `LabRuntime::build` must succeed, and the
  start of that one VM must fail with the capacity message. Today the build
  of the lab fails.
- Severity: wrong result.
- Confidence: medium. The review did not check each verb for a path that
  works without the daemon.

### F7. An edit during a build is stored under the digest of the old content

- Where: `src/media/cache.rs:44-71`.
- Fault: `ensure` hashes the folder, then the ISO or floppy tool reads the
  folder again. A file that changes between the two reads goes into the
  image, and the image is installed under the digest of the content from
  before the change. A later return of the folder to the old content hits
  that entry, which holds the new content. The entry is never checked
  again, so the error stays until someone removes the cache. Nothing calls
  `fsync` before the rename either, so a host crash can leave a short or
  empty file under a valid name, with the same result.
- Proof: `an_edit_during_the_build_is_not_cached_under_the_old_digest` in
  `src/media/cache.rs`: put a stub `xorriso` first on `PATH` (a script that
  appends a byte to one source file and then runs the real one), `ensure`,
  restore the file, `ensure` again, and extract the file from the returned
  image. It must hold the restored content. The test has to hold the lock
  that the other tests which change `PATH` use.
- Severity: wrong result.
- Confidence: high.

### F8. The detached helper carries the password on its command line

- Where: `src/cli/app.rs:60-65` and `:139-158`.
- Fault: `vmlab app run --detach --password <pw>` starts the helper
  `vmlab __appsession ... --password <pw>`. The helper lives as long as the
  shared app, and every local user can read `/proc/<pid>/cmdline` for that
  time. `src/appshare/rdp.rs:8-11` passes all of FreeRDP's arguments on
  stdin to keep the same credential out of an argv.
- Proof: change `the_detached_helper_gets_the_same_arguments_less_detach`
  (`src/cli/app.rs:188-211`), or add
  `the_detached_helper_argv_holds_no_password`: `helper_args()` of a request
  with a password must not contain the password. Today it does.
- Severity: security against a peer (another user of the host). PRD §1.2
  says vmlab serves one trusted host user, which lowers this.
- Confidence: high.

### F9. The X11 proxy sends the host's real cookie to whoever holds the abstract socket name

- Where: `src/appshare/x11.rs:59-64`, `:417-421`, `:466`.
- Fault: for a local display the proxy tries the abstract socket
  `\0/tmp/.X11-unix/X<N>` before the path socket, and then writes the setup
  prefix with the real `MIT-MAGIC-COOKIE-1`. An abstract name has no owner
  and no mode. When the X server does not listen on the abstract name, any
  local user in the same network namespace can bind it, take the
  connection, and read the cookie of the user's display.
- Proof: `the_real_cookie_goes_to_the_path_socket_only` in
  `src/appshare/x11.rs`: bind a path socket (the "server") and an abstract
  socket of the name the proxy would try, run `proxy` with a
  `HostDisplay` that holds a cookie, and assert that the abstract listener
  receives no connection. The test needs a way to give the proxy an
  endpoint under a temporary directory, which `Endpoint::Local` allows.
- Severity: security against a peer. libxcb is said to try the abstract
  name first too (from memory), so vmlab may be no worse than any X client
  of the host. PRD §1.2 lowers this as it does F8.
- Confidence: high for the code, medium for how many hosts leave the
  abstract name free.
- Fixed in card #732. The proxy connects to the path socket first. It takes
  the abstract name only when the path socket refuses, and only when
  `SO_PEERCRED` says that this user or root listens on it.

### F10. The host cookie is not found for two common Xauthority shapes

- Where: `src/appshare/x11.rs:139-161` and `:221-235`.
- Fault: two cases where an X client on the host finds its cookie and the
  proxy does not. The proxy then forwards with no authorization, the server
  refuses, and no window opens.
  - `DISPLAY=localhost:10` (an `ssh -X` login to the host). The proxy looks
    for an Internet-family entry with the address 127.0.0.1. `xauth` files
    the entry of such a display under `FamilyLocal` with the host name, and
    libxcb looks it up that way for a loopback peer (from memory).
  - An entry whose display-number field is empty. libXau takes it for every
    display (from memory). `pick_cookie` wants the number to be equal.
- Proof: two tests in `src/appshare/x11.rs`.
  - `a_loopback_tcp_display_uses_the_local_entry`:
    `HostDisplay::resolve("localhost:10", ...)` with an Xauthority that
    holds one `FamilyLocal` entry for this host's name and display 10 must
    yield that cookie.
  - `an_entry_without_a_number_matches_every_display`: `pick_cookie` with
    one `FamilyLocal` entry whose `number` is empty must return its data.
- Severity: wrong result.
- Confidence: medium. Both depend on what libxcb and libXau do.

### F11. An RDP request is refused only after the guest was changed

- Where: `src/appshare/mod.rs:314-349`, `src/appshare/rdp.rs:108-127`.
- Fault: `attempt` sends `machine.app_open` first, and builds FreeRDP's
  arguments from the reply after it. `app_option` refuses an argv that
  FreeRDP's comma and quote rules would change (an apostrophe in an
  argument is one case). That refusal depends on the command line alone.
  By the time it is made, the agent has turned Remote Desktop on, changed
  the two RemoteApp policies and may have added the account to Remote
  Desktop Users (PRD §20.3: "leaves them changed"), and the daemon holds an
  open session until its accept grace is over.
- Proof: `an_argv_freerdp_cannot_carry_is_refused_before_the_open` in
  `src/appshare/mod.rs`: run `attempt` in RDP mode with the argv
  `["notepad", "it's.txt"]` against a fake lab client that counts requests.
  The error must come back with zero requests sent. Today one
  `MachineAppOpen` is sent.
- Severity: wrong result.
- Confidence: high.

### F12. The guest can write the two records that only the daemon may write

- Where: `src/labd/app_share.rs:293-311`, `src/appshare/mod.rs:431-449`,
  `guest/agent-proto/src/app.rs:65-72`.
- Fault: the protocol says that an agent never sends `exited` or `failed`.
  The daemon relays the agent's bytes without decoding them, so the CLI
  takes an agent-written `{"record":"exited","code":0}` or `failed` record
  as the daemon's and ends the session with it. The guest program then
  still runs with its connections cut, and `app run` reports an exit code
  or an error text that no part of vmlab produced. A guest already controls
  the exit code of its own program, so the gain for a hostile guest is
  small: a false "the lab daemon says" message.
- Proof: `an_agent_written_exited_record_does_not_end_the_session` in
  `src/labd/app_share.rs`: feed `relay_control` a session whose data holds
  an encoded `AppRecord::Exited`, and assert that the client does not
  receive that record (the relay drops it or fails the session with its own
  `failed` record).
- Severity: wrong result.
- Confidence: high.

### F13. Guest text reaches the terminal with its control characters

- Where: `src/appshare/mod.rs:442-448`, `src/appshare/rdp.rs:444-446`.
- Fault: the text of a `log`, `diagnostic` and (through F12) `failed`
  record is printed with `eprintln!` as it came. A guest can put terminal
  escape sequences into it: move the cursor, change the title, rewrite
  lines that vmlab printed before. `vmlab exec` passes program output
  through too, and that is its purpose. Here the lines are vmlab's own
  status lines with a `[machine]` prefix, and an error line of the CLI.
  The tail of FreeRDP's log is printed the same way, and FreeRDP logs
  strings that the server sent.
- Proof: `log_text_has_no_control_characters` in `src/appshare/mod.rs`:
  `log_text` of a record whose text holds `ESC ] 0 ; x BEL` and a carriage
  return must return text without bytes below 0x20 other than tab. Today it
  returns the text unchanged.
- Severity: cosmetic, with a small security part (a hostile guest forges
  lines on the user's terminal).
- Confidence: high.

## Notes

These have no proving test, or are not faults by the documents.

- N1. X11 forwarding gives the guest the reach of `ssh -X`. PRD §1.2 and
  ADR-0016 accept it. The proxy filters no request. A guest process that
  can read the session cookie in the guest (root, or the login) is a full
  client of the host display.
- N2. `waypipe client` and FreeRDP run as the desktop user with no sandbox
  and parse bytes that the guest wrote. `client_args`
  (`src/appshare/waypipe.rs:26-34`) does not pass `--no-gpu`, so the guest's
  buffers reach the host GPU path of waypipe (from memory of waypipe's
  defaults). FreeRDP gets `/cert:ignore` (`src/appshare/rdp.rs:70`). The
  connection runs over the agent channel to the guest's own loopback, so
  there is no third party to guard against, and the guest is the server.
- N3. Nothing bounds the connections of one session. Each `pending` record
  makes the CLI start a task and connect (`src/appshare/mod.rs:438-441`,
  `:475-487`), and each connection makes the daemon open an agent channel
  (`src/labd/app_share.rs:424-464`) and, on refusal, queue a note on an
  unbounded channel (`:265`, `:461`). A guest that writes `pending` records
  without a parked connection makes both processes work and hold
  descriptors until the open timeout of each. PRD §1.2 asks that
  guest-to-host channels bound resource use. No document gives the bound,
  so there is no test to name.
- N4. A guest connection that never sends its X11 setup holds its task for
  the whole session (`src/appshare/x11.rs:322-327`). The count is bounded
  only as in N3.
- N5. `--detach` gives the helper null stdio (`src/cli/app.rs:153-157`).
  The check of the host session runs before the detach. Every later
  refusal is lost: a wrong login, a guest without waypipe and no fallback,
  FreeRDP's failure and the path of its log. The command prints "sharing
  ... in the background" and exits 0.
- N6. After a failed RDP session the private directory with `freerdp.log`
  is kept under `$XDG_RUNTIME_DIR` and never removed
  (`src/appshare/rdp.rs:431-439`). Without `--verbose` FreeRDP's whole
  output goes to that file for the length of the session, with no size
  limit, on a file system that is memory.
- N7. `HostEnv::detect` reads the host config with `unwrap_or_default()`
  (`src/appshare/mod.rs:125`). A host config with an error is ignored
  without a word, and the binaries come from `PATH`, not from the `waypipe`
  and `freerdp` settings. `on_path` (`:139-144`) takes a file that is not
  executable.
- N8. The daemon binds each session socket and sets mode 0600 after it
  (`src/labd/app_share.rs:279-290`). Between the two calls the mode is that
  of the umask. The review did not check the mode of the run directory.
- N9. The media cache is never pruned. `MediaCache::gc` is dead code
  (`src/media/cache.rs:75-101`), so each edit of a media folder leaves one
  more image in `.vmlab/media`, and a build that the daemon did not finish
  leaves its `.tmp-<pid>-...` file.
- N10. The temporary name holds the process id and the entry name only
  (`src/media/cache.rs:52-58`). Two builds of one entry in one process
  would write one file. Today the daemon builds media one after the other
  under the state lock, so this needs a new caller to matter.
- N11. The hash follows links (`src/media/hash.rs:79`). A link to a
  directory above itself is walked until the kernel's link limit and then
  fails as "opening ... for hashing". Two such links in one folder make the
  walk grow as a power of two. A FIFO in the folder blocks `File::open`
  (`:46`) until a writer comes, and a link to `/dev/zero` never ends. All
  three hang the daemon's start. Only the lab's author can make them, and a
  test that proves a hang needs a time limit.
- N12. File names that are not UTF-8 are hashed through
  `to_string_lossy` and then opened by the lossy name
  (`src/media/hash.rs:71-73`, `:40`), so such a folder fails with "No such
  file".
- N13. `check_capacity` (`src/media/floppy.rs:82-113`) counts clusters only.
  It does not count the 224 entries of the root directory, the extra
  entries of long names, or a directory with more entries than one cluster
  holds. Such a folder passes the check and fails in `mcopy` with mtools'
  own message. Two names that differ only in case make `mcopy` ask what to
  do. With no terminal it copies the first file and exits 1 (run once on
  helios with `setsid` and an empty stdin), so the build fails and does not
  hang.
- N14. The label is checked only at build time (`src/media/iso.rs:74-86`,
  `src/media/floppy.rs:118-133`), not by `vmlab validate`. A lower-case
  floppy label and its upper-case form make two cache entries with the same
  image.
- N15. Joliet names longer than 64 characters are shortened by the ISO
  tool without an error (from memory), so a Windows guest can see another
  name than the folder has.
- N16. A built image has the mode that the tool and the umask give it,
  0644 as a rule. An unattend ISO holds the passwords of its answer file,
  and the source folder is as readable as the image.
- N17. An X11 `DISPLAY` of the old form `host/unix:N` is taken for a TCP
  host of that name (`src/appshare/x11.rs:59-78`).
