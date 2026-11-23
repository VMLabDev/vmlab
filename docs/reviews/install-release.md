# Review: install, update and release scripts

Card #581. A fault-finding review of how vmlab is installed, updated and
released: `install.sh`, the release scripts (`scripts/release-*.sh`,
`scripts/release-assets.py`, `scripts/install-cosign.sh`), the build they
call (`build/dist.sh`, `build/run.sh`, `guest/package.sh`), the CI workflows,
the signature checks, the update path of the binary and of the guest assets,
and the capability helper `vmlab-fastpath`. It lists faults and makes no fix.
Reviewed at `next` commit `6c24250c`. All paths are from the repository root,
and all line numbers are from that commit.

Every finding is from reading the code, except F6, whose central step was run
in a sandbox (quoted there). Each finding names the test that would prove it.
A finding whose test fails to reproduce is wrong and should be dropped.

Severity words: **data loss**, **crash**, **security** (against a hostile
download, mirror or asset host, another local user, or a program running as
the installing user), **wrong result**, **cosmetic**.

## Who is trusted

- The person who runs `install.sh` is trusted, and so are its flags and
  environment (`--release-key`, `VMLAB_RELEASE_BASE_URL`,
  `VMLAB_INSECURE_SKIP_SIGNATURE`). A flag that weakens a check is not a
  finding by itself.
- The download is not trusted: the asset host, a mirror named by
  `VMLAB_RELEASE_BASE_URL`, and anyone who can replace release assets. The
  signature over `SHA256SUMS` is what is meant to stop them.
- The helper's file capabilities are meant to give "any program running as
  you" exactly the closed request vocabulary of
  `src/net/fastpath/helper/server.rs`, and nothing more
  (`install.sh:45-54`, `install.sh:322`). A path to more than that, or to
  that for another user, is ranked **security**.
- A release is built on helios under the account that also builds untrusted
  card branches. Code on a card branch is not trusted to change a release.

## Findings

### F1. The capability grant can be steered onto another file (security)

`install.sh:291-296` moves the verified helper to `$BIN_DIR/vmlab-fastpath`,
a path in a directory the user owns. Only later, `install.sh:377` runs
`sudo setcap cap_bpf,cap_net_admin+ep "$HELPER"` on that path. Between the
two, `fastpath_ask` (`install.sh:326-336`) waits on the terminal for as long
as the person takes to answer, and `sudo` waits for its password. Nothing
checks that the file at the path is still the verified one when `setcap`
runs: no digest under sudo, no file descriptor kept open, no root-owned
staging place.

Any program running as the user (the threat the helper design names) can
rename its own regular file onto `$BIN_DIR/vmlab-fastpath` in that window.
The person answers `y`, and root gives CAP_BPF and CAP_NET_ADMIN to that
program, which then loads any BPF and reconfigures the host network,
outside the closed vocabulary. Where `fs.protected_hardlinks` is 0, a hard
link to a root-owned binary (for example `/usr/bin/python3`) works too, and
then every user of the host holds the capabilities through it.

Test: run `install.sh --version <v>` against a local release server
(`VMLAB_RELEASE_BASE_URL`) from a terminal. When the `[y/N]` prompt shows,
from a second shell as the same user run
`cp /usr/bin/busybox ~/.local/bin/.x && mv ~/.local/bin/.x ~/.local/bin/vmlab-fastpath`.
Answer `y`. Then `getcap ~/.local/bin/vmlab-fastpath` shows the capabilities
on busybox, and `sha256sum` of the file differs from the helper's line in
`SHA256SUMS`. The expected result is a refusal.

Fixed in card #607. `fastpath_caps` no longer grants a path in the bin
directory. It opens the helper as the user and gives the stream to one
`sudo sh -c` script (`FASTPATH_GRANT`), which copies it into a mode 0700
staging directory of root's inside the bin directory, checks the SHA-256 of
that copy against the line of the signed `SHA256SUMS`, runs `setcap` on the
copy, and renames it onto `vmlab-fastpath`. A file swapped in before sudo
starts fails the digest and gets nothing (a refusal, with a warning and no
`sudo setcap` advice). A file swapped in later is replaced by the verified,
granted copy. Root makes a new file, so a hard link cannot receive the grant,
and `setcap` works in a directory no other user can change, which covers N5.
Proof: `test_fastpath_refuses_a_file_swapped_in_before_the_grant` and
`test_fastpath_grants_only_verified_bytes_when_swapped_under_sudo` in
`scripts/test_release_assets.py`, with stubs for `sudo` and `setcap`. Both
failed before the fix: the stub `setcap` was run on the swapped-in file. The
manual `sudo setcap` command that the installer prints when it grants nothing
itself is unchanged, and whoever runs it grants whatever is at the path.

