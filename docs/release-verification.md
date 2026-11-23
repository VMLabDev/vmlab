# Release verification

A release contains `SHA256SUMS` and `provenance.json`, and one signature of
`SHA256SUMS`. The manifest covers the CLI, the fast-path helper, the guest
bundle, the hypervisor bundle and the provenance.

Which signature a release has depends on where it was built:

- `SHA256SUMS.bundle`: releases that the GitHub workflow built. The release
  job signed with cosign using GitHub OIDC, with no stored signing key. The
  bundle includes the certificate and transparency-log evidence.
- `SHA256SUMS.key.bundle`: releases built on the project's build host and
  signed with the project's cosign key pair. The public half is
  `build/release-signing.pub` and is also inside `install.sh`. This way is off
  until that key is committed: until then `install.sh` refuses such a release.
- Neither: an unsigned release. `install.sh` refuses it without the opt-out
  below.

Install cosign v2.6.1 or newer through a trusted package source before using
`install.sh`. The installer will not bootstrap its own verifier from an
unverified download. By default, missing cosign or unsigned releases are refused.
Hash verification requires `sha256sum`.

For an independently trusted source only, an explicit opt-out is available:

```sh
sh install.sh --insecure-skip-signature
# Equivalent: VMLAB_INSECURE_SKIP_SIGNATURE=1 sh install.sh
```

This prints a warning and skips cosign signature authentication only. The
installer still downloads `SHA256SUMS` and verifies each asset digest before
using it. An attacker who replaces both an asset and its checksum can execute
malicious code with this option. Checksums alone do not authenticate a release.
Neither `--skip-checks` nor a missing cosign implicitly enables this opt-out.

To verify a downloaded manifest of a release that GitHub built:

```sh
cosign verify-blob --bundle SHA256SUMS.bundle \
  --certificate-identity https://github.com/VMLabDev/vmlab/.github/workflows/ci.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com SHA256SUMS
sha256sum --check SHA256SUMS
```

To verify one that is signed with the release key:

```sh
cosign verify-blob --key build/release-signing.pub --bundle SHA256SUMS.key.bundle \
  --insecure-ignore-tlog=true SHA256SUMS
sha256sum --check SHA256SUMS
```

A key signature is in no transparency log, so that check is turned off and
cosign prints a warning about it. The key alone authenticates the manifest.

The provenance is a SLSA v1 statement. For a release built on the build host
it records the asset digests, the source commit, the build image, and a
builder id that names the host and the commit. For an older release it
records the workflow and the run URL. It is authenticated by inclusion in the
signed manifest. This is not a claim of hermetic or independently verified
builds, or a SLSA assurance level.

`just ci::release-dry-run` exercises manifest and provenance creation, both
signing ways, the installer refusal paths and the upload command offline. Its
cosign contract stub checks the arguments, but does not prove cryptographic
signing. No release or signing secret is available to card-branch tests.
How a version is built and published: `docs/agents/release-gate.md`.
