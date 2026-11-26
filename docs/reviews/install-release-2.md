# Second review: install, update and release scripts

Card #1090, 2026-11-26. A second fault-finding pass over how vmlab is
installed, updated and released. It lists faults and makes no fix. Reviewed at
`next` commit `8e2c3a12`. All paths are from the repository root, and all line
numbers are from that commit.

The first pass is `docs/reviews/install-release.md` (card #581, findings F1 to
F11, notes N1 to N10). This document continues its numbering, so a code names
one finding across both: F12 to F18 and N11 to N22 are new here. A finding of
the first pass is named again only where its fix is incomplete, and the text
says which (F13 for F3, F14 for F1, F17 for F5).

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**. The trust rules are those of the
first pass too ("Who is trusted"). The order is by how far a fault reaches:
the two that touch every release first, then the installer.

## How the findings were checked

Every finding is from reading the code. Five were also run in the review
sandbox, and one was measured on the checkout itself. No probe is committed.

The installer probes ran `sh install.sh --version 1.2.3` against a fixture
release made with `scripts/release-assets.py prepare`, with a stub `curl` on
`PATH` that copies fixture files, as `scripts/test_release_assets.py` does.
Each printed what the installer does today:

- F14: stubs for `sudo` (runs its arguments), `setcap` and `chown` (write
  their arguments to a log), and a `stat` that answers `0:50` to
  `stat -c %u:%g ..`. The installer ran as uid 1000. The log held
  `chown 0:50 ./vmlab-fastpath` and then the `setcap` line, the exit code was
  0, and the installer printed `Granted: cap_bpf,cap_net_admin+ep`.
- F15: an old guest directory with the bundle layout, and an `mv` stub that
  fails when its last argument is the guest directory. Exit 1, and neither
  the old nor the new guest directory existed afterwards. A second run with
  an `mv` stub that sends `SIGTERM` to the installer after the first rename:
  exit 143, and again no guest directory.
- F17: an old `vmlab-fastpath` in the bin directory, and the helper download
  failing with curl's exit code 28 (a timeout). Exit 0, the new `vmlab`
  installed, the old helper removed, and the warning `this release gave no
  vmlab-fastpath-1.2.3-linux-x86_64`. `SHA256SUMS` of the fixture names that
  asset. A second run on a key-signed fixture with the download of
  `SHA256SUMS.key.bundle` failing the same way: exit 1 with `release has no
  signature bundle. An unsigned release installs only with
  --insecure-skip-signature`.
- F18: a full run without `--skip-checks` on a host with no QEMU, no
  `/dev/kvm` and no vmlab-hv. The report listed `qemu-system-x86_64   run
  x86_64 guests`, printed `/dev/kvm is missing: guests will run under
  emulation, which is very slow.`, and did not contain the word `vmlab-hv`.
- F13: `git ls-files --others --directory --no-empty-directory
  --exclude-per-directory=.gitignore` (the check of
  `scripts/release-local.sh:99`) printed nothing with a planted file
  `.ci-cache/x/planted` in the checkout. `cache_key` of `build/hv.py` takes
  `version`, `sha256` and `patches` only.
- F16: on this checkout of `next`, `scripts/release-version.sh --pre-release`
  printed `version=0.43.0-alpha` and `last_tag=v0.42.1-alpha`, and
  `--release` printed `version=0.42.1`. The newest tag of the checkout is
  `v0.49.0-alpha`, and `git merge-base --is-ancestor` says that
  `v0.46.1-alpha` to `v0.49.0-alpha` are not reachable from `HEAD`.

F12 was not run: the sandbox has no docker.

## Findings

### F12. A release builds and signs in whatever image the tag names at that moment (security, wrong result)

`scripts/release-local.sh:171` rebuilds the image with `just buildbox::image`,
which is `docker build ... -t vmlab-build .` (`.just/buildbox/mod.just:25`).
After that every step names the image by that tag and docker resolves the tag
again each time:

