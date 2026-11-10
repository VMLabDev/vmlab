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
# The agent's Windows and macOS code is inside the bar: `guest-windows-check`
# and `guest-macos-check` lint it for x86_64-pc-windows-gnu and
# x86_64-apple-darwin, which need those two rustup targets and no linker.
#
# Deliberately outside the bar: the privileged fast-path tests
# (`just fastpath-test` — sudo, CAP_BPF, an XDP-capable kernel), the guest asset
# build (`just guest-build` — pulls an Alpine rootfs), and `guest-cross-build`,
# which needs the musl targets and cross linkers a developer has no other
# reason to install. ci.yml runs `guest-cross-build`; none of the three runs
# unattended on an ordinary machine.

# Recipes run from the project root, not from .just/ci.
set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: path-check shell-paths release-version release-dry-run lint fmt-check test guest-check guest-windows-check guest-macos-check ebpf-verify agent-conformance hv-desktop-test hv-fmt-check hv-lint hv-test win3d-check

# Refuse tracked paths that Windows cannot check out (: * ? " < > | \, trailing dot or space)
[group('ci')]
path-check:
	python3 -m unittest discover -s scripts -p 'test_check_paths.py'
	python3 scripts/check_paths.py

# Refuse shell scripts and just recipes that copy, move or remove a variable path
# that is not guarded, or that run without errexit (VML-88; the check's own tests come first)
[group('ci')]
shell-paths:
	python3 -m unittest discover -s scripts -p 'test_check_shell_paths.py'
	python3 scripts/check_shell_paths.py

# Test the release version calculation of ci.yml's "Calculate version" job: promotion of a pre-release, bumps, and 0.x breaking changes (VML-104)
[group('ci')]
release-version:
	python3 -m unittest discover -s scripts -p 'test_release_version.py'

# Offline release/installer dry-run, with an explicit signature-verifier stub
[group('ci')]
release-dry-run: require-uv
	uv run --no-project --with pytest==8.4.2 python -m pytest scripts/test_release_assets.py --junitxml=target/release-dry-run.xml

# Run clippy with warnings as errors
[group('ci')]
lint: require-cargo
	{{host_target_env}}cargo clippy --all-targets --all-features -- -D warnings

# Verify formatting without changing files
[group('ci')]
fmt-check: require-cargo
	cargo fmt --check

# Run the test suite
[group('ci')]
test: require-cargo require-bwrap
	{{host_target_env}}cargo test --all-features

# Run the Linux agent against the required protocol contract
[group('ci')]
agent-conformance: require-cargo require-bwrap require-waypipe
	cargo build --manifest-path guest/agent/Cargo.toml --target-dir {{quote(agent_target)}}
	{{host_target_env}}VMLAB_CONFORMANCE_AGENT={{quote(agent_target / 'debug/vmlab-agent')}} VMLAB_CONFORMANCE_TIER=full cargo test --lib external_agent -- --ignored --nocapture --test-threads=1

# Format check, lint and test one guest-side crate
[group('ci')]
guest-crate-check crate: require-cargo
	cargo fmt --check --manifest-path guest/{{crate}}/Cargo.toml
	{{ if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(_target_root / ('guest-' + crate)) + ' ' } }}cargo clippy --all-targets --manifest-path guest/{{crate}}/Cargo.toml -- -D warnings
	{{ if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(_target_root / ('guest-' + crate)) + ' ' } }}cargo test --manifest-path guest/{{crate}}/Cargo.toml

# Check every guest-side crate (standalone workspaces the host build never sweeps in)
[group('ci')]
guest-check: (guest-crate-check 'cinit-proto') (guest-crate-check 'cinit') (guest-crate-check 'agent-proto') (guest-crate-check 'agent')

# Lint the agent's Windows code (type-checks without a Windows linker)
[group('ci')]
guest-windows-check: require-cargo (require-rust-target 'x86_64-pc-windows-gnu')
	{{agent_target_env}}cargo clippy --target x86_64-pc-windows-gnu --manifest-path guest/agent/Cargo.toml -- -D warnings

# Lint the agent's macOS code (type-checks without zig or an Apple SDK)
[group('ci')]
guest-macos-check: require-cargo (require-rust-target 'x86_64-apple-darwin')
	{{agent_target_env}}cargo clippy --target x86_64-apple-darwin --manifest-path guest/agent/Cargo.toml -- -D warnings

# Cross-build the Linux agent binaries (musl targets; CI only)
[group('ci')]
guest-cross-build: require-cargo
	./guest/build-agent.sh linux-x86_64 linux-aarch64 linux-riscv64

