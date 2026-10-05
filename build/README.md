# vmlab-build: the build container

`build/Dockerfile` is an Ubuntu 24.04 image holding every toolchain the release
artefacts need: stable Rust with the four musl targets, the pinned nightly with
`rust-src`, mingw-w64 and the msvcrt CRT, OpenWatcom v2, `gcc-multilib`,
`bpf-linker`, and `just`. It also holds what `just ci::check` needs: rustfmt,
clippy, the `x86_64-pc-windows-gnu` target, and the QEMU and helper packages
the tests probe for. It holds none of the checkout. The toolchains come from
`scripts/guest-toolchains.sh`, one stage per layer, so editing the source never
rebuilds a toolchain.

```sh
just buildbox::image          # build the image (tag: $VMLAB_BUILD_IMAGE, default vmlab-build)
just buildbox::dist [version] # build every release artefact into target/dist/
just buildbox::shell          # open a shell in the same container setup
```

`buildbox::dist` writes the following into `target/dist/`. The version defaults
to the one in `Cargo.toml`.

| File | Built by |
| --- | --- |
| `vmlab-<version>-linux-x86_64` | `cargo build --release --bin vmlab`, named as CI names the release asset |
| `vmlab-guest-<version>.tar.gz` and `.sha256` | `just guest-package`, which fails if any target is missing |
| `bpf/fastpath_sockmap.bpf.o`, `bpf/xdp_switch.bpf.o` | `just ebpf-build` |

The run leaves tracked files as it found them. If you pass a version other than
the one in `Cargo.toml`, it is written into `Cargo.toml` for the binary build
and put back afterwards. The BPF objects are rebuilt into `target/dist/bpf/`
and compared with the committed copies. A difference fails the run, the same
way `just ci::ebpf-verify` would, and `src/net/fastpath/bpf/` is left as it
was.

## How it runs

`build/run.sh` starts the container for both recipes:

- It bind-mounts the checkout at its own absolute path, and also the git common
  directory when the checkout is a worktree, so git works inside the container.
- It runs the command as your uid and gid, so nothing in the checkout ends up
  owned by root. The container starts as root only long enough to give the
  volume roots to that uid, then drops to it with `setpriv`.
- It keeps build state in named volumes. `vmlab-build-cargo` holds the cargo
  registry and git cache and is shared by every checkout. Each checkout gets its
  own `vmlab-build-<hash>-*` volumes, mounted over `target/debug`,
  `target/release`, `guest/*/target` and `ebpf/target`. These volumes shadow
  your host build directories without touching them, so a host build and a
  container build never invalidate each other. Remove the volumes with
  `docker volume rm` to start from cold.

The Alpine packages `guest/build-asset.sh` downloads are cached in
`guest/.cache/` inside the checkout, so they are only fetched once.

This image is not `e2e/Dockerfile`. That image is the end-to-end runtime: it
runs vmlab against KVM, and you start it with `just e2e::run`.
