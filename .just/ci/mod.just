# The merge bar: everything a change must pass before it can be merged.
#
# `ci::check` is the whole gate; every part is its own recipe so
# .github/workflows/ci.yml can fan them out across parallel jobs and still have
# one definition of what each part does. The exit code is the only signal a
# caller needs.
#
# The gate is self-sufficient from a clean checkout. External tooling is
# guarded by the require-* helpers in ../shared.just, so a missing tool reports
# as a missing tool rather than as a confusing build failure.
#
# Deliberately outside the bar: the privileged fast-path tests
# (`just fastpath-test` — sudo, CAP_BPF, an XDP-capable kernel), the guest asset
# build (`just guest-build` — pulls an Alpine rootfs), and ci.yml's cross-target
# agent check, which needs four rustup targets a developer has no other reason
# to install. None of the three runs unattended on an ordinary machine.

# Recipes run from the project root, not from .just/ci.
set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: lint fmt-check test guest-check ebpf-verify

# Run clippy with warnings as errors
[group('ci')]
lint: require-cargo
	cargo clippy --all-targets --all-features -- -D warnings

# Verify formatting without changing files
[group('ci')]
fmt-check: require-cargo
	cargo fmt --check

# Run the test suite
[group('ci')]
test: require-cargo
	cargo test --all-features

# Format check, lint and test one guest-side crate
[group('ci')]
guest-crate-check crate: require-cargo
	cargo fmt --check --manifest-path guest/{{crate}}/Cargo.toml
	cargo clippy --all-targets --manifest-path guest/{{crate}}/Cargo.toml -- -D warnings
	cargo test --manifest-path guest/{{crate}}/Cargo.toml

# Check every guest-side crate (standalone workspaces the host build never sweeps in)
[group('ci')]
guest-check: (guest-crate-check 'cinit-proto') (guest-crate-check 'cinit') (guest-crate-check 'agent-proto') (guest-crate-check 'agent')

# Verify the committed BPF objects match the ebpf/ sources
[group('ci')]
ebpf-verify: ebpf-build
	git diff --exit-code src/net/fastpath/bpf/
