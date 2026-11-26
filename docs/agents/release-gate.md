# Release gate

R1. Do not cut a new version while any cut version is waiting for its full
e2e. Keep fixes on that frozen release branch, not a new cut from `next`.

R2. Run `just ci::hv-gate` first, following [hv-gate.md](hv-gate.md). A failure
blocks e2e and publication. Then run the full e2e suite on the pushed release
commit, not a moving branch or an uncommitted checkout.

R3. Require one full green e2e on the exact commit to be merged, with the
same tree as the eventual main merge. The only alternative is a written
waiver per failed scenario on the release card. A waiver names the scenario,
failure, risk, reason for accepting it and approving lead. List skipped or
unrun scenarios explicitly too, never count them as passes. Changes after
the run invalidate its evidence unless the tree is identical.

## Reserve helios

The release lead reserves the host for the whole release e2e, including
setup and teardown. No other heavy vmlab run (including merge queue work)
may overlap. Coordinate with the lead before starting, observe the project
calendar and hardware reservations, and never stop another worker's process.

The shared wrapper `/var/build/cache/vmlab/bin/heavy` checks
`/var/build/cache/vmlab/heavy.hold` before admitting new work. The lead creates
that file exclusively (do not overwrite another hold), recording the release,
owner and run ID. Existing runs are not stopped: wait for both `slot1.lock`
and `slot2.lock` to drain. Hold both slot locks while setting up the reservation
to close the admission race, then release them before starting e2e.

Mark the release invocation with `VMLAB_HEAVY_RELEASE=1` while `/var/build/cache/vmlab/heavy.hold` reserves the host.
Keep all other workers and merge jobs paused, with no queued wrapper commands,
and start the release via `terrarium run` through the heavy wrapper.
If the host cannot be drained and other admissions paused, do not
start. Keep the reservation through teardown, remove only your own hold,
and tell the lead when the host is free. On interruption, record the hold
owner and unfinished run on the release card before handing off.

## Exact-tree release-card checklist

Card comments and waivers are free text, so use this manual checklist rather
than treating a parser of comments as an approval gate.

A1. In a clean full checkout, record `git rev-parse HEAD` and
`git rev-parse 'HEAD^{tree}'` on the release card before running hv-gate and
full e2e. Record commands, target, run IDs, exit codes and passed, failed and
ignored counts. Upload the actual test report with `terrarium test push`,
linking the release card, version label, commit and branch. Attach the full
log and expected scenario list so missing scenarios can be identified.

A2. Read the linked report with `terrarium test ls --card <release-card>`
and `terrarium test show <run-id>`. Check its commit matches the recorded
commit and its tree. Confirm this is one complete run, not a union of partial
reruns. Require all scenarios green or the explicit per-scenario waivers in
R3. A green `ci::check` or `release-dry-run` does not replace full e2e.

A3. Prepare the main merge locally without pushing. Compare
`git rev-parse 'HEAD^{tree}'` with `git rev-parse '<tested-commit>^{tree}'`.
Require equality and verify the tested commit is the release commit being
merged. If they differ, test the new candidate tree before publishing. Record
the final merge SHA and both tree hashes on the release card. Never stamp or
edit source files between this comparison and the merge.

## Build and publish a version (not on GitHub)

GitHub builds, signs and publishes no version (operator, 2026-11-22, card 423).
`.github/workflows/ci.yml` holds the cheap checks only. A trailer on a commit
starts nothing. A version is built on helios in the `vmlab-build` container,
and uploaded by a second command.

B1. Build. In a clean full checkout of the release commit, with the tags
fetched, as the account that owns the checkout:

```sh
just release::local --pre-release --bump patch    # or --release, or --bump minor|major
```

It runs `scripts/release-local.sh`. The version comes from
`scripts/release-version.sh` with the kind and the bump of the command line
(`just release::version --pre-release --bump patch` prints it and builds
nothing). Without `--bump` the commit subjects since the last `v*` tag decide.
`--release` after an alpha promotes that alpha. The build is
`build/run.sh build/dist.sh <version>` in the container. The result is the
release directory `target/release-local/v<version>/` (or `--out DIR`):

| File | What |
| --- | --- |
| `vmlab-<version>-linux-x86_64` | the CLI |
| `vmlab-fastpath-<version>-linux-x86_64` | the fast-path helper |
| `vmlab-guest-<version>.tar.gz`, `.sha256` | the strict guest bundle |
| `vmlab-hv-<version>-linux-x86_64.tar.gz`, `.sha256` | the hypervisor bundle |
| `provenance.json` | SLSA v1 statement. The builder id names the host and the commit: `.../scripts/release-local.sh@<commit>#host=helios` |
| `SHA256SUMS` | the four assets and `provenance.json` |
| `SHA256SUMS.key.bundle` | only with `--signing key`: the signature of `SHA256SUMS`, made by cosign in the container |

