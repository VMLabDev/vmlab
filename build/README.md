# vmlab-build: the build container

`build/Dockerfile` is an Ubuntu 24.04 image holding every toolchain the release
artefacts need: stable Rust with the four musl targets and
`x86_64-apple-darwin`, the pinned nightly with `rust-src`, mingw-w64 and the
msvcrt CRT, OpenWatcom v2, `gcc-multilib`, zig with `cargo-zigbuild` and
`rcodesign` for the macOS agent, `bpf-linker`, `just`, and the pinned `uv` runner
for the reproducible Secure Boot VARS build. It also holds what
`just ci::check` needs: rustfmt, clippy, the `x86_64-pc-windows-gnu` target,
and the QEMU and helper packages
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
| `vmlab-hv-<version>-linux-x86_64.tar.gz` and `.sha256` | `just ci::hv-package`, using the hypervisor commit pinned by `vmlab-hv-proto` in `Cargo.lock` |
| `bpf/fastpath_sockmap.bpf.o`, `bpf/xdp_switch.bpf.o` | `just ebpf-build` |

The run leaves tracked files as it found them. If you pass a version other than
the one in `Cargo.toml`, it is written into `Cargo.toml` for the binary build
and put back afterwards. The BPF objects are rebuilt into `target/dist/bpf/`
and compared with the committed copies. A difference fails the run, the same
way `just ci::ebpf-verify` would, and `src/net/fastpath/bpf/` is left as it
was.

## How it runs

To build only the hypervisor bundle, rebuild the image after native dependency
changes, then use the same recipe as release CI:

```sh
just buildbox::image
./build/run.sh just ci::hv-package
```

The bundle contains both hypervisor and viewer, virglrenderer 1.3.0 and its
Venus render server. The build verifies the source tarball's SHA256 before
compiling. `.ci-cache/virglrenderer-<version>-<sha256>/` holds the installed
native library outside `target/`, so Rust cache restores cannot overwrite it.
CI keys that cache on the version, SHA256 and build script. Rust output lives
under `target/hv/`. `readelf -d` checks relative RUNPATHs and `ldd` checks that
the hypervisor and server resolve the bundled library before packaging.
The archive has normalized ownership and timestamps, using `SOURCE_DATE_EPOCH`
or the checkout commit's timestamp.

The extracted `bin/vmlab-hv` launcher selects `libexec/virgl_render_server`
through virglrenderer's `RENDER_SERVER_EXEC_PATH` override. Without it, Venus
would use the absolute server path compiled into the library. The ELF binaries
use `$ORIGIN/../lib`; the library uses `$ORIGIN`. Firmware remains in the
existing guest bundle. The archive's `README` lists the remaining host runtime
libraries. glibc, Mesa and GPU drivers stay on the host.

`build/run.sh` starts the container and builds the image first if its tag is
missing. Set `VMLAB_BUILD_IMAGE` to use another tag. An existing image is not
rebuilt automatically; use `just buildbox::image` after toolchain changes.

`just guest-build` and `just guest-install` build the kernel and initramfs on
the host, then run firmware fetching, OVMF, iPXE, and SeaBIOS through
`build/run.sh`. Docker must be available to your user. To build only firmware:

```sh
bash guest/build-firmware.sh x86_64
```

The image and `build/run.sh` set `VMLAB_BUILD_ENV=1`. Firmware runs directly
when that marker is present, so `buildbox::dist` does not start a nested
container. The release CI job sets the same marker because it installs the
toolchains directly on its runner.

The container setup:

- It bind-mounts the checkout at its own absolute path, and also the git common
  directory when the checkout is a worktree, so git works inside the container.
- It runs the command as your uid and gid, so nothing in the checkout ends up
  owned by root. The container starts as root only long enough to give the
  volume roots to that uid, then drops to it with `setpriv`.
- It keeps build state in named volumes. `vmlab-build-cargo` holds the cargo
  registry and git cache and is shared by every checkout. Each checkout gets its
  own `vmlab-build-<hash>-*` volumes, mounted over `target/debug`,
  `target/release`, `target/hv`, `guest/*/target` and `ebpf/target`. These volumes shadow
  your host build directories without touching them, so a host build and a
  container build never invalidate each other. Remove the volumes with
  `docker volume rm` to start from cold.

The Alpine packages `guest/build-asset.sh` downloads are cached in
`guest/.cache/` inside the checkout, so they are only fetched once.

This image is not `e2e/Dockerfile`. That image is the end-to-end runtime: it
runs vmlab against KVM, and you start it with `just e2e::run`.
