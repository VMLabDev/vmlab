# Playbooks (config-weave)

_Declarative guest configuration via config-weave: playbook {} blocks apply on `vmlab up` interleaved with provisions, re-run on demand with `vmlab playbook check|apply`, and drive template builds._

Where wscript provisions \*drive\* a guest imperatively, a **playbook** declares
what the guest should look like and lets [config-weave](https://github.com/Configweave)
converge it — package installs, files, services, domain joins — with drift
detection and idempotent re-runs. vmlab integrates config-weave natively:
[`playbook {}` blocks](../references/entity_playbook_block.md) in a `lab {}` are applied during
`vmlab up`, interleaved with `provision {}` blocks in declaration order, so
imperative and declarative steps can hand off to each other. Playbooks push
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
`--playbook <path>` / `--play <name>` disambiguate when several target one
machine.

Playbooks also run inside [template builds](../references/concept_template_builds.md): a
`template {}` may declare `playbook {}` blocks that apply to the build VM,
again interleaved with provisions, with steps streaming as structured build
progress. In the [web console](../references/concept_web_console.md), machine pages grow a
**Playbook** tab (check/apply with live output), the designer shows playbook
nodes on the canvas, and the Files tab edits playbook folders directly —
including config-weave package search/add buttons and a repos manager.


## Related

- [Automating labs](../references/concept_automation_overview.md)

- [playbook {} block](../references/entity_playbook_block.md)

- [Provisions & event handlers](../references/concept_provisions.md)

- [Template build flow](../references/concept_template_builds.md)

- [The web console](../references/concept_web_console.md)

[← Back to SKILL.md](../SKILL.md)
