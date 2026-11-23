# Review: template build and seal

Card #673. A bug-hunting review of `vmlab template build`: the build VM, the
fetch of the source artefact, the inputs of an unattended install, the agent
install and its verification, the seal, and the artefact cache. It lists
faults and makes no fix. Reviewed at `next` commit `a07dd4a9`. All paths are
from the repository root, and all line numbers are from that commit.

The store, the registry client and the variant install were reviewed by card
#440 (`docs/reviews/templates-registry.md`). This review does not repeat those
findings. Its note N11 (paths of a definition that leave the template root)
is still open and is extended in N1 below.

Severity words: **data loss**, **crash** (of a host process), **security**
(against a hostile guest or a peer), **wrong result**, **cosmetic**.

## Who is trusted

- A template definition is as trusted as a build script. Its `provision`
  scripts are wscript that runs on the host, and on an architecture that
  QEMU still runs its `qemu_args` reach the emulator. A definition from a
  shared repository is therefore code of that repository, and the user who
  builds it runs that code. A path or a command in a definition is not a
  privilege gain by itself. A fault that needs a hostile definition is
  ranked by what an honest mistake in a definition does.
- The server behind a `url` source is not trusted. The definition pins the
  content with `sha256`, and nothing else about the answer.
- The build guest is not trusted after it has run an installer from the
  network.

## How the findings were checked

Each finding is from reading the code. One fact was also measured on helios,
with a scratch program that uses the standard library only (no vmlab code, no
VM started, its directory removed after the run): the path that
`build_workdir` forms for `version = "1/../../../../victim"`, given to the
three calls of `claim_workdir`, deletes the content of a directory outside
`cache/builds` and leaves `victim.lock` beside it (F1).

No other finding was run. Each names the test that proves it. A finding whose
test does not reproduce is wrong and should be dropped.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | data loss | The build's working directory is named from an unchecked `name` and `version`, and is deleted recursively before any name check runs (fixed, card #691) |
| F2 | security (peer) | A `url` source is downloaded with no size limit, over plain HTTP if the definition or a redirect says so, and is checked only when it is complete |
| F3 | wrong result | The seal accepts a guest that was cut off: the stop ladder quits the emulator after about a minute and the build seals that disk |
| F4 | wrong result | A version whose variants are not built hides the complete version below it, and `template prune` keeps the unusable one |
| F5 | wrong result | A build VM outlives a killed supervisor: the next start deletes its working directory and leaves the process |
| F6 | wrong result (hang) | Two builds of one `<arch>/<name>` with different declared versions share one runtime directory |
| F7 | wrong result | `template stop` during the seal is ignored: the version is installed and the build reports `done` |
| F8 | wrong result | The sealed `agent_version` is the stamp of the staged agent, not the version of the agent that answered |
| F9 | wrong result | The artefact cache serves a compressed file as the decompressed image after one digest was fetched under two URL spellings |

### F1. The working directory is named from an unchecked name and version (data loss)

**Fixed by card #691.** `build_template` applies the store's name rule to the
architecture, the name, the declared version and a pinned version before it
plans, fetches or claims anything (`check_build_names`), and to the
auto-incremented version before the working directory is claimed. Both proof
tests are in `src/template/build.rs`. `vmlab validate` still accepts such a
name: the refusal comes at the start of the build.

- Where: `src/template/build.rs:977-979` (`build_workdir`), `:153-154` (the
  claim), `:952-970` (`claim_workdir`, the delete at `:964`), `:907-913` (the
  delete when the guard drops). `src/config/extract.rs:948-953` (the name is
  the block label, the version any string). `src/config/validate.rs:712`
  (the only rule: the version is not empty). `src/template/trust.rs:6-27`
  (the rule of the store), applied at `src/template/store.rs:437`.