The command ends with a check: every asset is there and not empty,
`SHA256SUMS` and `provenance.json` describe the files, and the CLI reports the
version. `just release::verify <dir>` runs the check again, and checks a
`SHA256SUMS.key.bundle` too. The command refuses
a checkout with uncommitted changes to tracked files, a checkout with an
untracked file that the committed `.gitignore` files do not ignore (an exclude
in `.git/info/exclude`, in your own git configuration or in an untracked
`.gitignore` does not count), a checkout with anything in `.ci-cache/` (what
an earlier build or check in that checkout left: remove the directory, or run
checks with `VMLAB_TARGET_ROOT` set), and a
version whose tag exists. It uploads nothing. Start it like every heavy run on helios.

The command rebuilds the `vmlab-build` image first, so the image has the
toolchains that the commit pins. It takes the id of the image that this build
made, and starts every container of the release by that id: the build
containers and the cosign container. `provenance.json` holds the same id as
`buildImage`. The tag `vmlab-build` is one name for the whole docker host, and
a build of another checkout can move it while the release runs. That changes
nothing for the release. `--image-as-is` skips the rebuild and takes the image
that the tag names at that moment. It is for the version `0.0.0-dry-run`
only, and the command refuses it for every other version.
The hypervisor bundle takes its virglrenderer from a build of the same run,
not from `.ci-cache/`.
A change of `scripts/guest-toolchains.sh` rebuilds every layer of the image
(about 10 GiB). Every release build is a cold build: the command gives its
containers new docker volumes (`vmlab-release-<random>-*`, the cargo home and
the target dirs) and removes them when it ends, so no earlier build on the
host can leave sources or outputs for a release. A cold build of all assets
took 14 minutes on helios and
about 3 GiB of docker volumes, beside the caches in the checkout. Do not
start it with less than 15 GiB free: the disk is shared with the e2e runs.

B2. Publish. After R1 to R3 and A1 to A3, with the release commit on GitHub
`main`, the factory runs this with its GitHub login:

```sh
just release::publish target/release-local/v<version>
```

It checks the directory again and then runs exactly this, and nothing else
(`--print` shows it and uploads nothing):

```sh
gh release create v<version> --repo VMLabDev/vmlab --target <commit> \
  --title v<version> --generate-notes [--prerelease] \
  vmlab-<version>-linux-x86_64 vmlab-fastpath-<version>-linux-x86_64 \
  vmlab-guest-<version>.tar.gz vmlab-guest-<version>.tar.gz.sha256 \
  vmlab-hv-<version>-linux-x86_64.tar.gz vmlab-hv-<version>-linux-x86_64.tar.gz.sha256 \
  SHA256SUMS provenance.json [SHA256SUMS.key.bundle]
```

`<commit>` is the commit in `provenance.json`. A version with a suffix gets
`--prerelease`. GitHub makes the tag at that commit. The command refuses the
version `0.0.0-dry-run`.

## Windows display driver

The guest bundle needs the signed display package: `vmlab-display.inf`,
`.dll`, `.cat` and `.cer`, `install.exe`, `install.ps1`, the three `ivshmem`
files and `PACKAGE.json`. Two steps make it. The container compiles the DLL
and the installer with Clang and lld (the former `vdisplay-build` job).
Windows makes the catalog and test-signs it with the pinned Inf2Cat and
SignTool (the former `vdisplay-sign` job on a GitHub Windows runner).

`just release::local` takes the package in one of three ways:

- `--display staged` (default): the package staged in
  `guest/vdisplay-windows/target/signed-package`, or in
  `VMLAB_VDISPLAY_PACKAGE_DIR` inside the checkout. `guest/package.sh` refuses
  it when a source file changed after signing. When no build input of
  `guest/vdisplay-windows` changed since a published release, stage the
  package of that release: `scripts/release-display-from.sh <tag> [checkout]`.
  It downloads the guest bundle of the release, verifies the signature of its
  `SHA256SUMS` with cosign the way `install.sh` does (an unsigned release is
  refused), and stages the package through `package.py`, which compares every
  build input that the package names with the checkout.
- `--display rebuild`: runs `guest/vdisplay-windows/rebuild.py --route container`
  first. That compiles in the container and signs in the Windows 11 build lab
  on the host. It needs a built vmlab CLI (`VMLAB_RELEASE_VMLAB`, default
  `target/debug/vmlab`), the Windows 11 `standard` template, the config-weave
  Windows binary, and disk space for the build VM and its toolchain.
- `--display unsigned`: compile only. For the version `0.0.0-dry-run` alone.
  The bundle has no catalog and no certificate. It proves the build, and it is
  never a release.

