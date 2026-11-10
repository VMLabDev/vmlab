# Release gate

R1. Do not cut a new version while any cut version is waiting for its full
e2e. Keep fixes on that frozen release branch, not a new cut from `next`.

R2. Run `just ci::hv-gate` first, following [hv-gate.md](hv-gate.md). A failure
blocks e2e and publication. Then run the full e2e suite on the pushed release
commit, not a moving branch or an uncommitted checkout.

R3. Require one full green e2e on the exact commit to be merged, with the
same tree as the eventual main merge. The only alternative is a written
waiver per failed scenario on the release card. A waiver names the scenario,
failure, risk, reason for accepting it and approving lead. List skipped or
unrun scenarios explicitly too, never count them as passes. Changes after
the run invalidate its evidence unless the tree is identical.

## Reserve helios

The release lead reserves the host for the whole release e2e, including
setup and teardown. No other heavy vmlab run (including merge queue work)
may overlap. Coordinate with the lead before starting, observe the project
calendar and hardware reservations, and never stop another worker's process.

The shared wrapper `/var/build/cache/vmlab/bin/heavy` checks
`/var/build/cache/vmlab/heavy.hold` before admitting new work. The lead creates
that file exclusively (do not overwrite another hold), recording the release,
owner and run ID. Existing runs are not stopped: wait for both `slot1.lock`
and `slot2.lock` to drain. Hold both slot locks while setting up the reservation
to close the admission race, then release them before starting e2e.

The current wrapper has no hold-owner bypass. Leaving `heavy.hold` in place
would block the release run too. Keep all other workers and merge jobs paused,
with no queued wrapper commands, remove only the hold you created, and start
the release via `terrarium run ... -- /var/build/cache/vmlab/bin/heavy ...`.
Recreate your hold once the release has its slot to prevent new admissions.
This is a coordinated reservation, not an atomic exclusive mode in the
wrapper. If the host cannot be drained and other admissions paused, do not
start. Keep the reservation through teardown, remove only your own hold,
and tell the lead when the host is free. On interruption, record the hold
owner and unfinished run on the release card before handing off.

## Exact-tree release-card checklist

Card comments and waivers are free text, so use this manual checklist rather
than treating a parser of comments as an approval gate.

A1. In a clean full checkout, record `git rev-parse HEAD` and
`git rev-parse 'HEAD^{tree}'` on the release card before running hv-gate and
full e2e. Record commands, target, run IDs, exit codes and passed, failed and
ignored counts. Upload the actual test report with `terrarium test push`,
linking the release card, version label, commit and branch. Attach the full
log and expected scenario list so missing scenarios can be identified.

A2. Read the linked report with `terrarium test ls --card <release-card>`
and `terrarium test show <run-id>`. Check its commit matches the recorded
commit and its tree. Confirm this is one complete run, not a union of partial
reruns. Require all scenarios green or the explicit per-scenario waivers in
R3. A green `ci::check` or `release-dry-run` does not replace full e2e.

A3. Prepare the main merge locally without pushing. Compare
`git rev-parse 'HEAD^{tree}'` with `git rev-parse '<tested-commit>^{tree}'`.
Require equality and verify the tested commit is the release commit being
merged. If they differ, test the new candidate tree before publishing. Record
the final merge SHA and both tree hashes on the release card. Never stamp or
edit source files between this comparison and the merge.

## Release-branch CI

Pushes to `release/**` run version-stamped CLI, guest and hypervisor builds
and prepare checksums/provenance from their real artifacts. The version is
`0.0.0-dry-run`; it is not a publishable release. Windows display compilation
runs, but catalog signing is main-only. The dry guest bundle explicitly uses
the unsigned display output and omits its generated catalog/certificate.
It is an artifact for checking packaging only, not for installation or release.
No signing identity or write permission is granted to asset preparation.
The offline `just ci::release-dry-run` tests remain part of local validation.
Signing (Windows and cosign), tags and GitHub releases run only on main pushes
with a publish trailer. Action SHA pins must remain intact.
