# Bounded parser fuzzing

Install cargo-fuzz and the pinned nightly from `ebpf/rust-toolchain.toml`. From the repository root,
run `just fuzz 60` to run every target for 60 seconds each: qcow2, hv-proto,
agent-proto, vnet-offload, net-frame and oci-document.
On shared targets invoke this recipe through the heavy-run wrapper.
`docs/reviews/fuzz-coverage.md` lists every parser of hostile input and the
target that reaches it, or that none does.

Protocol and registry text seeds are checked in. `scripts/fuzz-seeds.py` generates the
binary ones: qcow2 (empty and snapshot), agent (control and data frame), guest
frames (a DHCP lease, DNS queries, ARP, ICMP and TCP) and vnet records (TSO,
USO and a partial checksum).
Generated corpus entries and crash
artifacts stay in the build directory, not the checkout.

The qcow2 target uses only a temporary image and refuses backing paths.
It exercises image opening, reading and snapshot-table parsing.
The protocol targets exercise decoding and successful-message round trips.

`vnet_offload` is a vnet stream record as the switch reads it from a vmlab-hv
port: the guest's virtio-net header, then checksum completion and GSO
segmentation for a raw port. `net_frame` is a sequence of ethernet frames a
guest NIC sends, through the header views, the DHCP and DNS servers and the L3
rules of the userspace fabric; it leaves the NAT engine out, which would open
host sockets. `oci_document` is one document a registry serves: a manifest or
index, a config blob, a `Www-Authenticate` challenge, a reference.

The last two reach private modules of the main crate, so their bodies are in
`src/fuzzing.rs`, compiled under `--cfg fuzzing` (cargo-fuzz sets it) and in
no other build. `fuzz/` is its own workspace: `Cargo.lock` here started as a
copy of the main one, so that the fuzzed dependencies are the shipped ones,
and the `[patch]` of the main manifest is repeated in `Cargo.toml`. Keep both
in step with the main manifest.

`FUZZ_TARGETS='net_frame' just fuzz 1800` runs some targets only, and
`FUZZ_ARGS` adds libFuzzer arguments: `-fork=1 -ignore_crashes=1` keeps a long
run going past its first crash.

A nonzero exit fails the recipe. Preserve any artifact and turn it into a
regression test before accepting the run.