### F2. Every local user who can reach the helper gets its capabilities (security)

The helper is installed mode 0755 (`install.sh:294`, after the download into
a `mktemp -d` directory, umask 022) and granted file capabilities. File
capabilities apply to whoever executes the file, not to its owner. So any
local account that can traverse `$BIN_DIR` (a home directory of mode 0755
is the default on several distributions, and `--bin-dir` can name any
directory) starts the helper on a socket pair and asks it to create taps,
bring them up, and load and attach the XDP and sockmap programs. Nothing in
`src/net/fastpath/helper/mod.rs:22-55` checks the caller's uid. Each
connection may hold 64 engines with 256 taps each
(`src/net/fastpath/helper/proto.rs:24-26`), and connections are unlimited,
so another user can also fill the host with `vmfp*` interfaces. The consent
text says "Any program running as you" (`install.sh:322`, and the manual),
which is not what is granted.

Test: install with `--fastpath` into a directory under a 0755 home. As a
second account, run a small program that makes a `SOCK_SEQPACKET` pair,
starts `~first/.local/bin/vmlab-fastpath` with one end as stdin, and sends
`XdpLoad` then `XdpTap`. Expected: a refusal. Observed (by reading): a tap
`vmfp0` appears in `ip link`, up, with the XDP program attached.

Fixed by card #608, in two halves. `install.sh` installs the helper mode
0700, so no other account can execute it
(`scripts/test_release_assets.py`: `test_helper_is_executable_by_its_owner_only`,
`test_helper_keeps_owner_only_mode_through_the_grant`). The helper itself
refuses a start that gained privilege from its file (`AT_SECURE`) when the
caller is neither the owner of the file nor root
(`src/net/fastpath/helper/mod.rs`, `caller_refusal` and its tests), which
also covers a helper whose mode was changed and one granted by hand. A
helper that root owns still serves every user: only the administrator can
have granted it. The limits on engines and taps are per connection as
before. The two-account test above was not run: the merge bar has one
account and no way to grant file capabilities.

### F3. A release is built from caches that any build on the host can write (security)

`build/run.sh:59` mounts one named docker volume, `vmlab-build-cargo`, as the
cargo home of every container build on the docker host, from every checkout.
It holds `registry/src`, the unpacked crate sources that cargo compiles
without checking them again against `Cargo.lock` once the `.cargo-ok` marker
is there. `build/run.sh:61-67` adds per-checkout target volumes keyed by the
checkout's path only (`build/run.sh:45`), so a later checkout at the same
path (a reused worktree directory) inherits the earlier build's
`target/release`, where cargo decides freshness by mtime.

`scripts/release-local.sh:131` builds the release in exactly this container.
Any code that ran earlier in a `build/run.sh` container as the same uid (a
`build.rs` or test of a card branch under `just buildbox::shell` or
`just buildbox::dist`) can edit a crate's source in the shared registry, or
leave fresh outputs in a reused target volume. The next release compiles
them into the CLI and the capability helper, and `provenance.json` still
names a clean `HEAD`.

Test: in checkout A run
`build/run.sh sh -c 'f=$(ls -d $CARGO_HOME/registry/src/*/serde_json-*/src/lib.rs | head -n1); echo "compile_error!(\"tampered\");" >> "$f"'`.
Then in checkout B (a clean checkout of a release commit) run
`just release::local --version 0.0.0-dry-run --display unsigned --image-as-is`.
Expected: the build is unaffected. Observed (by reading): it fails with
`tampered`, which shows the edit reached the release build; an edit that
compiles would pass silently.