Which way to take: compare the build inputs with the last release that has a
signed package, `git diff --stat <tag> HEAD -- guest/vdisplay-windows`. The
build inputs are the files that `SOURCES` in `rebuild.py` lists (`README.md`
and `TESTING.md` are none). No change: stage the package of that release. A
change: `--display rebuild`. `0.49.0-alpha` (card 444) took the package of
`v0.48.0-alpha` this way, because the two differ in `README.md` only.

Still open: `--display rebuild` was never run on helios (cards 423 and 444).
Helios has the Windows 11 template. The account vmlab has no config-weave
Windows binary. It is the asset `config-weave-windows-x86_64.exe` of a release
of `github.com/Configweave/config-weave` (with a `SHA256SUMS`), or the output
of `just install` in that repo. vmlab looks for it in `config_weave_bin_dir`
of the host config, then `VMLAB_CONFIG_WEAVE_DIR`, then
`~/.local/share/config-weave/bin`. The first version that changes a build
input of the driver needs one green `--display rebuild` before B1.

## Signing: two ways, and the decision of the operator

The keyless cosign signature of the old release job used the GitHub OIDC
identity of `ci.yml`. It cannot be made on helios. `scripts/release-sign.sh`
is the one signing step, with two ways (`--signing`, or
`VMLAB_RELEASE_SIGNING`):

- `none` (default): `SHA256SUMS` only. `install.sh` refuses the release
  unless the user gives `--insecure-skip-signature`.
- `key`: a cosign key pair. The private half comes from the environment
  variable `VMLAB_RELEASE_COSIGN_KEY` (the content of the encrypted
  `cosign.key`) and its password from `COSIGN_PASSWORD`. The public half is
  the committed file `build/release-signing.pub`. The step writes
  `SHA256SUMS.key.bundle`, then verifies it with the public half. Nothing goes
  to a transparency log.

cosign runs in the `vmlab-build` container, never on the host (card 1021).
`build/Dockerfile` installs the pinned cosign through
`scripts/install-cosign.sh`, which downloads the binary of the sigstore
release and refuses one without the pinned sha256. `scripts/release-cosign.sh`
is the one place a release step starts cosign. `just release::local --signing
key`, `release::sign`, `release::verify`, `release::publish`, `release::keygen`
and `scripts/release-display-from.sh` all go through it. A step that finds an
image from before the cosign layer says so and names `just buildbox::image`.

Where the key is, and where it is not:

- The two variables are in the environment of the command that signs. The
  factory fills them from its secrets for that command alone.
- `release-cosign.sh` hands them to the container with `docker run -e NAME`,
  a name without a value. The value is in no argument list, so `ps` shows the
  names only. cosign reads the key as `env://VMLAB_RELEASE_COSIGN_KEY`.
- Nothing writes them to a file. No script prints them, and each script that
  touches them turns shell tracing off first. A variable that is not set is an
  error that names the variable.
- The image holds no key and its history holds none: a key reaches a
  container at run time only. The signing container has no network, runs as
  the uid of the caller, mounts the release directory alone, and is removed
  when cosign ends. The build containers of `build/run.sh` never get the two
  variables.
- What stays true: root on the build machine, and every account in the
  `docker` group, can read the environment of a running process and of a
  running container. The password protects the key at rest in the secret
  store, not against the machine that signs.

`just release::verify <dir>` checks the assets, `SHA256SUMS`,
`provenance.json`, and a `SHA256SUMS.key.bundle` with the committed public
half. `just release::publish` verifies the bundle again before the upload,
against the `SHA256SUMS` beside it and with the committed
`build/release-signing.pub` only. It refuses a bundle that a test key made or
that signs an earlier `SHA256SUMS`.

`install.sh` picks the check by the signature file of the release.
`SHA256SUMS.key.bundle` is verified with `cosign verify-blob --key` against
`RELEASE_KEY`, a variable at the top of `install.sh`. That variable is the
switch, and it is on since card 1030 (2026-11-26): it holds the operator's
public key, the same text as `build/release-signing.pub`, and the installer
refuses a release that another key signed. With an empty variable the
installer trusts no key and refuses every key-signed release.
`SHA256SUMS.bundle` is verified keyless against the
identity of `ci.yml` as before, so every release that GitHub built still
installs. `--release-key FILE` verifies with another public key. It is for
testing a local build.

The operator's public key is committed. No version is signed with it yet: a
version built with `--signing none`, and every version published before the
first key-signed one, is unsigned, and `curl ... | sh -s -- --pre` refuses it
without `--insecure-skip-signature`. The first key-signed version needs K4
and K5 below.

The operator decided (2026-11-26, card 1021):

- D1. Yes: a cosign key pair is the trust root of the releases built outside
  GitHub.