- Why it is a fault: `build_workdir` joins
  `"<arch>-<name>-<version>"` onto `cache/builds`. The name and the version
  come from the definition as written. `claim_workdir` creates the parent,
  creates `<work>.lock`, and calls `remove_dir_all(work)`. The store's name
  rule (`[a-z0-9][a-z0-9._-]*`, no `..`) is applied by `store.install`, which
  is the last step of the build.
  Two consequences:
  1. A version or a name with `/` and `..` moves the working directory out of
     the cache. `version = "1/../../../../victim"` makes `claim_workdir`
     delete the directory `victim` four levels up, before anything else runs
     (measured, see above). The guard deletes it again at the end.
  2. An honest definition with a name or a version that the store refuses,
     for example `version = "24H2"` (upper case) or `name = "Windows-11"`,
     passes `vmlab validate`, downloads its source, installs the guest and
     seals the disk. Then `store.install` fails with `invalid template path
     component`, and the whole build is lost.
  The variant name is checked before the build (`plan_variants`, `:215`),
  the name and the version of the template are not.
- Proof:
  1. Unit test `a_build_working_directory_stays_inside_the_builds_directory`
     in `src/template/build.rs`: parse a definition with
     `version = "1/../../x"`. Assert that `build_template` fails before
     `claim_workdir`, or that the normalised `build_workdir(&def)` starts
     with `builds_dir()`. Today the path leaves the directory and nothing
     refuses it.
  2. Unit test `a_name_the_store_refuses_is_refused_before_the_build`: a
     definition with `version = "24H2"` and a `path` source that does not
     exist. Assert that the error names the version. Today the error is
     `source file ... does not exist`, which shows that the fetch ran first.

### F2. A `url` source has no size limit and may be fetched over plain HTTP (security)

- Where: `src/template/artefact.rs:212-240` (`download`), `:37-90` (the digest is
  compared after the download is complete).
  `src/config/extract.rs:1148-1167` (a `url` needs `sha256`, and no rule
  names a scheme).
- Why it is a fault: `download` writes the body to the cache until the
  server ends it. There is no limit from `Content-Length`, from the
  definition, or from the free space. The `sha256` is compared when the
  stream has ended. The client follows redirects, and both the URL and a
  redirect target may be `http://`. A mirror, or anyone on the path of a
  plain HTTP request, can therefore send data without end. The cache is
  under the data directory, on the filesystem that holds the template store
  and usually the disks of running labs. When it is full, every running
  guest gets write errors on its disk. The read timeout
  (`DOWNLOAD_LIMITS`, `:205-208`) does not help, because the server keeps
  sending.
- Proof: test `a_download_that_never_ends_is_stopped_at_a_limit` in
  `src/template/artefact.rs`, with the loopback mirror of the existing tests:
  the mirror answers with no `Content-Length` and sends blocks until the
  client closes. Give `resolve_in` a limit of a few megabytes (a new field of
  `DownloadLimits`). Assert an error and a `.part` file not larger than the
  limit. Today there is no such limit, and the test's mirror would write
  until the test's disk is full, so the test has to be written with the
  limit. A second assertion: a `http://` URL is refused or warned about by
  `extract_source`.

### F3. The seal accepts a guest that was cut off (wrong result)

- Where: `src/template/build.rs:1299-1305` (base), `:575-583` (variant),
  `:1371-1379` (verification boot). `src/labd/vm.rs:1745-1827`
  (`stop_ladder`). `src/labd/stop_plan.rs:30` (`RUNG_EXIT_WAIT`, 30 s),
  `:93-126` (`plan_stop`).
- Why it is a fault: the seal logs `sealing: graceful shutdown` and calls
  `vm.stop(false)`. That is the stop ladder of a lab machine: the agent's
  shutdown, the ACPI button, `quit` of the emulator, then SIGKILL. Each rung
  waits 30 seconds when the profile has `acpi_shutdown`. The ladder returns
  `Ok` at whichever rung ends the process, and does not say which. So a
  guest that needs more than about a minute to shut down is ended by `quit`,
  and the build flattens and installs that disk. A Windows guest that
  installs updates at shutdown, and a Linux guest with a slow unit, both
  take longer. The sealed image then has a file system that was not
  unmounted, and every clone starts from it. The only trace is a `warn` line
  in the supervisor's log (`guest ignored shutdown, quitting the emulator`,
  `src/labd/vm.rs:1805`). The build log does not have it.
  For a lab machine the ladder is right: `stop` must end. For a seal, a
  forced end is a failed build, except for the guests that never power off
  by themselves (DOS, Windows 9x), which the profile can name.
