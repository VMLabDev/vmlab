# Patched wscript crates

`wscript-core` and `wscript-compiler` are vendored from wscript commit
`c631c6b724ebcc38a9ae53235b20bf10ba64dc51`. The only functional delta is
host-registered nominal type aliases, used to keep the historical wscript
`Vm` name compatible with `Machine`.

The remaining wscript crates continue to come from that pinned Git revision.
Remove these patches when the pinned upstream revision exposes the same alias
primitive.
