# LLVM toolchain (`toolchain/llvm`)

The pinned LLVM tools of the aarch64 decoder (phase 3d, DESIGN.md G1). The
decoder of `vmlab-hv-tcg-aarch64` is generated from LLVM's AArch64 TableGen
tables. This directory pins those tables and builds the two tools that read
them:

- `llvm-tblgen --dump-json` writes every record of `AArch64.td` as JSON
  (100 MB, not committed). `vmlab-hv-tcg-a64gen` reads it.
- `llvm-mc --disassemble --show-inst` prints the record name of an encoding.
  It is the oracle for the generated decoder.

LLVM is Apache-2.0 WITH LLVM-exception. The generated files carry the notice,
and `THIRD_PARTY_NOTICES` has the entry. No LLVM code is linked into vmlab-hv.

## Layout

- `pins.env`: the base image digest, the LLVM tag and commit, and the sha256
  of the JSON dump.
- `td.sha256`: the sha256 of each `.td` file that the dump reads (98 files),
  in `sha256sum` format with paths from the root of llvm-project.
- `Containerfile`: fetches llvm-project at the pinned commit and builds
  `llvm-tblgen` and `llvm-mc` for the AArch64 target only.
- `scripts/dump.sh`: runs the dump and checks every file that tblgen read
  against `td.sha256`. It fails on a file that is not listed.
- `scripts/fetch-src.sh`: fetches the pinned sources for a host that has the
  LLVM tools of the pinned version already.
- `scripts/llvm-mc-container.sh`: runs the `llvm-mc` of the image.

## Recipes (in `hv/`)

Each recipe takes `llvm=container` (the default: build and use the image) or
`llvm=host` (use `llvm-tblgen` and `llvm-mc` from `PATH` on the pinned
sources). The JSON must have the pinned sha256 in both cases, so a host tool
of another version fails the recipe.

| Recipe | Effect |
|---|---|
| `just tcg-a64-generate` | Dump, then write `src/generated/decode.rs`, `src/generated/sysreg.rs` and `tests/vectors.txt` of the crate. |
| `just tcg-a64-generate-check` | Dump, regenerate in memory, and compare the three committed files. |
| `just tcg-a64-decode-check [count] [seed]` | Compare the decoder with `llvm-mc` on random words of the selected sets (10 million by default). |
| `just tcg-a64-pin` | After a change of the pin: write `td.sha256` again and print the new sha256 of the dump for `pins.env`. |

`just ci::check` needs no LLVM: the crate tests decode the committed vector
file.

## What steers the generator

Three hand-written tables in `crates/vmlab-hv-tcg-aarch64/tables/`:

- `sets.toml`: which instruction sets are emitted, by LLVM feature.
- `overrides.toml`: the winner of two overlapping records of equal
  specificity.
- `constraints.toml`: conditions that the fixed bits of a record do not
  express. LLVM's C++ decoder methods are not run, so each condition that
  they check is restated there, with the method that it comes from. A
  condition that only a lockstep run against QEMU found says so.

The raw value of a field follows the bit numbers of the LLVM record. A field
that LLVM declares as bits 5 to 4 of an operand has the values 0, 16, 32 and
48 (the `shift` of a move wide immediate is an example).

A bit that LLVM marks `SoftFail` is free in the generated decoder, as it is in
LLVM's: a word with any value there matches the record.

## Changing the pin

Edit `LLVM_TAG` and `LLVM_COMMIT` in `pins.env`, run `just tcg-a64-pin`, put
the printed sha256 into `pins.env`, then run `just tcg-a64-generate` and
`just tcg-a64-decode-check`. A new overlap or a new decoder method shows up as
a failed generation or as a difference against `llvm-mc`.
