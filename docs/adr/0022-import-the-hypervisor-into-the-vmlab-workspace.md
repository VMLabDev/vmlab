# ADR-0022: Import the hypervisor into the vmlab workspace

## Status

Accepted, 2026-11-04. Amends ADR-0021's repository and dependency arrangement.

## Context

Separate source repositories require a hypervisor merge, protocol repin and
second vmlab gate for one cross-project change. The hypervisor's integration
tests depend back on vmlab's guest protocol crates. Binding design plans live
outside version control in the old checkout.

The owner approved the monorepo-merge map and specification, decisions D1,
D3, D5, D7, D8, D10, D12 and D13. Runtime hypervisor selection stays unchanged.

## Decision

We will import the hypervisor's complete main-branch history under `hv/`,
without source tags, and land the import PR with a merge commit. A committed
script clones the source fresh, filters only the temporary clone and merges
the resulting history. Rewiring commits follow the import merge and can be
replayed after rebuilding the import against the latest source main.

The root Cargo workspace contains vmlab and `hv/crates/*`. The root package
is an implicit member. Guest workspaces, eBPF and the TAP helper stay excluded.
Protocol dependencies use paths. One root lockfile and the pinned 1.99.0
toolchain govern host crates. Hypervisor crates keep their own lint policy.

Builds, release packaging and the e2e image compile the in-tree hypervisor.
`ci::check` includes formatting, clippy and tests with guest-booting and
host-asset acceptance cases ignored. The orchestrator runs `ci::hv-gate`
locally once per batch. The native virglrenderer cache stays outside `target/`.

Binding hypervisor plans are committed under `hv/docs/plans/`, without agent
prompts or logs. Root MIT and third-party notices cover the imported source.
The hypervisor and native viewer remain separate binaries.

## Consequences

A protocol change can build both sides in one branch. Original authors and
history remain visible under `hv/`; preserving that history requires merge
landing rather than squash or rebase. Import refreshes may need conflict
resolution when newer source changes overlap the rewiring commits.

The workspace adds native build dependencies to the light merge bar. Guest
tests still need a separate orchestrator gate and local assets. The virgl
pin, native dependency lists and iPXE implementations remain duplicated until
the agreed follow-up. Windows 3D import, issue transfers and old-repository
archival belong to later work.
