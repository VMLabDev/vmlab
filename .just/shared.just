# Helpers shared by the root justfile and the `ci` module.
#
# `mod` gives each module its own namespace, so a recipe in `.just/ci` cannot
# depend on a root recipe. Everything both need — the require-* tool guards and
# the frontend/BPF build steps `ci::check` has to run for itself — is defined
# here once and imported into both.

# pnpm's content-addressed store. CI overrides it: under pnpm/action-setup's
# default store the @forge/* git deps build inside a node_modules path, where
# TypeScript's wildcard matching silently skips their source files and the
# packages end up shipping stub .d.ts files.
pnpm_store := env('PNPM_STORE_DIR', '')
pnpm_store_flag := if pnpm_store == '' { '' } else { '--store-dir ' + pnpm_store }

[private]
require-cargo:
	@command -v cargo > /dev/null 2>&1 || (echo 'cargo is required: install the Rust toolchain from https://rustup.rs' && exit 1)

[private]
require-pnpm:
	@command -v pnpm > /dev/null 2>&1 || (echo 'pnpm is required: the web UI @forge/* deps are git-subdir deps npm cannot resolve' && exit 1)

[private]
require-rustup:
	@command -v rustup > /dev/null 2>&1 || (echo 'rustup is required: ebpf/rust-toolchain.toml pins the nightly the BPF objects build with' && exit 1)

[private]
require-bpf-linker:
	@command -v bpf-linker > /dev/null 2>&1 || (echo 'bpf-linker is required: run "just ebpf-tools" once to build it against the pinned nightly' && exit 1)

# Install the SolidJS web UI's pnpm dependencies (@forge/* are git-subdir deps)
[group('web')]
web-ui-install: require-pnpm
	cd web-ui && pnpm install {{pnpm_store_flag}}

# Install the web UI's dependencies only when they are absent. There is no
# lockfile on purpose (the @forge revs are pinned in package.json), so every
# install re-resolves over the network — the gate must not pay that per run.
# Run `just web-ui-install` after changing package.json.
[private]
web-ui-deps-ensure:
	#!/usr/bin/env bash
	set -euo pipefail
	[ -d web-ui/node_modules ] || just web-ui-install

# Build the web UI to web-ui/dist (embedded into vmlab-web at compile time)
[group('web')]
web-ui-build: web-ui-deps-ensure
	cd web-ui && pnpm build

# Regenerate the console's status types from the Rust projection (src/status.rs)
[group('web')]
status-types-build: require-cargo
	cargo test --lib status::export_bindings

# Rebuild the committed BPF fast-path objects from ebpf/ (pinned toolchain)
[group('build')]
ebpf-build: require-rustup require-bpf-linker
	cd ebpf && cargo test -p fastpath-logic
	cd ebpf && cargo build --release --target bpfel-unknown-none -Z build-std=core \
		-p fastpath-sockmap -p xdp-switch
	cp ebpf/target/bpfel-unknown-none/release/fastpath-sockmap src/net/fastpath/bpf/fastpath_sockmap.bpf.o
	cp ebpf/target/bpfel-unknown-none/release/xdp-switch src/net/fastpath/bpf/xdp_switch.bpf.o
