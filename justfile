import '.just/shared.just'

# The merge bar: everything a change must pass before it can merge (`just ci::check`)
mod ci '.just/ci'

[default, private]
main:
	@just --list

# Build the project (debug)
[group('build')]
build:
	cargo build

# Build release artifacts
[group('build')]
release:
	cargo build --release

# Run the test suite
[group('test')]
test: ci::test

# Format the codebase (the fixer for `just ci::fmt-check`)
[group('check')]
fmt:
	cargo fmt

# Install the vmlab binary into the user profile (~/.cargo/bin)
[group('build')]
install:
	cargo install --path . --locked

# Build the container micro-VM guest asset (kernel + initramfs, PRD §18)
[group('build')]
guest-build arch='x86_64 aarch64':
	./guest/build-asset.sh {{arch}}

# Build the vmlab-agent guest binaries (all targets; missing toolchains skip)
[group('build')]
agent-build target='':
	./guest/build-agent.sh {{target}}

# Rewrite the generated protocol reference (ADR-0007)
[group('build')]
proto-generate:
	VMLAB_WRITE_PROTOCOL_DOCS=1 cargo test --lib proto::report::tests::generated_artefacts_are_current

# Build + install the guest asset and agent binaries into ~/.local/share/vmlab/guest
[group('build')]
guest-install: guest-build agent-build
	mkdir -p ~/.local/share/vmlab/guest
	cp -r guest/dist/* ~/.local/share/vmlab/guest/

# The eBPF fast-path programs (ebpf/ workspace) need the nightly pinned in
# ebpf/rust-toolchain.toml plus bpf-linker built against that same toolchain
# (its LLVM proxy dlopens the toolchain's libLLVM — a mismatched install
# falls back to whatever LLVM the host has, breaking reproducibility).

# Install the pinned bpf-linker for the ebpf toolchain (one-time setup)
[group('build')]
ebpf-tools:
	cd ebpf && rustup run "$(grep '^channel' rust-toolchain.toml | cut -d'"' -f2)" \
		cargo install bpf-linker --version 0.10.3 --locked --force

# `ebpf-build` is imported from .just/shared.just (the gate needs it, and a
# module cannot depend on a root recipe); `ebpf-verify` is now `ci::ebpf-verify`.

# Run the privileged fast-path integration tests (kernel splice + XDP; sudo
# prompts once). The tier is a per-process singleton, so each tier gets its
# own invocation of the test binary with VMLAB_FASTPATH forced.
[group('test')]
fastpath-test:
	#!/usr/bin/env bash
	set -euo pipefail
	bin=$(cargo test --lib --no-run 2>&1 | sed -n 's|.*Executable unittests src/lib.rs (\(.*\))$|\1|p')
	[ -n "$bin" ] || { echo "could not locate the test binary"; exit 1; }
	sudo VMLAB_FASTPATH=sockmap "$bin" fastpath_sockmap --ignored --test-threads=1 --nocapture
	sudo VMLAB_FASTPATH=afxdp "$bin" fastpath_afxdp --ignored --test-threads=1 --nocapture

# A/B throughput smoke: the same frame pump with the fast path off vs on
[group('test')]
fastpath-bench:
	#!/usr/bin/env bash
	set -euo pipefail
	bin=$(cargo test --release --lib --no-run 2>&1 | sed -n 's|.*Executable unittests src/lib.rs (\(.*\))$|\1|p')
	[ -n "$bin" ] || { echo "could not locate the test binary"; exit 1; }
	echo "--- userspace ---"
	VMLAB_FASTPATH=off "$bin" fastpath_bench_ab --ignored --nocapture --test-threads=1
	echo "--- sockmap (skipped without CAP_BPF/CAP_NET_ADMIN) ---"
	sudo VMLAB_FASTPATH=sockmap "$bin" fastpath_bench_ab --ignored --nocapture --test-threads=1

# Bring a lab up (a VNC viewer opens per VM when the lab sets `gui = true`)
[group('lab')]
lab-up dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab up

# Stop a running lab gracefully (clones retained)
[group('lab')]
lab-down dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab down

# Tear a lab down completely: stop + delete clones and lab-local state
[group('lab')]
lab-destroy dir='examples/mixed-lab': release
	cd {{dir}} && {{justfile_directory()}}/target/release/vmlab destroy

# Launch the winsrv-desktop example (opens the WS2025 guest window)
[group('lab')]
winsrv-desktop: (lab-up 'examples/winsrv-desktop')

# The website + vmlab wskill are authored in wdoc and rendered by the `wcl` CLI.
# Install it from https://wcl.dev (or `cargo install --git …/wcl wcl`).

# Validate the vmlab wskill model and every projection template
[group('docs')]
wskill-check:
	wcl check docs/wskills/vmlab/wskill.wcl
	wcl check docs/wskills/vmlab/wdoc/book/main.wcl
	wcl check docs/wskills/vmlab/wdoc/skill/main.wcl
	wcl check docs/wskills/vmlab/wdoc/presentation/main.wcl
	wcl check docs/wskills/vmlab/wdoc/training/main.wcl

# Build the documentation website to docs/_site (landing + embedded reference book, deck, and course)
[group('docs')]
docs-build: wskill-check
	wcl wdoc build docs/main.wcl --out docs/_site

# Serve the website locally with live reload; pass `true` to enable comment review mode, and a port to pin one (`just docs-serve true 9090`). Default `auto` picks the first free port near 8080 and prints the URL
[group('docs')]
docs-serve comment="false" port="auto":
	wcl wdoc serve docs/main.wcl --addr {{ if port == "auto" { "auto" } else { "127.0.0.1:" + port } }} {{ if comment == "true" { "--comment" } else { "" } }}

# Regenerate the Claude Code skill at .claude/skills/vmlab from the wskill (single source)
[group('docs')]
skill-build: wskill-check
	wcl wdoc skill docs/wskills/vmlab/wdoc/skill/main.wcl --out .claude/skills/vmlab

# Remove generated site + wskill projections
[group('docs')]
docs-clean:
	rm -rf docs/_site docs/wskills/vmlab/out
