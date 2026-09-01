# Playbooks (config-weave)

_Declarative guest configuration via config-weave: playbook {} blocks apply on `vmlab up` interleaved with provisions, re-run on demand with `vmlab playbook check|apply`, and drive template builds._

Where wscript provisions \*drive\* a guest imperatively, a **playbook** declares
what the guest should look like and lets [config-weave](https://github.com/Configweave)
converge it — package installs, files, services, domain joins — with drift
detection and idempotent re-runs. vmlab integrates config-weave natively:
[`playbook {}` blocks](../references/entity_playbook_block.md) declared inside a `vm {}` or
`container {}` are applied during `vmlab up`, interleaved with that machine's
`provision {}` blocks in declaration order, so imperative and declarative steps
can hand off to each other. Playbooks push
over the vmlab-agent channel, so they need agent-baked templates and work with
no guest network; guest reboots demanded by a step (Windows feature installs,
domain joins) are handled automatically.


```console
vmlab playbook list                 # declarations + any in-flight runs
vmlab playbook check dc01           # report drift, change nothing (re-pushes first)
vmlab playbook apply dc01           # converge; auto-reboots when a step demands it
```

Exit codes mirror config-weave: `0` converged/clean, `1` step error,
`2` validation failure, `3` reboot still required after bounded retries.
`--playbook <path>` / `--play <name>` disambiguate when a machine declares
several.

Targeting is structural: the machine that declares the block is the one the
play converges. That is also where its **variables** live — \`var "name"
{ value = "…" }` children become `--var name=value\` for that machine's run
only, so one play can converge several machines with different hostnames,
credentials or paths.

Playbooks also run inside [template builds](../references/concept_template_builds.md): a
`template {}` may declare `playbook {}` blocks that apply to the build VM,
again interleaved with provisions, with steps streaming as structured build
progress.


## Related

- [Automating labs](../references/concept_automation_overview.md)

- [playbook {} block](../references/entity_playbook_block.md)

- [Provisions & event handlers](../references/concept_provisions.md)

- [Template build flow](../references/concept_template_builds.md)

[← Back to SKILL.md](../SKILL.md)