- Proof: test `a_seal_fails_when_the_guest_does_not_power_off_by_itself` with
  the fake hypervisor (`src/labd/hypervisor/fake.rs`): a machine whose guest
  ignores the agent's shutdown and the ACPI button. Assert that the seal step
  returns an error, or that `stop` reports the rung and the build refuses
  `quit` and `kill`. Today `vm.stop(false)` returns `Ok(())` after the `quit`
  rung, and no value tells the caller.

### F4. A version without its variants hides the complete version below it (wrong result)

- Where: `src/template/build.rs:156-184` (the base is installed, then the
  variants are built one at a time), `:379-427`.
  `src/template/store.rs:302-319` (`resolve_version`: the highest version).
  `src/template/meta.rs:213-225` (`select_variant` refuses).
  `src/supervisor/store.rs:365-387` (`superseded`).
- Why it is a fault: a definition with variants installs its base as version
  N+1 first, marked `variants_expected`. Each variant build then boots a VM
  and can take as long as the base did. From the install of the base until
  the default variant is installed, a reference without a version
  (`x86_64/win`) resolves to N+1 and is refused with `its variants are not
  built yet`. The complete version N is still in the store. Every `up` that
  has to clone the template fails in that window. When the variant build
  fails, is stopped, or the supervisor is killed, the state stays until
  someone builds the variant or removes N+1. The next plain
  `template build` does not repair N+1. It makes N+2.
  `template prune` makes it worse. `superseded` keeps the newest `keep`
  versions of a family by version order alone. With the default `--keep 1`
  (`src/template/cli.rs:121`) it removes the complete version N and keeps the unusable N+1, unless a
  clone holds N.
- Proof:
  1. Store test `an_unversioned_reference_skips_a_version_without_its_default`
     in `src/template/store.rs`: install `win@1.0.0` with a default variant,
     then a base `win@1.0.1` with `variants_expected` and no variant. Call
     `resolve("x86_64", "win", None, None, Purpose::Lab)`. Today it is an
     error. It should resolve to `1.0.0`, or the build should not make the
     base visible before its default variant.
  2. Unit test `prune_keeps_the_newest_usable_version` in
     `src/supervisor/store.rs`: `superseded` over the same two versions with
     `keep = 1`. Today it returns `1.0.0`.

### F5. A build VM outlives a killed supervisor (wrong result)

- Where: `src/qemu/process.rs:150` (`kill_on_drop(false)`, and no parent
  death signal). `src/supervisor/mod.rs:197-206` (the sweep at start).
  `src/template/build.rs:994-1017` (`sweep_workdirs_in`).
  `src/supervisor/templates.rs:517-547` (`stop_all`, the only place that
  kills the processes of a build lab).
- Why it is a fault: the build VM runs under the supervisor, in a lab that
  is not in the registry. When the supervisor is killed (SIGKILL, the OOM
  killer, a crash), the hypervisor and its `swtpm` keep running. The lock on
  the working directory is released with the dead process. The next
  supervisor sweeps the working directory, so it deletes the disk and the
  media under the running VM, and it kills nothing: `kill_lab_orphans` runs
  for registered labs (`src/supervisor/mod.rs:745-781`) and in `stop_all`,
  which a killed supervisor never reached. The orphan keeps its memory, its
  CPU and the space of the deleted disk. It also keeps the sockets in the
  runtime directory of `build-<arch>-<name>`, where the next build of that
  template starts its VM (see F6 for what a shared runtime directory does).
- Proof, by steps: build a `scratch` template whose provision waits. Send
  SIGKILL to `vmlabd`. Start `vmlabd`. The log says `cleared 1 build working
  directory`, and `pgrep -f build-<arch>-<name>` still lists the hypervisor.
  As a test: `a_start_reaps_the_processes_of_a_leftover_build` in
  `src/supervisor`, with a process table that holds one command line of a
  build lab and a leftover directory in `cache/builds`. Assert that the
  sweep returns the lab to kill. Today the sweep has no such output.

