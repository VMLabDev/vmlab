//! Files that `vmlab-hv-tcg-a64gen` writes from the LLVM AArch64 tables
//! (DESIGN.md G1). `just tcg-a64-generate` in `hv/` makes them again, and
//! `just tcg-a64-generate-check` compares them with the pinned tables.
//! rustfmt leaves them as the generator wrote them.

#[rustfmt::skip]
pub mod decode;
#[rustfmt::skip]
pub mod sysreg;
