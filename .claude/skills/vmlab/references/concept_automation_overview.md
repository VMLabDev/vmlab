# Automating labs

_The four automation surfaces — provision scripts, event handlers, playbooks, and ad-hoc drives — and how to pick between them._

Everything in a lab can be automated, but there are four distinct surfaces —
choose by \*when\* the automation runs and \*what shape\* it takes:

| Surface | Runs | Shape | Reach for it when |
| --- | --- | --- | --- |
| [Provision scripts](../references/concept_provisions.md) (`provision {}`) | On `vmlab up`, in declaration order | Imperative [wscript](../references/concept_wscript_overview.md) | Sequenced setup: install, configure, reboot, verify |
| [Event handlers](../references/entity_on_handler.md) (`on "event" {}`) | When a lifecycle event fires | Imperative wscript | Reactions: collect dumps on `vm.crashed`, alert on disk-low |
| [Playbooks](../references/concept_playbooks.md) (`playbook {}`) | On `up` (interleaved with provisions) and on demand | Declarative config-weave | Desired-state config: packages, files, services, domain joins — with drift check |
| Ad-hoc | Whenever you run it | `vmlab script x.ws`, or one-shot `exec` / `shell` / `cp` | Experiments, debugging, one-off tasks |

The imperative surfaces all land in the same place: a wscript `main(lab)`
holding a [`Lab` handle](../references/entity_lab_api.md), from which
[`Vm`](../references/entity_vm_api.md) and [`Container`](../references/entity_container_api.md) handles expose
power, exec, file transfer, terminals, snapshots, keystrokes, screen matching
and OCR. Multi-step guest work belongs in a script, not a chain of
`vmlab exec` calls — scripts get send/expect terminals, retries and real
control flow.

Underneath, guest access rides the [vmlab-agent](../references/entity_vmlab_agent.md) channel
(virtio-serial, no guest network needed), falling back to the QEMU guest
agent on templates that predate it. Vision-based automation (screen matching,
OCR) needs no agent at all — it reads the display, which is what makes even
vintage guests scriptable.

The surfaces compose: a typical lab declares playbooks for steady-state
config, provisions for the sequenced glue playbooks can't express, and a
crash handler or two — then you poke at the result with `vmlab shell`.


## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [Playbooks (config-weave)](../references/concept_playbooks.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [Lab](../references/entity_lab_api.md)

- [Vm](../references/entity_vm_api.md)

- [vmlab-agent](../references/entity_vmlab_agent.md)

[← Back to SKILL.md](../SKILL.md)