Fixed in card #634. `build/run.sh` takes the prefix of its named volumes from
`VMLAB_BUILD_VOLUMES` (default `vmlab-build`, so other builds are as before).
`scripts/release-local.sh` sets it to `vmlab-release-<16 random hex digits>`
for every container of the release (the display driver build and
`build/dist.sh`), refuses to start when a volume of that name exists, and
removes the volumes when it ends. A release so starts from an empty cargo
home, where cargo downloads each crate and checks it against `Cargo.lock`,
and from empty target dirs. Every release build is now a cold build. Proof:
`test_a_release_build_takes_nothing_from_the_volumes_of_another_build` in
`scripts/test_release_workflow.py`, with a stub `docker` that keeps each
volume as a directory and writes a file into every volume a container mounts.
Before the fix the release mounted `vmlab-build-cargo` and the target volumes
of the earlier build at the same path, each with that file in it. The docker
test above was not run: the merge bar has no docker. Not covered: the caches
inside the checkout (`guest/.cache/`, `.ci-cache/`, `guest/dist/`), which
belong to the release checkout, and code that runs outside a container as the
build account, which can reach docker itself.

### F4. The documented vmlab-hv install checks the bundle against an unsigned sidecar (security)

`docs/manual/pages/getting-started/install.wcl:89-98` tells the user to
download `vmlab-hv-<version>-linux-x86_64.tar.gz` and its `.sha256` from the
release and run `sha256sum -c` on the pair. The `.sha256` sidecars are
uploaded by `scripts/release-publish.sh:45-48` but are not in `SHA256SUMS`
(`scripts/release-assets.py:86-88` lists the four assets and
`provenance.json`), so no signature covers them. Whoever can replace release
assets replaces both files, and the check passes. `install.sh` does not
install vmlab-hv at all, so this manual path is the only one, and the
hypervisor it installs runs every new x86_64 KVM VM (§22). It stays
unauthenticated even after a release key is turned on.

Test: serve a release directory locally, replace the hv tarball with any
tarball and regenerate its `.sha256`. Follow the manual's commands against
that server: `sha256sum -c` prints `OK`. Verifying against the signed
`SHA256SUMS` instead reports a mismatch.

### F5. An old helper keeps its capabilities beside a new vmlab (wrong result, security)

Three paths leave the previous `vmlab-fastpath`, with its file capabilities,
next to a newer `vmlab`:

- `install.sh:291-299`: when the helper download fails for any reason
  (a network error, a release without the asset), the installer warns that
  "the eBPF fast path stays off". It does not remove or check the old helper,
  which is still capable and is what the daemons start
  (`src/net/fastpath/helper/client.rs:58-70`).
- `install.sh:280` replaces `vmlab` before the helper is verified at
  `install.sh:292`. A checksum mismatch on the helper ends the install with
  the new CLI and the old capable helper in place.
- `--no-guest` and the other options do not change this.

The daemon and the helper exchange no version (`client.rs:76-115`, and no
version field in `src/net/fastpath/helper/proto.rs`). A new daemon then
drives an old privileged binary, with old embedded BPF objects and any fault
fixed since, and the user was told the fast path is off.

Test: install release A with `--fastpath` (helper capable). Install release
B from a server that returns 404 for B's helper asset. Expected: the old
helper is removed or reported. Observed: `getcap ~/.local/bin/vmlab-fastpath`
still lists the capabilities, `vmlab --version` is B, and `vmlab fastpath`
selects a kernel tier through A's helper.

### F6. Replacing the binary is not atomic and can leave a broken one (wrong result)

`install.sh:215` stages downloads in `mktemp -d`, under `$TMPDIR` or `/tmp`,
which is often tmpfs, while `$BIN_DIR` is on disk. `install.sh:280` and
`install.sh:295` then `mv` across filesystems. GNU `mv` does that by
removing the destination and copying into its name. A failed copy leaves a
truncated, non-executable file and no old binary. `install_guest` does it
right (staged beside the target, `install.sh:427-431`); the binary and the
helper do not.

Evidence (run in the review sandbox, `/tmp` tmpfs and `/work` on disk, with
an old binary at the destination):

```
(trap '' XFSZ; ulimit -f 50; mv $s/new $d/vmlab)
mv: error writing '/work/.mvtest.2UyV/vmlab': File too large
-rw------- 1 agent agent 51200 Nov 23 01:31 vmlab
```

Test: put an old `vmlab` in a `--bin-dir` on another filesystem than
`$TMPDIR`, make the copy fail (`ulimit -f` below the binary size, or a
nearly full filesystem), and run the installer. Expected: the old `vmlab`
is intact. Observed: a 0600 partial file. A `vmlab` started during the copy
also sees a partial file.