- `scripts/release-local.sh:178` and `:201` start `build/run.sh`, which runs
  `docker run ... "$IMAGE"` with `IMAGE=vmlab-build` (`build/run.sh:31`,
  `:89-90`).
- `scripts/release-local.sh:230` signs through `scripts/release-cosign.sh`,
  which runs `docker run ... -e VMLAB_RELEASE_COSIGN_KEY -e COSIGN_PASSWORD
  "$IMAGE" cosign` (`scripts/release-cosign.sh:33`, `:110-116`).
- `scripts/release-local.sh:200` reads the image id once, between the display
  build and the main build, and `:226-227` writes that id into
  `provenance.json` as `buildImage`.

The tag is one name for the whole docker host. `just buildbox::image`,
`buildbox::dist` and `buildbox::shell` from any other checkout move it to an
image built from that checkout's `build/Dockerfile`
(`.just/buildbox/mod.just:24-35`). A cold release build takes 14 minutes
(`docs/agents/release-gate.md:113`), and the signing step comes after it. A
card branch built on the same host in that time changes the image under the
release. Then:

- the assets are compiled by another toolchain image than the one
  `provenance.json` names, or the provenance names the other checkout's image
  (when the tag moved between line 171 and line 200),
- the release key and its password go into the environment of a container of
  that image, and the `cosign` that runs there is the one its Dockerfile
  installed. The first pass rules that code of a card branch is not trusted to
  change a release, and `docs/agents/release-gate.md:219-222` says that the
  signing container is the one of the release.

`--image-as-is` skips line 171 and takes the tag as it is, which the help text
allows for a test build only (`scripts/release-local.sh:33-35`). Nothing
refuses it for a real version.

Test: in `scripts/test_release_workflow.py`, extend the docker stub so that
`image inspect -f '{{.Id}}' vmlab-build` prints `sha256:<A>` and every `run`
writes its image argument to a log. Run `release-local.sh --version 9.9.9
--signing key` with a stub key. Expected: every `docker run` of the release,
the signing one included, names `sha256:<A>`, the id that `provenance.json`
holds. Observed (by reading): each names `vmlab-build`. A second case: the
stub answers `sha256:<B>` from the first `run` on, as after a retag. Expected:
the release stops. Observed (by reading): it builds and signs in `<B>` and the
provenance says `<A>`.

### F13. The hypervisor bundle takes built binaries from a cache in the checkout (security, wrong result). F3's fix is incomplete

F3's fix gives a release fresh docker volumes. The first pass recorded that
the caches inside the checkout are not covered. One of them puts binaries into
a signed asset unchecked.

`build/dist.sh:123` runs `just ci::hv-package`, which depends on
`hv-virgl-build` (`.just/ci/mod.just:161`). `build/hv.py:96-101` reuses the
directory `.ci-cache/virglrenderer-<version>-<sha256>-<patch digest>/` when it
holds a file `.complete`. The only checks are `pkg-config --exact-version` and
that `libexec/virgl_render_server` exists. `build/hv.py:195-197` then copies
`virgl_render_server` and every `libvirglrenderer.so*` from that directory
into `vmlab-hv-<version>-linux-x86_64.tar.gz`, and `:262-264` writes the
sha256 of whatever it copied into the bundle's `virglrenderer-BUILDINFO`.

