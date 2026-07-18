# The web console

_The browser UI served by vmlab-web: visual lab designer, per-machine consoles and terminals, template builds, playbooks, a whole-lab file editor, and proxied guest web pages._

The console (served by [vmlab-web](../references/entity_vmlab_web.md)) is a full alternative to
the CLI for day-to-day lab work. The sidebar lists the current lab's sections;
the topbar holds the lab switcher (with **New lab…**), a network fast-path
badge, the **Help** button (opens this reference as an in-app tab), theme
toggle and sign-out.


| Where | What you do there |
| --- | --- |
| Lab overview | Power the lab up/down, watch per-machine state and events, and launch declared guest web pages from their cards |
| Lab editor — Overview | The visual designer: an SVG topology canvas (VMs, containers, segments, routers, playbook nodes) with a full-schema inspector; edits are span-addressed surgical rewrites of `vmlab.wcl` |
| Lab editor — Files | Whole-lab-directory tree editor (create/edit/rename/delete any lab file), including playbook folders with config-weave package buttons and a repos modal |
| Lab editor — Logs | The lab's JSON-line event stream, live |
| Machine page | Tabs: **Console** (live VNC desktop), **Terminal** (vmlab-agent shell), **Log**, and **Playbook** when one targets the machine; plus screenshot, key/clipboard and metrics widgets |
| Container page | Same shape: **Console**, **Recovery terminal**, **Log**, **Playbook** |
| Templates | Build templates with live progress consoles (VNC into the build VM), stop builds, and publish/pull against OCI registries |
| Web | Aggregate tab of all open [guest web pages](../references/entity_web_block.md), proxied into same-origin iframes |
| Segment inspector — DNS | Live DNS registrations while the lab runs; expected registrations when it is powered off |

Everything the console does goes through the [REST + WebSocket API](../references/fact_web_api.md),
so it can also be scripted directly. The designer's **New lab…** flow scaffolds a
managed lab directory; existing labs opened from disk are edited in place.


## Related

- [vmlab-web](../references/entity_vmlab_web.md)

- [vmlab-web: the REST + WebSocket API](../references/fact_web_api.md)

- [Playbooks (config-weave)](../references/concept_playbooks.md)

- [Running vmlab in a container](../references/concept_containers.md)

- [Templates](../references/concept_templates.md)

[← Back to SKILL.md](../SKILL.md)
