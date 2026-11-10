# Bounded parser fuzzing

Install cargo-fuzz and a nightly Rust toolchain. From the repository root,
run `just fuzz 60` to run qcow2, hv-proto and agent-proto for 60 seconds each.
On shared targets invoke this recipe through the heavy-run wrapper.
Each target has a checked-in seed corpus. Generated corpus entries and crash
artifacts stay in the build directory, not the checkout.

The qcow2 target uses only a temporary image and refuses backing paths.
It exercises image opening, reading and snapshot-table parsing.
The protocol targets exercise decoding and successful-message round trips.
A nonzero exit fails the recipe. Preserve any artifact and turn it into a
regression test before accepting the run.
