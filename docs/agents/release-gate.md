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
| `SHA256SUMS.key.bundle` | only with `--signing key` |

The command ends with a check: every asset is there and not empty,
`SHA256SUMS` and `provenance.json` describe the files, and the CLI reports the
version. `just release::verify <dir>` runs the check again. The command refuses
a checkout with uncommitted changes to tracked files, and a version whose tag
exists. It uploads nothing. Start it like every heavy run on helios.

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
  it when a source file changed after signing.
- `--display rebuild`: runs `guest/vdisplay-windows/rebuild.py --route container`
  first. That compiles in the container and signs in the Windows 11 build lab
  on the host. It needs a built vmlab CLI (`VMLAB_RELEASE_VMLAB`, default
  `target/debug/vmlab`), the Windows 11 `standard` template, the config-weave
  Windows binary, and disk space for the build VM and its toolchain.
- `--display unsigned`: compile only. For the version `0.0.0-dry-run` alone.
  The bundle has no catalog and no certificate. It proves the build, and it is
  never a release.

Left open by card 423: `--display rebuild` was not run on helios. Helios has
the Windows 11 template. Its disk was full (6 GiB free), and no config-weave
Windows binary was found there. A real version needs one green
`--display rebuild`, or a signed package staged by hand, before B1.

## Signing: two ways, and the decision of the operator

The keyless cosign signature of the old release job used the GitHub OIDC
identity of `ci.yml`. It cannot be made on helios. `scripts/release-sign.sh`
is the one signing step, with two ways (`--signing`, or
`VMLAB_RELEASE_SIGNING`):

- `none` (default): `SHA256SUMS` only. `install.sh` refuses the release
  unless the user gives `--insecure-skip-signature`.
- `key`: a cosign key pair. The private half comes from the environment
  variable `VMLAB_RELEASE_COSIGN_KEY` (the contents of the encrypted
  `cosign.key`) and its password from `COSIGN_PASSWORD`. The public half is
  the committed file `build/release-signing.pub`. The step writes
  `SHA256SUMS.key.bundle`, then verifies it with the public half. Nothing goes
  to a transparency log.

`install.sh` picks the check by the signature file of the release.
`SHA256SUMS.key.bundle` is verified with `cosign verify-blob --key` against
`RELEASE_KEY`, a variable at the top of `install.sh`. That variable is the
switch, and it is empty today: the installer trusts no key and refuses a
key-signed release. `SHA256SUMS.bundle` is verified keyless against the
identity of `ci.yml` as before, so every release that GitHub built still
installs. `--release-key FILE` verifies with another public key. It is for
testing a local build.

No trust root was picked in card 423. Until the operator turns `key` on,
every version built on helios is unsigned, and `curl ... | sh -s -- --pre`
refuses it without `--insecure-skip-signature`.

The operator decides:

- D1. Whether a key pair that the factory holds becomes the trust root of new
  releases. The alternative is unsigned releases (`none`), or another root in
  a later card.
- D2. Where the private half and its password live, and who can read them.
  Whoever reads them can sign a release that every installer accepts.
- D3. Whether a signature without a transparency log is enough. `key` uploads
  nothing to Rekor, so cosign prints its tlog warning at each install.
- D4. What happens when the key is lost or leaks. `install.sh` holds one key.
  A new key needs a new `install.sh`, and old copies refuse the new releases.

To turn `key` on:

- K1. Make the pair on a trusted machine with cosign v2.6.1 and a password:
  `cosign generate-key-pair`. It writes `cosign.key` and `cosign.pub`.
- K2. Store the contents of `cosign.key` as the factory secret that fills
  `VMLAB_RELEASE_COSIGN_KEY`, and the password as the secret that fills
  `COSIGN_PASSWORD`. Remove `cosign.key`. Never commit it.
- K3. Commit `cosign.pub` as `build/release-signing.pub`, and put the same
  text between the quotes of `RELEASE_KEY=''` in `install.sh`.
  `just ci::release-dry-run` fails when the two differ.
- K4. Deploy that `install.sh` to vmlab.io before the first key-signed
  release.
- K5. Install cosign v2.6.1 on helios. It has none today. Signing with
  cosign v3 fails on `--tlog-upload=false`. Verifying works with v2.6.1 and v3.
- K6. Build with `just release::local ... --signing key`. To sign a directory
  that exists: `just release::sign <dir> key`.

The offline `just ci::release-dry-run` tests cover the manifest, the
provenance, both signing ways through a cosign contract stub, the installer's
refusals and the upload command. They prove no cryptography. Action SHA pins
in the remaining workflows must stay intact.
