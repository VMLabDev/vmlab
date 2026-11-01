# macOS integration verification

## Parked draft

The owner parked this work on 2026-11-01. Remaining work is tracked in #200:
https://github.com/VMLabDev/vmlab/issues/200

This branch preserves the implementation as it stood when parked. Both
`vmlab-hv-proto` and `vmlab-hv-display` retain revision
`36e90a92010ff37745f4ae053a4e2f802006571d` in Cargo.toml and Cargo.lock.
That protocol revision lacks macOS fields and variants used by this change.
The draft therefore does not build against its retained dependency pin and
is not ready to merge. The changed product documentation describes the
intended cutover, not verified support with this pin.

## Verification completed before parking

Rust compilation, lint, and integration checks used temporary Cargo patches
to a local vmlab-hypervisor checkout containing the macOS protocol. Those
patches are not committed. Their results do not validate the retained pin.

- V1: `cargo test --all-features` with the real KVM TSC probe filtered out
  passed 2,054 library tests and all four CLI integration tests. Ten existing
  tests were ignored. The exact real KVM TSC probe subsequently passed on
  its own after host capacity became available.
- V2: Clippy passed for all targets and features with warnings denied.
  Guest format, lint, and test checks passed, including Windows and macOS
  cross-target clippy.
- V3: Formatting, the edited WCL schemas, and the manual build passed.
  Both Python redaction and phase-applicability tests passed.
- V4: BPF verification passed without object changes. Its build emitted an
  LLVM shared-library loader warning.
- V5: Actual CLI checks confirmed that a template-derived macOS profile
  selects vmlab-hv, a missing host OSK is refused at validation, and explicit
  QEMU is refused with a message naming vmlab-hv. A regression also covers
  a macOS profile revealed by a deferred template pull, preserving existing
  machine handles and honoring an explicit backend choice.
- V6: The macOS scenario exited successfully with six skips when the key
  was absent, and with six skips when the template was absent. A separate
  container probe confirmed a 64-GiB outer limit and an inner user scope
  with MemoryMax=24G and MemorySwapMax=0. That container booted no VM and
  was removed.

An initial real TSC probe attempt timed out while its capacity guard waited
inside the probe's 20-second budget. The retry waited for capacity outside
that budget and passed. The host's kvm.ignore_msrs setting remained N.

## Unverified and remaining

- U1: Select and pin a published upstream revision that passes its macOS
  gates. The earlier local revision e4611a4 failed a TSC snapshot test;
  no replacement revision was supplied before this work was parked.
- U2: Run the exact `just ci::check` gate with the final dependency pin.
  The separate development checks above are not that merge gate.
- U3: Build the pinned e2e image and run `macos` with
  `--hypervisor vmlab`, the owner's template, and the private host key.
  Agent readiness, exec, copy in both directions, screenshot, online
  snapshot restore, and agent shutdown have not been exercised together
  through this vmlab integration.
- U4: Verify live OSK redaction in the resulting runtime descriptions,
  logs, startup errors, and e2e artifacts. Fake-key regressions pass;
  the live macOS integration run has not occurred.
- U5: Real host-reboot snapshot restore and unexplained upstream lifecycle
  failures remain tracked in #200. No host reboot or KVM setting change
  was performed here.

No VM is running from this task. Parking and publication of this draft do
not authorize further VM boots or further implementation work.
