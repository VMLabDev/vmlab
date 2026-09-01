# Patched wscript crates

`wscript-core` and `wscript-compiler` are vendored from wscript commit
`ddb6d9fedc77e68736a1281eaf8a122250f40aa3`. The only functional delta is
host-registered nominal type aliases, used to keep the historical wscript
`Vm` name compatible with `Machine`.

The remaining wscript crates continue to come from that pinned Git revision.
Remove these patches when the pinned upstream revision exposes the same alias
primitive.