### F6. Two builds of one template share one runtime directory (wrong result, hang)

- Where: `src/template/build.rs:1551-1553` (`build_lab_name`: architecture
  and name), `:977-979` (the working directory: architecture, name and
  declared version). `src/supervisor/templates.rs:43-88` (one operation per
  lab, architecture and template). `src/paths.rs:140-147` and
  `src/labd/vm.rs:163-175` (the sockets and the logs of a VM are under the
  lab name). `src/labd/build_lab.rs:155-178`.
- Why it is a fault: `try_begin` allows two labs to build the same
  `<arch>/<name>` at the same time (its own test
  `an_ambiguous_stop_is_refused` does it). When the two definitions declare
  the same version, the second build is refused at `claim_workdir`. When
  they declare different versions, for example two checkouts or two git
  worktrees of one repository on different branches, both builds run. Both
  VMs are `build-<arch>-<name>/build`, so both use one set of control,
  agent and display sockets, one serial log and one event history. The
  comment on `build_lab_name` describes this failure for two architectures
  (2026-09-03): the second hypervisor binds the sockets again, the first
  build loses its control channel and waits out its budget. The second
  build also prints `lab ... is already running under its own daemon`, which
  is not what happened.
- Proof: test `two_builds_of_one_template_do_not_share_a_runtime_directory`
  in `src/template/build.rs`: two definitions `t` for `x86_64` with versions
  `1` and `2`. Assert that their working directories and their lab names
  both differ, or that the second claim is refused. Today the working
  directories differ and `build_lab_name` is equal, and
  `TemplateOps::try_begin("lab-a", ..)` and `("lab-b", ..)` both succeed.

### F7. `template stop` during the seal is ignored (wrong result)

- Where: `src/template/build.rs:1316-1318` (the last cancel check of the
  base build), `:1392-1450` (flatten, hash, install), `:590-592` and
  `:600-634` (the same in a variant build).
  `src/supervisor/templates.rs:493-498` (the build reports `done`).
- Why it is a fault: after the guest has shut down, the build converts the
  working disk, hashes it and installs it. None of these steps looks at the
  cancel token. For a large image they take minutes. A `vmlab template stop`
  in that time answers `stopping`, and the build then installs the version
  and emits `template.op.done`. For a definition with variants the cancel
  is seen at the next variant (`:380-382`), which leaves the state of F4:
  a new base without variants above a complete version.
- Proof, by steps: build a `scratch` template with a large disk, wait for
  the log line `flattening sealed image`, run `vmlab template stop <name>`,
  then `vmlab template list`: the version is installed. As a test:
  `a_stop_during_the_seal_installs_nothing`, with
  `qimg::testing::pause_next("convert")` (`src/template/qimg.rs:544-549`) to
  hold the flatten, the cancel sent while it is held, and the store empty
  afterwards. The seal has to be a function of its own for that test.

### F8. The sealed `agent_version` is not the version of the agent that answered (wrong result)

- Where: `src/template/agent_install.rs:96-108`.
  `src/labd/agent_repair.rs:602-640` (`installed_stamp`, `decide`).
- Why it is a fault: `verify` waits for a handshake and then records the
  stamp of the agent on the bootstrap ISO, chosen by the guest's OS. It does
  not compare that stamp with `info.agent_version`. A `qcow2` or layered
  source that already carries an agent answers the handshake with the old
  agent, whether or not the definition's install hook ran. The metadata
  then says that the template holds the host's current agent. The damage is
  bounded for an agent that reports a stamp, because `decide` reads the
  handshake first. For an agent from before the stamp, `installed_stamp`
  falls back to the sealed value, finds it equal to the shipped one, and
  answers `Current`: that agent is never refreshed. On any host,
  `template list` and a push state a version the image does not hold.
- Proof: test `the_sealed_version_is_the_one_the_agent_reports` in
  `src/template/agent_install.rs`, with a mock machine whose handshake says
  `agent=old` and a staged ISO with `agent=new`. Today `verify` returns
  `Some("agent=new")`. It should fail the build or record `agent=old`.

