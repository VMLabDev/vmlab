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

`wscript-vm` is vendored from the revision `Cargo.toml` pins,
`faf09e6e4c7e8ed28611993aa8dc71633a67c028`, with two deltas (card #1086,
`docs/reviews/wscript-2.md` F13 and F17). Both are off unless `VmConfig` asks
for them:

- `VmConfig::memory` is a limit on the bytes of values a script holds at one
  time (`src/mem.rs`). Fuel counts instructions and one instruction can
  allocate any amount, so fuel never limited memory. The VM counts by walking
  the values reachable from the register stack each time enough new ones were
  made, and a string concatenation and a pad are refused before they allocate.
- `VmConfig::refuel` is a hook the VM asks after every host function returns.
  On `true` the tank goes back to the amount it was set to, so the fuel bounds
  the instructions between two host calls.

The crate's own tests (`tests/vm.rs`, and the unit tests in `src/ops.rs`) need
`wscript-compiler` as a dev-dependency, which a crate outside the workspace
cannot have. They are not run here, and `tests/` is not vendored. The two
deltas are tested from the main crate, in `src/scripting/runner.rs` and
`src/labd/lab.rs`.

Keep it until upstream has a memory limit and a way for a host function to
fill the tank.

The remaining wscript crates come from the same Git revision, which leaves
`wscript-core` and `wscript-compiler` as they were at `ddb6d9f`. Remove those
two patches when the pinned upstream revision exposes the same alias
primitive.
