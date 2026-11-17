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

# Persistent cargo target dirs. With VMLAB_TARGET_ROOT set (an absolute path),
# each cargo workspace, and each RUSTFLAGS set within one, builds into its own
# dir under it, so a validation run reuses the previous run's build without the
# workspaces evicting each other. Unset, cargo's default target/ is used as
# before. CI reports, virgl's .ci-cache, hv iPXE and rebuilt BPF objects also
# move under it, so ci::check needs no writes in the checkout. Release
# packaging (target/dist) remains in the checkout and is outside ci::check.
target_root := env('VMLAB_TARGET_ROOT', '')
_target_root := if target_root == '' { '' } else if target_root =~ '^/' { target_root } else { error('VMLAB_TARGET_ROOT must be an absolute path, got: ' + target_root) }

# The BPF workspace's target dir, and the env prefix that selects it for cargo.
ebpf_target := if _target_root == '' { 'ebpf/target' } else { _target_root / 'ebpf' }
ebpf_target_env := if _target_root == '' { '' } else { 'CARGO_TARGET_DIR=' + quote(ebpf_target) + ' ' }
ebpf_output := if _target_root == '' { 'src/net/fastpath/bpf' } else { _target_root / 'bpf' }
ci_cache := if _target_root == '' { justfile_directory() / '.ci-cache' } else { _target_root / '.ci-cache' }
release_report := if _target_root == '' { 'target/release-dry-run.xml' } else { _target_root / 'release-dry-run.xml' }

# Rebuild the committed BPF fast-path objects from ebpf/ (pinned toolchain)
[group('build')]
ebpf-build: require-rustup require-bpf-linker
	cd ebpf && {{ebpf_target_env}}cargo test -p fastpath-logic
	cd ebpf && {{ebpf_target_env}}cargo build --release --target bpfel-unknown-none -Z build-std=core \
		-p fastpath-sockmap -p xdp-switch
	[ -n {{quote(ebpf_target)}} ] && [ {{quote(ebpf_target)}} != / ] || { echo "VML-88: ebpf_target is empty or /" >&2; exit 1; }
	[ -n {{quote(ebpf_output)}} ] && [ {{quote(ebpf_output)}} != / ] || { echo "ebpf_output is empty or /" >&2; exit 1; }
	mkdir -p {{quote(ebpf_output)}}
	cp {{quote(ebpf_target)}}/bpfel-unknown-none/release/fastpath-sockmap {{quote(ebpf_output)}}/fastpath_sockmap.bpf.o
	cp {{quote(ebpf_target)}}/bpfel-unknown-none/release/xdp-switch {{quote(ebpf_output)}}/xdp_switch.bpf.o