# Verify the committed BPF objects match the ebpf/ sources
[group('ci')]
ebpf-verify: ebpf-build
	git diff --exit-code src/net/fastpath/bpf/

# Install the hypervisor's native build dependencies on Ubuntu
[group('ci')]
hv-native-deps-install: require-uv
	uv run --no-project python build/hv.py native-deps-install

# Install the imported hypervisor test dependencies on Ubuntu
[group('ci')]
hv-test-native-deps-install: require-uv
	uv run --no-project python build/hv.py test-native-deps-install

# Test the viewer's release payload and per-user icon installer
[group('ci')]
hv-desktop-test: require-uv
	uv run --no-project python -m unittest discover -s build -p 'test_*.py'

# Emit the pinned virglrenderer cache identity
[group('ci')]
hv-cache-key: require-uv
	uv run --no-project python build/hv.py cache-key

# Build virglrenderer with Venus and its render server outside target/
[group('ci')]
hv-virgl-build: require-uv
	uv run --no-project --with meson --with PyYAML python build/hv.py virgl-build

# Build and check the in-tree hypervisor release bundle in target/dist/
[group('ci')]
hv-package version='': require-cargo hv-virgl-build
	uv run --no-project python build/hv.py package {{quote(version)}}

# Check hypervisor formatting with the root toolchain
[group('ci')]
hv-fmt-check: require-cargo
	cargo fmt --all --check

# Lint every hypervisor crate without enabling guest-boot tests
[group('ci')]
hv-lint: require-cargo hv-virgl-build
	{{hv_target_env}}PKG_CONFIG_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/lib/pkgconfig" RUSTFLAGS="--cfg vmlab_ci_no_kvm --cfg vmlab_ci_no_host_assets" cargo clippy --workspace --exclude vmlab --all-targets -- -D warnings

# Run hypervisor tests, ignoring KVM guests and host-asset acceptance cases
[group('ci')]
hv-test: require-cargo hv-virgl-build hv-ipxe-build
	{{hv_target_env}}PKG_CONFIG_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/lib/pkgconfig" LD_LIBRARY_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/lib" RENDER_SERVER_EXEC_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/libexec/virgl_render_server" RUSTFLAGS="--cfg vmlab_ci_no_kvm --cfg vmlab_ci_no_host_assets" RUST_TEST_THREADS=2 cargo test --workspace --exclude vmlab

# Build the hypervisor's pinned iPXE option ROMs for non-guest tests
[group('ci')]
hv-ipxe-build: require-uv
	uv run --no-project python guest/ipxe/build.py --profile hv

# Run the guest-booting hypervisor gate locally, once per orchestrator batch (env: RUST_TEST_THREADS, default 2; VMLAB_HV_TEST_VM_LIMIT, default 4)
[group('ci')]
hv-gate: require-cargo hv-virgl-build hv-ipxe-build
	PKG_CONFIG_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/lib/pkgconfig" LD_LIBRARY_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/lib" RENDER_SERVER_EXEC_PATH="{{justfile_directory()}}/.ci-cache/{{hv_virgl_key}}/libexec/virgl_render_server" RUST_TEST_THREADS="{{hv_gate_test_threads}}" cargo test --workspace --exclude vmlab

# Check guest pins, documentation, Python packaging and shell scripts without building drivers
[group('ci')]
win3d-check: require-uv
	python3 guest/win3d/scripts/pins-table.py --check
	python3 guest/win3d/scripts/check-docs.py
	python3 -m unittest discover -s guest/win3d/scripts -p 'test_*.py'
	uv run --no-project --with shellcheck-py==0.11.0.1 python guest/win3d/scripts/check-shell.py

# Target dirs under VMLAB_TARGET_ROOT (see shared.just), one per cargo workspace
# and RUSTFLAGS set: `host` is the root workspace with default flags, `hv` the
# same workspace with the vmlab_ci_no_* cfgs of hv-lint and hv-test, and
# `guest-<crate>` each standalone guest workspace. Each *_env is a
# `CARGO_TARGET_DIR=... ` prefix, empty when VMLAB_TARGET_ROOT is unset.
host_target_env := if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(_target_root / 'host') + ' ' }
hv_target_env := if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(_target_root / 'hv') + ' ' }
agent_target := if _target_root == '' { 'guest/agent/target' } else { _target_root / 'guest-agent' }
agent_target_env := if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(agent_target) + ' ' }

hv_virgl_key := `python3 build/hv.py cache-key | cut -d= -f2`

# Test threads for hv-gate: the caller's RUST_TEST_THREADS, else 2. VMLAB_HV_TEST_VM_LIMIT
# passes through to vmlab-hv-tests, which defaults it to 4.
hv_gate_test_threads := env('RUST_TEST_THREADS', '2')
