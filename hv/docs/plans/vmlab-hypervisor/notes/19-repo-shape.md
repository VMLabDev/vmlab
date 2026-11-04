---
id: "19"
decision: D18
---

# Repo shape

## Question
Crate layout, and where the shared protocol and disk crates live?

## Answer
Everything lives in its own repo, `vmlab-hypervisor`, as a cargo workspace: the hypervisor binary, device crates, a protocol crate and a disk crate. vmlab depends on the protocol and disk crates as git dependencies. Once mature, the owner may merge the workspace into the vmlab repo.

## Why
Owner's call: develop to the side, merge later if it earns it. Dependency runs one way, vmlab to hypervisor.

## Consequences
- Nothing in vmlab-hypervisor may depend on the vmlab crate, so a later merge is a move, not an untangling.
- Version pinning between vmlab and the hypervisor binary needs a rule (protocol version handshake).
- When and whether to merge is Fog.
