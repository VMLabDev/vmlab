# Linux template rebuild on helios

Card #245 used vmlab `next` at `b09d50f00c6620f0d31055016e8763baa1b71a51`
and vmlab-templates `main` at `c1d7e52f2539ded9eb17d32c56306cdff1b428f5`.
On helios, the vmlab CLI was installed from the fetched `origin/next` with
`cargo install --path . --locked` and an install root private to the card.
The vmlab-hv binary and Linux x86_64 guest agent were built from the same
checkout. The agent reported `b09d50f0`.

The inventory covers the catalog's Linux definitions. Catalog examples
duplicate those definitions as clone labs. Versions marked as prefixes are
the versions declared in the source before an install version was allocated.
Build times measure `vmlab template build` and exclude media preparation.

The builds ran serially with a private supervisor and template store.
The host config selected vmlab-hv for builds. Each clone pinned the sealed
version, explicitly selected vmlab-hv, and kept the sealed agent. The boot
check used `up --wait` and verified `ready=yes` and `hypervisor=vmlab` in
`status -v`. The probe was `exec guest -- uname -sm`. The snapshot check
created and restored an online snapshot and ran the same probe again.
Run files are under `/home/vmlab/src/stc/c245` on helios. Each executed
row has a `result.json` under `evidence/<catalog-directory>/`, or
`evidence/nixos-25.11-retry/` for the NixOS retry. The same directories
hold command output and a captured vmlab-hv process command line.

All 11 eligible templates sealed and passed the clone checks. The private
store grew from zero to 46,030,938,112 allocated bytes against the
150,000,000,000-byte cap. The clones were stopped and retained.

| Catalog directory | Version | Build | Build time | Boot to agent-ready | Exec | Snapshot round trip |
| --- | --- | --- | --- | --- | --- | --- |
| almalinux-10 | `10.2.4` | Pass | 97 s | Pass, 10 s | Pass | Pass |
| alpine-3.23 | `3.23.11` | Pass | 69 s | Pass, 90 s | Pass | Pass |
| alpine-3.23-arm64 | `3.23` prefix | Blocked, aarch64 | — | — | — | — |
| arch | `20260901.3` | Pass | 154 s | Pass, 14 s | Pass | Pass |
| debian-13 | `13.20261001.1` | Pass | 173 s | Pass, 5 s | Pass | Pass |
| debian-13-arm64 | `13.20261001` prefix | Blocked, aarch64 | — | — | — | — |
| debian-riscv64 | `13.20261001` prefix | Blocked, riscv64 | — | — | — | — |
| fedora-44 | `44.1.12` | Pass | 71 s | Pass, 10 s | Pass | Pass |
| fedora-44-arm64 | `44.1` prefix | Blocked, aarch64 | — | — | — | — |
| fedora-riscv64 | `42.20250911` prefix | Blocked, riscv64 | — | — | — | — |
| home-assistant-aarch64 | `17.3` prefix | Blocked, aarch64 | — | — | — | — |
| kali | `2026.1.4` | Pass | 260 s | Pass, 61 s | Pass | Pass |
| nixos-25.11 | `25.11.4` | Pass on retry | 159 s | Pass, 25 s | Pass | Pass |
| parrot | `7.4.0` | Pass | 354 s | Pass, 65 s | Pass | Pass |
| rocky-9 | `9.8.4` | Pass | 86 s | Pass, 7 s | Pass | Pass |
| steamos | `3.8.14` prefix | Excluded, requires GPU | — | — | — | — |
| ubuntu-24.04 | `24.04.4.3` | Pass | 591 s | Pass, 8 s | Pass | Pass |
| ubuntu-26.04 | `26.04.4` | Pass | 81 s | Pass, 8 s | Pass | Pass |
| ubuntu-arm64 | `24.04` prefix | Blocked, aarch64 | — | — | — | — |
| ubuntu-riscv64 | `24.04` prefix | Blocked, riscv64 | — | — | — | — |

## Architecture and GPU evidence

On the tested runtime, `vmlab validate` exited 1 for both aarch64 and
riscv64 scratch VMs under explicit `hypervisor = "vmlab"`. Both diagnostics
reported that this integration runs x86_64 only. The full output is in
`evidence/arch-aarch64.log` and `evidence/arch-riscv64.log` under
`/home/vmlab/src/stc/c245` on helios. Those eight catalog builds were
blocked before media preparation. The Home Assistant OS definition also
declares `agent = false`, so agent readiness and exec are unavailable for
that appliance.

At the pinned catalog commit, `steamos/vmlab.wcl` declares
`gpu { mode = "vulkan" }` and its build provision requires a Venus device.
The card excludes GPU builds.

## Download and preparation evidence

The first NixOS attempt exited 1 after it was stopped at 363 s during the
pinned ISO download, before VM launch. The download was resumed with
HTTP byte ranges and the assembled 1,625,800,704-byte ISO matched the
catalog SHA-256 before it was placed in the private cache. The first
attempt is in `evidence/nixos-25.11/build.log`, the checksum is in
`nix-media.log`, and the successful serial retry is in
`evidence/nixos-25.11-retry/`.

Parallel archive prefetch returned HTTP 429 for Kali and HTTP 428 for
Parrot. Those prefetches were stopped. The serial run used each
definition's `fetch-deps.sh` to download, verify and prepare its disk,
retaining the downloaded archives. The catalog scripts succeeded in
337 s for Kali and 612 s for Parrot. Their output is in
`evidence/kali/prep.log` and `evidence/parrot/prep.log`.
The prefetch diagnostics are in `kali-media.log` and `parrot-media.log`.
