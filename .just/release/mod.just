# A release built on this host in the vmlab-build container, not on GitHub
# (card 423). `local` builds a version into a release directory and uploads
# nothing; `publish` is the second command, and it only uploads. cosign runs
# in the same container, and the release key comes from the environment
# (card 1021). The steps, the signing ways and a new build machine:
# docs/agents/release-gate.md.

set working-directory := '../..'

[default, private]
main:
	@just --list release

# Print the version a release of HEAD gets (--pre-release | --release, --bump major|minor|patch)
[group('release')]
version *args:
	./scripts/release-version.sh {{args}}

# Build a version into a release directory: assets, SHA256SUMS, provenance, signature, check (see --help)
[group('release')]
local *args:
	./scripts/release-local.sh {{args}}

# Sign the SHA256SUMS of a release directory again: key (a cosign key pair, from the environment) or none
[group('release')]
sign dir way='':
	./scripts/release-sign.sh {{quote(dir)}} {{way}}

# Check a release directory: every asset, SHA256SUMS, provenance.json, and the key signature when there is one
[group('release')]
verify dir:
	./scripts/release-verify.sh {{quote(dir)}}

# For the operator, at a terminal: make the cosign key pair of the releases in a directory outside the checkout
[group('release')]
keygen dir:
	@./scripts/release-keygen.sh {{quote(dir)}}

# Turn a public key on: copy cosign.pub to build/release-signing.pub and into RELEASE_KEY of install.sh, and check that they match
[group('release')]
trust pub:
	./scripts/release-trust.sh {{quote(pub)}}

# Say what this machine lacks to build and sign a release (key | none; never prints a secret)
[group('release')]
doctor way='':
	@./scripts/release-doctor.sh {{way}}

# Upload a release directory with `gh release create` (--print shows the command and uploads nothing)
[group('release')]
publish dir *args:
	./scripts/release-publish.sh {{quote(dir)}} {{args}}
