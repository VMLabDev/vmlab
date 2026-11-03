# Helpers shared by the root justfile and the `ci` module.
#
# `mod` gives each module its own namespace, so a recipe in `.just/ci` cannot
# depend on a root recipe. Everything both need — the require-* tool guards and
# the BPF build step `ci::check` has to run for itself — is defined here once
# and imported into both.

[private]
require-cargo:
	@command -v cargo > /dev/null 2>&1 || (echo 'cargo is required: install the Rust toolchain from https://rustup.rs' && exit 1)

[private]
require-uv:
	@command -v uv > /dev/null 2>&1 || (echo 'uv is required: run scripts/guest-toolchains.sh uv' && exit 1)

[private]
require-bwrap:
	@command -v bwrap > /dev/null 2>&1 || (echo 'bubblewrap is required for isolated host-process agent conformance' && exit 1)

[private]
require-waypipe:
	@command -v waypipe > /dev/null 2>&1 || (echo 'waypipe is required for the Linux agent app conformance case' && exit 1)

[private]
require-rustup:
	@command -v rustup > /dev/null 2>&1 || (echo 'rustup is required: ebpf/rust-toolchain.toml pins the nightly the BPF objects build with' && exit 1)

[private]
require-rust-target target: require-rustup
	@rustup target list --installed | grep -qx '{{target}}' || (echo 'the {{target}} rustup target is required: rustup target add {{target}}' && exit 1)

[private]
require-bpf-linker:
	@command -v bpf-linker > /dev/null 2>&1 || (echo 'bpf-linker is required: run "just ebpf-tools" once to build it against the pinned nightly' && exit 1)

# Rebuild the committed BPF fast-path objects from ebpf/ (pinned toolchain)
[group('build')]
ebpf-build: require-rustup require-bpf-linker
	cd ebpf && cargo test -p fastpath-logic
	cd ebpf && cargo build --release --target bpfel-unknown-none -Z build-std=core \
		-p fastpath-sockmap -p xdp-switch
	cp ebpf/target/bpfel-unknown-none/release/fastpath-sockmap src/net/fastpath/bpf/fastpath_sockmap.bpf.o
	cp ebpf/target/bpfel-unknown-none/release/xdp-switch src/net/fastpath/bpf/xdp_switch.bpf.o