`.ci-cache/` is ignored by the tracked `.gitignore` (`.gitignore:32`), so the
untracked-file check of `scripts/release-local.sh:99-118` (F7's fix) does not
see it, and no step of a release removes it. Two faults follow:

- The key holds the version, the source hash and the patches
  (`build/hv.py:32-34`). It does not hold `VIRGL_MESON_OPTIONS`
  (`build/hv.py:23`, from `build/hv-pins.toml`) and not the build image. A
  commit that changes the meson options, or a rebuilt image with another
  compiler, ships the libraries of the earlier build, and the provenance names
  the new commit.
- Anything that ran earlier in that checkout as the build account (a test, a
  `build.rs`, `just ci::check` of another commit checked out there) can leave
  a `.complete` directory with its own `libvirglrenderer.so.1` and render
  server. The next release signs them. `vmlab-hv` loads that library in the
  process that runs every x86 guest.

The download caches under `guest/.cache/` are not part of this finding: each
script checks the cached file against its pinned sha256 on every use
(`guest/build-asset.sh:120-121`, `guest/fetch-firmware.sh:107-108`,
`guest/build-ovmf.sh:33`, `guest/build-seabios.sh:18`).

Test, without docker: in `build/test_hv.py`, make a cache directory under a
temporary `VMLAB_TARGET_ROOT` named by `CACHE_KEY`, with `.complete`, a
`lib/pkgconfig/virglrenderer.pc` of the pinned version, and a
`libexec/virgl_render_server` that holds the text `planted`. Call
`virgl_build()`. Expected: it refuses or rebuilds, because nothing proves the
content. Observed (by reading): it returns, and `package` would copy the file.
A second case: assert that `cache_key` changes when one meson option changes.
Observed: `cache_key` has no such parameter. For the release as a whole, in
`scripts/test_release_workflow.py`: plant `.ci-cache/x/planted` in the fixture
checkout and run `release-local.sh`. Expected: a refusal that names it, as for
an untracked file. Observed: the check lists nothing (run, see above).

### F14. The grant hands the capable helper to the owner of the bin directory (security, wrong result). Brought in by F1's fix

`install.sh:446` in the root script of the grant runs
`chown "$(stat -c %u:%g ..)" ./vmlab-fastpath`: the owner and group of the bin
directory, not the account that ran the installer and consented. The two
differ whenever the installing user can write a directory they do not own, for
example a group-writable `/usr/local/bin` (`root:staff`, mode 2775) given
with `--bin-dir`, or a shared tools directory of a team.

- Directory owned by root: the helper ends as `root`, mode 0700
  (`install.sh:448`), with the capabilities. The installing user cannot
  execute it, so their daemons fall back to the userspace fabric. The
  installer prints `Granted: ...` (`install.sh:497`). Before the grant the
  user owned the helper and could run it.
- Directory owned by another account X: X now owns a helper with CAP_BPF and
  CAP_NET_ADMIN that X never asked for, and the helper serves X
  (`src/net/fastpath/helper/mod.rs:73-74`: the owner of the file is served).
  The consent text says the helper "is installed for you alone"
  (`install.sh:395`).

Test: in `scripts/test_release_assets.py`, beside
`test_helper_keeps_owner_only_mode_through_the_grant`: stubs for `sudo` and
`setcap` as there, a `chown` stub that logs its arguments, and a `stat` stub
that answers `0:50` to `stat -c %u:%g ..` and runs the real `stat` otherwise.
Run the installer with `--fastpath`. Expected: the helper is given to the uid
and gid of the installing process, or the grant is refused because the
directory belongs to another account. Observed (run): `chown 0:50
./vmlab-fastpath`, then `setcap`, exit 0, and `Granted`.

### F15. A failed or interrupted guest swap deletes the old guest directory (data loss)

`install.sh:581-588` moves the old guest directory to `$gtmp/old`, moves the
new one into its place, and puts the old one back when the second rename
fails. `$gtmp` is the staging directory that `cleanup_guest`
(`install.sh:557-562`) removes with `rm -rf` on every exit. So the old
directory is deleted, with nothing in its place, in two cases:

- The put-back at `install.sh:585` fails too. That line is the last command
  of an `&&` list under `set -e`, so the shell exits there, and the trap
  removes `$gtmp/old`.
- A signal arrives between the two renames (`install.sh:248-249` turn `INT`
  and `TERM` into an exit, which runs the trap).

The directory may hold the result of `just guest-install`, a long build from
source, which the installer adopts as its own (`install.sh:537-544`). The
recipe that the installer names as its fallback does this right: it keeps the
backup when the old directory cannot be restored (`justfile:98-99`,
`:105-108`). `install.sh:584` also lacks `-T`: when something has made the
guest directory again between the two renames, the new tree lands inside it
as `guest/new` and the installer reports success.

Test: in `scripts/test_release_assets.py`, beside
`test_guest_marker_allows_upgrade`: an old guest directory with the bundle
layout and a file that holds `old`, a real guest tarball in the fixture, and
an `mv` stub that fails when its last argument is the guest directory.
Expected: the installer fails or warns, and the old files are still on disk,
at the guest directory or at a path that the message names. Observed (run):
exit 1 and no guest directory anywhere. The signal case: an `mv` stub that
runs the real `mv` and then sends `TERM` to its parent when the destination
ends in `/old`. Observed (run): exit 143 and no guest directory. Neither test
waits on a clock.

### F16. The version is counted from the nearest tag that HEAD reaches, not from the newest version (wrong result)

`scripts/release-version.sh:67` takes the last version from
`git describe --tags --match 'v*' --abbrev=0`. That is the nearest tag among
the ancestors of `HEAD`. The tags of the published versions are on merge
commits of `main` ("Merge next into main"), which `next` and a release branch
cut from `next` do not contain. On `next` today the script counts from
`v0.42.1-alpha`, seven versions behind `v0.49.0-alpha` (measured, see above).

- `--pre-release` gives `0.43.0-alpha`. That tag exists, so
  `scripts/release-local.sh:127-129` stops the build, and only because the
  tags are fetched.
- `--release` gives `0.42.1`, a promotion of an alpha from long ago
  (`scripts/release-version.sh:79-84`). No such tag exists, so
  `just release::local --release` builds it, signs it and prints the upload
  command for a stable `v0.42.1` below the newest pre-release. `install.sh`
  without `--pre` then installs it as the latest stable version.

`docs/agents/release-gate.md` B1 asks for "a clean full checkout of the
release commit". R2 and A3 have the release commit tested on a release branch
before the merge to `main`, which is where this checkout is. Nothing in the
script says that the answer depends on which side of the merge `HEAD` is.

Test: in `scripts/test_release_version.py`, a scratch repository with the
commit `A` tagged `v0.1.0-alpha`, a branch `main` with a merge commit tagged
`v0.2.0-alpha`, and `HEAD` on a branch from `A` that does not contain the
merge. Run the script with `--release` and with `--pre-release`. Expected: a
version above `0.2.0-alpha`, or an error that names the unreachable newer tag.
Observed (measured on `next`): the promotion `0.1.0` and `0.2.0-alpha`.

### F17. A download that fails is taken for a file that the release does not have (wrong result). Brought in by F5's fix

Two places decide on the exit code of one download and cannot tell an absent
file from a failed transfer:

- `install.sh:327-331` and `:354-363`: when the helper download fails for any
  reason, the installer removes the `vmlab-fastpath` of the earlier install
  with its capabilities, installs the new `vmlab`, says "this release gave no
  vmlab-fastpath-...", and exits 0. The signed `SHA256SUMS` is already
  verified at that point and names the helper, so the installer knows that
  the release has one. A timeout during an update turns the fast path off
  for good, and the message blames the release. F5's fix chose removal for a
  release without a helper. For a release that has one, the consistent end is
  the one F5's fix gave a helper with a bad checksum: stop before anything is
  replaced.
- `install.sh:269-278`: when the download of `SHA256SUMS.key.bundle` fails
  for any reason, the installer goes on to the keyless bundle, does not find
  one, and ends with "An unsigned release installs only with
  --insecure-skip-signature". On a correctly signed release and a network
  fault, the installer tells the user to turn the signature check off.

N7 of the first pass is the third place of this kind (`--pre`).

Test: in `scripts/test_release_assets.py`, the fixture as it is, which names
the helper in `SHA256SUMS`, an old `vmlab` and `vmlab-fastpath` in the bin
directory, and `FAIL_DOWNLOAD` set to the helper asset. Expected: a non-zero
exit and both old files unchanged. Observed (run): exit 0, the new `vmlab`,
no helper. This is the setup of
`test_old_helper_is_removed_when_the_release_gives_none`, which asserts
today's result and would move to a fixture whose `SHA256SUMS` has no helper
line. Second test: the key-signed fixture with `FAIL_DOWNLOAD` set to
`SHA256SUMS.key.bundle`. Expected: an error that names the failed download
and does not name `--insecure-skip-signature`. Observed (run): the unsigned
release message.

### F18. The installer's report describes QEMU as the x86 hypervisor (wrong result, cosmetic)

Since §22.4 every x86 guest and every container on an x86_64 host runs on
vmlab-hv under KVM only, and a host without `/dev/kvm` fails with "KVM is
required" (`src/config/hypervisor.rs:576`). The installer does not install
vmlab-hv, and its report after the install still describes the earlier state:

- `install.sh:644` reports a missing `qemu-system-x86_64` as needed to "run
  x86_64 guests" and prints an install command for it. No x86 guest uses it.
- `install.sh:645` reports `qemu-img` as needed to "clone disks, snapshot,
  and build templates". The manual calls it optional
  (`docs/manual/pages/getting-started/install.wcl:234`).
- `install.sh:717-719` says that without `/dev/kvm` "guests will run under
  emulation, which is very slow". They do not run.
- Nothing looks for `vmlab-hv`. A user who follows `curl ... | sh` to the end
  and installs every tool the report names cannot start one x86 VM or one
  container, and the report says nothing about the one program that is
  missing. The header (`install.sh:64-66`) says the same wrong thing.

The manual has the same two sentences about emulation
(`docs/manual/pages/getting-started/install.wcl:22` and `:442`) and the check
"`qemu-system-x86_64 --version` prints a version" (`:269`), beside its correct
table row (`:233`).

Test: in `scripts/test_release_assets.py`, run the installer without
`--skip-checks` with a `PATH` that has no `vmlab-hv`. Expected: the report
names `vmlab-hv` and how to get it (`--fetch`), does not give
`qemu-system-x86_64` as the way to run x86_64 guests, and does not promise
emulation without `/dev/kvm`. Observed (run): the three lines quoted under
"How the findings were checked".

## Notes

These have no proving test, are small, or are decisions already on record.

- N11. The installer that `origin/main` holds in this workspace (commit
  `ffa3e6e9`, 2026-11-10) has no `RELEASE_KEY` line at all. vmlab.io serves
  the `install.sh` of `main` (N10). `scripts/release-publish.sh` checks the
  signature against the checkout it runs in (`:61`) and not against the
  deployed installer, so step K4 of `docs/agents/release-gate.md` rests on a
  person. A key-signed release published before that deploy is one that
  `curl https://vmlab.io/install.sh | sh -s -- --pre` refuses. The workspace
  copy of `origin/main` can be old, so this was not checked against the site.
- N12. `scripts/release-publish.sh:71-73` uploads an unsigned release with one
  line on standard error. With the key on, every unsigned version is one that
  the installer refuses, and `--pre` picks the newest release whatever its
  signature. A flag to publish unsigned on purpose would make this a choice.
- N13. `scripts/release-publish.sh:48-54` uploads the two `.sha256` files
  without comparing them with the assets. `scripts/release-assets.py verify`
  does not read them. Nothing of vmlab reads them after F4's fix.
- N14. `install.sh:544` and `:576` write the marker `.vmlab-guest`, and
  nothing reads it. The comment at `install.sh:529-530` says that unmarked
  directories are adopted "only when ownership and the bundle layout match",
  and a marked directory gets exactly the same checks.
- N15. `install.sh:564-567`: when the guest download fails, the guest assets
  of the earlier version stay beside the new `vmlab`, and the warning says
  "the guest assets are not" installed. This is F5's shape for the guest
  bundle. Whether a new `vmlab` refuses older guest assets was not checked.
- N16. `install.sh:305`: `--fetch` moves the asset with `mv` and no `-T`.
  When the current directory holds a directory of that name, the file lands
  inside it and the installer prints `Fetched`. It also replaces an existing
  file of that name without a word.
- N17. A root script of the grant that is killed (`install.sh:421-451`)
  leaves a directory `.vmlab-fastpath.XXXXXX` of root's in the user's bin
  directory, which the user cannot empty. The installer's own
  `.vmlab-install.XXXXXX` stays after a `SIGKILL` too.
- N18. `install.sh:365-368` renames `vmlab` before the helper. An exit
  between the two leaves the new `vmlab` beside the old capable helper, which
  is what F5's fix set out to prevent. The window is two renames.
- N19. `scripts/release-sign.sh:69-74` reports every failure of the
  verification run as "the private and public halves differ", also when
  docker or the image failed. `release-publish.sh:65` and
  `release-verify.sh:41` tell the two apart with `--check` first.
- N20. `scripts/release-version.sh:67` matches every tag that starts with
  `v`. A tag that is not `vX.Y.Z` (`vnext`) gives a version like `next..1`,
  which `release-local.sh:126` refuses and `just release::version` prints. A
  field with a leading zero is read as octal at lines 112-115.
- N21. `scripts/release-assets.py:65` sets `startedOn` to the time `prepare`
  runs, which is after the build has ended.
- N22. `scripts/release-display-from.sh:33` reads the variable `BASE_URL`,
  a name that other tools export. The download is still checked against a
  signed `SHA256SUMS`, so the result is a failed run and nothing worse.
  `scripts/release-doctor.sh:126-128` reports changed tracked files and not
  the untracked files that `release-local.sh` also refuses.

## What was read

- `install.sh` in full.
- `scripts/release-local.sh`, `release-publish.sh`, `release-sign.sh`,
  `release-verify.sh`, `release-cosign.sh`, `release-version.sh`,
  `release-trust.sh`, `release-keygen.sh`, `release-doctor.sh`,
  `release-display-from.sh`, `release-assets.py` and `install-cosign.sh` in
  full.
- `build/dist.sh` and `build/run.sh` in full. `build/hv.py`: the cache key,
  `virgl_build`, `virgl_install` and `package`. `guest/package.sh` in full.
  `.just/release/mod.just` and `.just/buildbox/mod.just` in full. The `check`,
  `release-dry-run` and `hv-package` recipes of `.just/ci/mod.just`, and
  `guest-install` in `justfile`.
- How the guest build scripts check their download caches (the lines named
  in F13).
- `src/net/fastpath/helper/mod.rs` (`caller_refusal` and its tests).
- `docs/agents/release-gate.md` in full, and the install, vmlab-hv and
  runtime tool parts of `docs/manual/pages/getting-started/install.wcl`.
- The `permissions` lines of the five workflows, and the triggers of
  `deploy-site.yml`.
- The fixture and the test names of `scripts/test_release_assets.py`, and the
  test names of the other `scripts/test_release_*.py`.
- `docs/reviews/install-release.md` in full.

Not read: the bodies of the release tests. `build/Dockerfile`,
`scripts/guest-toolchains.sh` and `build/hv-pins.toml`. The rest of
`build/hv.py`, and `guest/ipxe/build.py` with its cache. The guest build
scripts beyond their cache checks. `guest/vdisplay-windows/` (`package.py`,
`rebuild.py`, `build.py`) and `guest/win3d/`. The helper's protocol, client
and server (`src/net/fastpath/helper/` beyond `mod.rs`). The in-guest agent
update (`src/template/agent_install.rs`, `src/labd/agent_repair.rs`). How
`vmlab` looks up `vmlab-hv` and the guest assets at run time. `ci.yml` and the
other workflows beyond the lines named above. Nothing was run with docker,
with cosign, with `sudo`, or against GitHub.
