# CLAUDE.md

Project context for Claude Code.

## Project Purpose

**vmlab** is a VM lab management tool written in Rust. This is a fresh
rewrite; the product requirements live in `docs/vmlab-prd.md` — read that first;
it is the source of truth for design and scope.

Many earlier attempts are archived under
github.com/wiltaylor/.graveyard-private — notably `vmlab_qemu` (QMP/QGA
driver crate), `vmlab_oci` (OCI registry client for VM disk images), and
`vmlab_floppy` (pure-Rust FAT for floppy images), all buried 2026-06-12.
Consult them for prior art only — the PRD overrides anything they did.

## Status

Scaffold only. Structure will be shaped by the PRD once it lands.

## Conventions

- Trunk-based development: commit directly to `main`, no branches or PRs
  unless explicitly asked.
- **just** as command runner: `just build` / `just test` / `just check`
  (lint + fmt check + tests). Justfile follows the norms in the justfile
  skill (groups, doc comments, `[private]`, noun-verb naming).
- Standard Rust toolchain: `cargo build`, `cargo test`, `cargo clippy`,
  `cargo fmt`.
