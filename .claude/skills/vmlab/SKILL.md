---
name: vmlab
description: Using vmlab, the declarative QEMU/KVM lab orchestrator built in this repo. Load when writing or editing vmlab.wcl lab/template definitions, running vmlab CLI commands, writing wisp provision/handler scripts, building templates, pushing/pulling templates to OCI registries, configuring lab networking, or running vmlab in a container.
user-invocable: false
---

<overview>
vmlab orchestrates single-host VM labs: labs (VMs + virtual networks) are
declared in WCL (`vmlab.wcl`), reusable disk templates are built and stored
locally or distributed via OCI registries, and automation is written in wisp
scripts that drive guests (power, exec, keystrokes, screen matching, OCR).
Two-tier daemon (supervisor `vmlabd` + one daemon per lab), auto-started by
the CLI. This skill is a usage reference; `docs/vmlab-prd.md` is the binding
spec if anything here disagrees.
</overview>

<variables>
- `${CLAUDE_SKILL_DIR}`: Path to this skill's directory.
</variables>

<routing>
Read ONLY the reference file(s) the task needs:

| Task | Read |
|---|---|
| Run/choose CLI commands (lifecycle, snapshots, logs, exec, console) | `${CLAUDE_SKILL_DIR}/reference/cli.md` |
| Write/edit a lab definition (`lab {}` — VMs, segments, NICs, shares, handlers) | `${CLAUDE_SKILL_DIR}/reference/lab-config.md` |
| Build a template (`template {}` blocks, sources, store, media) | `${CLAUDE_SKILL_DIR}/reference/templates.md` |
| Push/pull templates to an OCI registry, registry refs in labs | `${CLAUDE_SKILL_DIR}/reference/oci.md` |
| Write a wisp provision/handler script — vmlab API (Lab/Vm/Segment) | `${CLAUDE_SKILL_DIR}/reference/wisp-api.md` |
| wisp language syntax itself (types, match, Result, containers) | `${CLAUDE_SKILL_DIR}/reference/wisp-language.md` |
| Run vmlab in docker/podman, WSL2 hosts | `${CLAUDE_SKILL_DIR}/reference/container.md` |
| Host config, profiles, file paths, daemon model | `${CLAUDE_SKILL_DIR}/reference/host-config.md` |
</routing>

<golden-path>
```sh
vmlab validate   # schema + semantic checks, no side effects — always after editing WCL
vmlab up         # clone, boot, run provision scripts
vmlab status     # VM/segment state, IPs, ready flags
vmlab down       # graceful stop; clones retained (destroy deletes them)
```
A worked example lab lives at `examples/ad-lab/` (lab WCL + wisp scripts).
</golden-path>

<boundaries>
<always>
- Run `vmlab validate` after editing `vmlab.wcl` and before `vmlab up`.
- For multi-step guest automation, write a wisp script (`vmlab run x.wisp`)
  instead of chaining many `vmlab exec` calls.
</always>
<ask>
- Which lab or template is meant when multiple `vmlab.wcl` files or store
  versions are plausible targets.
</ask>
<never>
- Run `vmlab destroy` or `vmlab template rm` without explicit user say-so —
  both delete state (clones / store images).
- Invent WCL attributes or wisp functions: everything that exists is in the
  reference files; if it's not there, check `src/config/schema.wcl` or
  `src/scripting/mod.rs` before using it.
</never>
</boundaries>
