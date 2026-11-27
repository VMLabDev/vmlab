//! Helpers: the work of an instruction that a lifter does not express in
//! scalar operations of the graph (DESIGN.md G3d, R10). A lifter calls a
//! helper through `Builder::call`, and the interpreter loop runs its function.

pub mod crypto;
pub mod fp;
pub mod simd;
pub mod softfloat;

/// The function that compiled code calls with the words of the arguments
/// of `helper` in registers, for the helpers that have one (3e DESIGN.md
/// J13): each operation of `crypto`.
pub fn in_registers_of(
    helper: &vmlab_hv_tcg_ir::native::Helper,
) -> Option<vmlab_hv_tcg_jit::RegisterFn> {
    crypto::in_registers_of(helper)
}