- D2. The private half and its password are two secrets that the operator
  makes and stores: in the factory as secrets, with a copy of their own. No
  agent makes, sees, prints or stores them. Whoever reads them can sign a
  release that every installer accepts.
- D3. A signature without a transparency log is accepted for the alpha
  versions. `key` uploads nothing to Rekor, so cosign prints its tlog warning
  at each install.
- D4. One key in `install.sh` is accepted for the alpha versions. A lost or
  leaked key needs a new key and a new `install.sh`, and old copies of the
  installer refuse the new releases.

To turn `key` on. K1 and K3 are done: the operator made the pair on
2026-11-26, and card 1030 committed its public half. K4 and K5 are open, and
whether K2 is done is the operator's to say:

- K1. The operator makes the pair, at a terminal, on a machine they trust:
  `just release::keygen <dir>`. `<dir>` is outside every git checkout, and
  best on a medium that is not the disk of a build machine. cosign runs in the
  container and asks for the password twice. The recipe writes `cosign.key`
  (mode 600, encrypted with the password) and `cosign.pub` (mode 644), never
  overwrites a key, and prints the next steps. An agent never runs it for the
  real key.
- K2. The operator stores the content of `cosign.key` as the factory secret
  that fills `VMLAB_RELEASE_COSIGN_KEY`, and the password as the secret that
  fills `COSIGN_PASSWORD`, keeps a copy of both off the machine, and removes
  `cosign.key` from the machine. It is never committed.
- K3. `just release::trust <dir>/cosign.pub` copies the public half to
  `build/release-signing.pub` and writes the same PEM text into
  `RELEASE_KEY='...'` of `install.sh`, then checks that the two match. It
  refuses a file that holds a private key. Commit both files.
  `just ci::release-dry-run` runs the same check (`release-trust.sh --check`)
  and fails when the two differ, or when only one of them holds a key.
- K4. Deploy that `install.sh` to vmlab.io before the first key-signed
  release.
- K5. Build with `just release::local ... --signing key`, with the two
  variables in the environment. It checks them before the long build. To sign
  a directory that exists: `just release::sign <dir> key`. Signing needs
  cosign v2: v3 fails on `--tlog-upload=false`. Verifying works with v2.6.1
  and v3.

## A new build machine

A release can be built and signed on any Linux x86_64 machine. helios is one
such machine and holds nothing that another cannot get. The steps:

- M1. docker, with the daemon running and the build account in the `docker`
  group. On the host also `git`, `just`, `python3` and coreutils. No Rust, no
  cross toolchain and no cosign: they are in the image. `gh` with a GitHub
  login is needed for `just release::publish` only, and that step can run on
  another machine with the release directory.
- M2. A full checkout of the release commit with the tags, owned by the build
  account: `git clone https://github.com/VMLabDev/vmlab && cd vmlab &&
  git checkout <commit>`. Not `--depth`: the version is counted from the last
  `v*` tag.
- M3. `just release::doctor` (or `just release::doctor key` for a signed
  release). It prints one line per check (`ok`, `MISSING`, `note`), exits 1
  when something is missing, and changes nothing. It checks the host commands,
  docker, the image and its cosign, 15 GiB of free space, the checkout, the
  staged display package, the public key of the repo, and whether the two
  variables are set. It never reads or prints their values.
- M4. The two environment variables, for a signed release only:
  `VMLAB_RELEASE_COSIGN_KEY` (the content of `cosign.key`) and
  `COSIGN_PASSWORD`. Set them from the secret store for the build command
  alone. Do not write them to a file, a shell profile or a command line.
- M5. The signed Windows display package, as on helios: see "Windows display
  driver" above. `scripts/release-display-from.sh <tag>` stages the package of
  a published release and needs the image for its cosign check, so run
  `just buildbox::image` before it on a machine without the image.
- M6. `just release::local --pre-release --bump patch --signing key` (B1).
  The first run builds the `vmlab-build` image (about 10 GiB and a long
  build). Then `just release::verify <dir>`, and B2 to publish.

Nothing else moves. There is no key file, no cosign configuration and no
machine state to copy. `--builder NAME` or `VMLAB_RELEASE_BUILDER` names the
machine in `provenance.json` (default: its host name). `--display rebuild`
alone needs more on the machine: the Windows 11 build lab.

The offline `just ci::release-dry-run` tests cover the manifest, the
provenance, both signing ways through a cosign contract stub, what of the key
reaches an argument list, a message and a file (a docker stub), the recipes
`keygen`, `trust` and `doctor`, the installer's refusals and the upload
command. They prove no cryptography: card 1021 holds the proof with a real
cosign and a throwaway key, and card 1030 the proof that the committed
`install.sh` refuses a release signed by a throwaway key. Action SHA pins
in the remaining workflows must stay intact.
