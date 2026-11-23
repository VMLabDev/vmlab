# Patched wscript crates

`wscript-core` and `wscript-compiler` are vendored from wscript commit
`ddb6d9fedc77e68736a1281eaf8a122250f40aa3`. The first functional delta is
host-registered nominal type aliases, used to keep the historical wscript
`Vm` name compatible with `Machine`.

`wscript-core` has a second delta (card #666, `docs/reviews/wscript.md` F2):
`value.rs` frees a struct, an enum, a closure and a `dyn` object through a
queue (`release`) instead of by recursion, so dropping a value nested to any
depth does not overflow the native stack. Keep it until upstream frees values
without recursion.

The remaining wscript crates come from the Git revision `Cargo.toml` pins,
`faf09e6e4c7e8ed28611993aa8dc71633a67c028`, which leaves these two crates as
they were at `ddb6d9f`. Remove these patches when the pinned upstream revision
exposes the same alias primitive.
