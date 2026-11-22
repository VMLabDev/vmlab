# A release built on this host in the vmlab-build container, not on GitHub
# (card 423). `local` builds a version into a release directory and uploads
# nothing; `publish` is the second command, and it only uploads. The steps,
# the signing ways and what the operator decides: docs/agents/release-gate.md.

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

# Sign the SHA256SUMS of a release directory again: key (a cosign key pair) or none
[group('release')]
sign dir way='':
	./scripts/release-sign.sh {{quote(dir)}} {{way}}

# Check a release directory: every asset, SHA256SUMS and provenance.json
[group('release')]
verify dir:
	python3 scripts/release-assets.py verify {{quote(dir)}}
	cd {{quote(dir)}} && sha256sum --check SHA256SUMS

# Upload a release directory with `gh release create` (--print shows the command and uploads nothing)
[group('release')]
publish dir *args:
	./scripts/release-publish.sh {{quote(dir)}} {{args}}