### F7. The provenance names HEAD while untracked files change the build (wrong result)

`scripts/release-local.sh:81-82` refuses changes to tracked files only
(`--untracked-files=no`). The container sees the whole checkout. An
untracked `.cargo/config.toml` (not in `.gitignore`, so it is untracked, and
hidden by that flag) can set `rustflags`, `[env]`, `[patch]` or a source
replacement, and changes the CLI and the helper. `provenance.json`
(`scripts/release-assets.py:75-79`) still says the assets come from `HEAD`.

Test: in a clean checkout create `.cargo/config.toml` with
`[env]\nVMLAB_PROBE = "x"` (or a `[patch.crates-io]` entry), and run
`just release::local --version 0.0.0-dry-run --display unsigned`. Expected:
a refusal of the untracked file. Observed: the build proceeds and the
provenance names `HEAD`.

### F8. Publishing does not check the signature it uploads (wrong result)

`scripts/release-publish.sh:52-56` uploads `SHA256SUMS.key.bundle` when the
file is not empty, without verifying it against `build/release-signing.pub`
or against the `SHA256SUMS` beside it. Two ways to get a bad one:

- `scripts/release-sign.sh:50` verifies with `VMLAB_RELEASE_PUBLIC_KEY` when
  it is set ("a test key: never for a published release"), so a release
  signed with a test pair passes signing, `release::verify` and publishing.
- `scripts/release-assets.py prepare` rewrites `provenance.json` with a new
  timestamp and so a new `SHA256SUMS` (`release-assets.py:65`, `84-88`) but
  leaves an existing bundle. `just release::verify` does not look at the
  bundle.

Either way the published release is one that every installer with the real
key refuses (`install.sh:245-247`).

Test: make a test key pair, sign a dry-run directory with
`VMLAB_RELEASE_PUBLIC_KEY=test.pub VMLAB_RELEASE_COSIGN_KEY=... release-sign.sh <dir> key`,
change its version to a real one, and run `release-publish.sh <dir> --print`:
it prints the command with the bundle. For the second way, run
`release-assets.py prepare` again after signing and then `release::verify`:
it passes, and `cosign verify-blob --key build/release-signing.pub` fails.

### F9. Every release build drops `--locked` (wrong result)

`build/dist.sh:63-69` drops `--locked` when the version differs from
`Cargo.toml`'s. `Cargo.toml:3` is `0.1.0` and is never bumped (the tags are
`v0.9.0-alpha` and up), so every release builds without `--locked`. A
`Cargo.lock` that does not match `Cargo.toml` is then resolved again against
the registry at release time, and the release links crates that no gate
tested, with no error.

Test: in a scratch checkout delete one `[[package]]` entry from `Cargo.lock`
and run `build/dist.sh 9.9.9`. Expected: a failure. Observed: cargo
resolves the crate again and the build passes. `cargo build --locked` on
the same tree fails.

### F10. `just guest-install` merges over an old guest directory (wrong result)

`justfile:85-87` runs `cp -r guest/dist/* ~/.local/share/vmlab/guest/`. Files
of an older version that the new build no longer has stay (an agent target
that was dropped, an old firmware name). `install.sh:401` sends every failed
guest install to this recipe, and the manual promises that "nothing from an
older version survives an upgrade". The recipe also writes through a
symlinked guest directory, which `install.sh:409-412` refuses.

Test: create `~/.local/share/vmlab/guest/agent/old-target/vmlab-agent` and
run `just guest-install`. Expected: the file is gone. Observed: it stays.

### F11. The manual describes the guest bundle check and its builder wrongly (cosmetic)

`docs/manual/pages/getting-started/install.wcl:287` says the bundle is
"built by CI", and `install.wcl:292` says the installer checks it "against
the `.sha256` published beside it". Releases are built on helios
(`docs/agents/release-gate.md`, "Build and publish a version"), and
`install.sh:446` checks the bundle against the signed `SHA256SUMS`, which is
the stronger check. A reader who copies the manual's description for a
manual check uses the unsigned sidecar (see F4).

Test: compare the two lines with `install.sh:423-446` and
`scripts/release-local.sh:2-4`.

## Notes

These have no proving test, or are decisions already on record.