### F9. The cache serves a compressed file as the decompressed image (wrong result)

- Where: `src/template/artefact.rs:37-90`.
- Why it is a fault: the cache file is `<sha256>.artefact` in both
  branches. For an URL that ends in `.xz` or `.gz` it holds the decompressed
  image, and `<sha256>.ok` says that it may be used without a check. For any
  other URL it holds the download, and is checked against the digest. When
  the same file is named by two URL spellings, one with the suffix and one
  without (`.../img.qcow2.gz` and `.../img.qcow2.gz/download`), the second
  kind finds the decompressed image, fails its check, downloads, and puts
  the compressed bytes at `<sha256>.artefact`. The marker stays. The next
  build with the first URL takes the compressed bytes as its disk or its
  ISO, with the log line `using cached artefact`. It stays wrong until
  someone deletes the cache files.
- Proof: test `one_digest_under_two_url_spellings_keeps_two_cache_entries`
  in `src/template/artefact.rs` with the loopback mirror: serve a gzip file
  at `/a.img.gz` and at `/a`. Call `resolve_in` for the first, the second,
  and the first again. Assert that the third result decompresses nothing and
  still holds the image. Today it holds the gzip bytes.

## Notes

No proving test, or not a fault by itself.

- N1. Paths of a definition are joined to the template root with no check,
  and an absolute path replaces the root: `first_boot` and
  `secure_boot_vars` (`src/template/build.rs:1404-1411`, `:1432-1435`,
  `:602-609`, note N11 of card #440), and also a `path` source (`:1531-1534`),
  `disk { from }` (`:2323`), `media { from }` (`:2333`), a provision script
  (`:2378`) and a playbook directory (`:2382`). The media builder follows
  symlinks (`src/media/iso.rs:27`). So a definition can put any file the
  user can read into the guest, into the sealed image, or into the metadata
  that a push publishes. By the trust rule above this is not a privilege
  gain. It matters to a user who reads a definition before a build and
  expects that it stays inside its directory.
- N2. Nothing is signed. A template is identified by the `sha256` of its
  disk in its own metadata, and the build records no signature and no
  provenance beyond `origin` (the URL or the source reference,
  `src/template/build.rs:2040-2047`). An URL in `origin` that holds a token
  in its query string is written to `template.wcl` and pushed.
- N3. vmlab does not generalize. The seal is a shutdown and a flatten
  (`src/template/build.rs:1392-1400`). Machine identity (machine-id, SSH
  host keys, the Windows SID) and anything the installer left on the disk
  (an answer file with a password, a shell history, a DHCP lease) are sealed
  unless the definition's own steps remove them. The PRD says so for Windows
  (§6.6). I found no place that says it for a Linux template.
- N4. Login passwords of a definition go into `template.wcl` in the clear
  and into the config blob of a push. The PRD states this (§19.2).
- N5. The synthetic lab file of a build holds every playbook variable in the
  clear, the `secret = true` ones too (`src/template/build.rs:2396-2404`,
  written at `:1097` and `:496` with the default file mode). It is removed
  with the working directory, and after a kill only at the next start of a
  supervisor. I did not check the mode of the data directory.
- N6. The serial log, the hypervisor log and the event history of a build
  are under the state directory of the lab `build-<arch>-<name>`
  (`src/labd/vm.rs:163-175`, `:990`). The build removes its working
  directory and not these. An unattended installer can print secrets on the
  serial console.
- N7. A cancelled build does not stop a convert that is running.
  `cancelable` (`src/template/build.rs:1512-1523`) drops the future, and the
  convert runs on the blocking pool (`src/template/qimg.rs:196-212`) until
  it is done, into a file that the guard has already unlinked. It uses disk
  space and I/O for that time. With `VMLAB_IMAGE_TOOL=qemu-img` the
  `qemu-img` process does the same.
- N8. `sha256_file` is called on an async worker (`:1400`, `:601`), so one
  worker of the supervisor's runtime is blocked for as long as the hash of
  the whole image takes. The convert before it uses the blocking pool.
- N9. A variant build boots an overlay on the base disk in the store
  (`:552-558`). I did not check whether `template rm` of that version sees
  the build as a user of the disk. The build's working directory is not a
  registered lab. If it does not, the result is a failed variant install,
  because the installer checks the base again.
- N10. The working directory of a variant build is named from the declared
  version, not from the version it builds onto (`:400-403`). Two
  `--variant` builds of one variant onto two installed versions exclude
  each other with `in use by another build`.
- N11. A decompressed artefact is not bounded either (`src/template/artefact.rs:175-193`).
  The compressed file is pinned by the definition's own `sha256`, so only
  the definition can make it a bomb.
- N12. An agent asset with no `VERSION` file is sealed as
  `agent_version = "unknown"` (`src/agent_asset.rs:146-149`). `decide` then
  compares that word with the shipped stamp.

## Checked and found sound

- A first-boot script of a layered source runs on the host during the build
  (`src/template/build.rs:1069`, `:1207-1219`). It can only be in the store
  if a pull or an import accepted it (`src/template/trust.rs:30-38`), so the
  build adds no new trust.
- The agent binaries on the bootstrap ISO come from the host's asset
  directories only (`src/agent_asset.rs:130-137`). A definition cannot name
  another one.
- A `qcow2` source with a backing file is refused before the convert
  (`src/template/build.rs:1486-1505`).
- The working directory is held by a lock outside it, a second build of the
  same directory is refused, and the sweep leaves a directory whose lock is
  held (`:931-970`, `:998-1017`). The lock file is not inherited by the
  hypervisor.
- A failed or cancelled boot, provision or shutdown forces the build VM down
  before the working directory goes (`:1308-1314`, `:586-589`,
  `:1383-1389`). The background agent wait is aborted on every way out
  (`:1803-1809`).
- The build lab's socket serves inspection and input only
  (`src/labd/build_lab.rs:32-105`), and the supervisor refuses to start a
  lab with the name of a running build (`src/supervisor/mod.rs:544-549`).
- An artefact download is written under a name of its own and gets its
  digest name only after the check (`src/template/artefact.rs:103-143`).

## What I read and what I did not

Read in full, without their test modules:

- `src/template/build.rs` (to line 2413), `artefact.rs`, `agent_install.rs`,
  `trust.rs`.
- `src/supervisor/templates.rs`, `src/labd/build_lab.rs`,
  `src/labd/stop_plan.rs`.

Read in part:

- `src/template/bootstrap.rs` (the staging, and the first 80 lines of
  `bootstrap/install.sh`), `qimg.rs` (the entry functions and both tools'
  convert), `store.rs` (`install_with`, the version resolution), `meta.rs`
  (`select_variant`), `cli.rs` (the names of the build verbs only).
- `src/config/extract.rs` (`extract_template`, the source block),
  `src/config/validate.rs` (the template rules).
- `src/labd/vm.rs` (`VmDirs`, `stop_ladder`), `src/labd/agent_repair.rs`
  (`eligible`, `decide`), `src/agent_asset.rs`, `src/guest_asset.rs`,
  `src/qemu/process.rs` (the spawn and `kill_lab_orphans`),
  `src/supervisor/mod.rs` (the sweep at start and the orphan reaping),
  `src/supervisor/store.rs` (`prune`, `superseded`).
- `hv/crates/vmlab-disk/src/ops.rs`: the start of `convert` only.

Not read:

- The other install scripts under `src/template/bootstrap/` (Windows, NT,
  9x, DOS, macOS). They run inside the guest with the highest rights there.
- `src/media/` beyond the fact that it follows symlinks, and the cache of
  built media.
- `LabRuntime::build` and `up` as a build uses them: the pre-provision hook,
  the provisioning engine, the playbook runner and how it masks secrets.
- `src/template/store.rs` beyond the parts named, `store_view.rs`,
  `catalog.rs`, `registries.rs`, `oci_bridge.rs`: covered by card #440.
- Every test module. A finding here may already have a test that I did not
  see. The named tests are new names.
- No build was run. The timing claims of F3 (how long a guest takes to shut
  down) are from experience with those guests, not from a measurement.