- N1. No release can be authenticated today. `RELEASE_KEY` is empty
  (`install.sh:76`), `build/release-signing.pub` does not exist, and no CI
  job makes the keyless signature any more (`.github/workflows/ci.yml:203-206`).
  So every current release installs only with `--insecure-skip-signature`,
  which `release-publish.sh:55` tells the publisher. This is the open
  decision D1 in `docs/agents/release-gate.md`. It should be closed before
  users are told to use that flag routinely.
- N2. `install.sh:253-256` and `scripts/release-display-from.sh:73-77` still
  trust any keyless signature by `ci.yml` on `main`. Today `ci.yml` has
  `contents: read` only and no `id-token: write`. A later job given
  `id-token: write` in that file could sign any blob that the installer
  accepts as a release.
- N3. `install.sh:205` asks for "cosign v2.6.1 or newer" and checks no
  version. `scripts/install-cosign.sh:7-8` pins v2.6.1 because v3 differs on
  the tlog flags. Whether v3's `verify-blob --insecure-ignore-tlog=true
  --key` still verifies the key bundle was not checked.
- N4. `install.sh:243-249` prefers `SHA256SUMS.key.bundle` when it exists. A
  host that serves a junk key bundle beside a valid keyless bundle makes the
  install fail. That host can refuse service anyway, so this is no gain.
- N5. libcap's `cap_set_file` does `lstat` and then `setxattr` on the path,
  so even a symlink check inside `setcap` leaves a narrow race of its own.
  Covered by F1's fix, not tested apart.
- N6. With umask 002 the installer leaves `vmlab` and the helper mode 0775.
  On a host where the user's group is shared, a group member can rewrite
  `vmlab`. On the usual one-user-per-group hosts it does not matter.
- N7. `install.sh:182-183`: under `--pre`, a GitHub API failure (rate limit)
  ends with "could not find any release", because the pipe hides the
  failure of `http_get`. Cosmetic.
- N8. `scripts/release-local.sh:90-92` refuses a version whose tag exists
  locally only. Without `git fetch --tags` it can build a version that
  exists on GitHub. `gh release create` then refuses it, so nothing wrong is
  published.
- N9. The Windows display package of a release is taken from an earlier
  build when the hashes of its inputs match (`guest/vdisplay-windows/package.py`
  via `guest/package.sh:121-123`). That proves the inputs, not that the
  binaries came from them. A design choice, recorded in card 444.
- N10. `install.sh` is served from Cloudflare Pages, deployed from `main`
  (`.github/workflows/deploy-site.yml:73-84`). The trust root (`RELEASE_KEY`,
  the keyless identity) is inside the served script, so whoever controls that
  project or its API token controls every `curl | sh` install. That is
  inherent to the pattern.

## What was read

- `install.sh` in full.
- `scripts/release-local.sh`, `release-publish.sh`, `release-sign.sh`,
  `release-version.sh`, `release-display-from.sh`, `release-assets.py`,
  `install-cosign.sh` in full.
- `build/dist.sh`, `build/run.sh`, `build/install-viewer-icons.sh` in full;
  `guest/package.sh:90-145`; `.just/release/mod.just`; the `release-dry-run`
  and `check` recipes of `.just/ci/mod.just`; `justfile:85-92`.
- `.github/workflows/ci.yml` (permissions and the release comment),
  `deploy-site.yml` in full, and the `permissions` lines of the other
  workflows.
- `src/net/fastpath/helper/mod.rs`, `server.rs:1-460`, the helper start in
  `client.rs:41-115`, the limits in `proto.rs`, and `create_tap` in
  `src/net/fastpath/engine.rs`.
- `docs/agents/release-gate.md` (build, publish and signing sections) and
  `docs/manual/pages/getting-started/install.wcl` (the install, vmlab-hv and
  guest asset parts).

Not read: the rest of `proto.rs`, `client.rs` and `server.rs`, and the BPF
loading in `engine.rs` (the helper's request checks belong to the
networking review); `build/Dockerfile` and `scripts/guest-toolchains.sh`;
`guest/fetch-firmware.sh`; the Windows display build and signing
(`guest/vdisplay-windows/`, `guest/win3d/`); `src/template/agent_install.rs`
and `src/labd/agent_repair.rs` (the in-guest agent update); the release test
scripts under `scripts/test_release_*.py`; `hv-nightly.yml`,
`parser-fuzz.yml` and `win3d.yml` beyond their permissions.
