---
name: vmlab
description: "Reference and processes for vmlab. A declarative QEMU/KVM VM-lab orchestrator: labs and virtual networks declared in WCL, reusable disk templates built locally or distributed over OCI registries, and guest automation written in wscript. Use when working with vmlab or answering questions about it."
allowed-tools:
  - Bash
  - Read
disable-model-invocation: false
metadata:
  wskill_schema_version: 1.3.0
---

# vmlab

<overview>

A declarative QEMU/KVM VM-lab orchestrator: labs and virtual networks declared in WCL, reusable disk templates built locally or distributed over OCI registries, and guest automation written in wscript.

**Upstream version:** `1.3`. If the real upstream has moved past this, the skill may be stale — bump `topic.version` and re-verify (see the update workflow).

vmlab orchestrates single-host VM labs: labs (VMs + virtual networks) are declared in WCL (`vmlab.wcl`), disk templates are built and stored locally or distributed via OCI registries, and automation is written in wscript scripts that drive guests (power, exec, keystrokes, screen matching, OCR).

OCI containers join labs as micro-VM machines (`container {}`), config-weave playbooks converge guests declaratively (`playbook {}`), and the `vmlab-web` console (REST + WebSocket API, visual designer, guest web-page proxy) manages labs from the browser.

A two-tier daemon (supervisor `vmlabd` + one daemon per lab) is auto-started by the CLI. This skill captures the full reference as data.

</overview>

## Parameters

<variables>

- `${CLAUDE_SKILL_DIR}`: path to this skill's directory (its `scripts/`, `assets/`, and `references/` live here).

- `$ARGUMENTS`: The vmlab topic, CLI subcommand, WCL attribute, or wscript API method to look up. How to determine: Take it from the user's request. If empty, summarise the reference and ask what they need.

</variables>

<boundaries>

<always>

- Run `vmlab validate` after editing `vmlab.wcl` and before `vmlab up`.
- For multi-step guest automation, write a wscript script (`vmlab script x.ws`) instead of chaining many `vmlab exec` calls.
- Cite the exact reference page when answering.

</always>

<ask>

- Which lab or template is meant when multiple `vmlab.wcl` files or store versions are plausible targets.

</ask>

<never>

- Run `vmlab destroy` or `vmlab template rm` without explicit user say-so — both delete state (clones / store images).
- Invent WCL attributes or wscript functions: everything that exists is in the reference; if it's not there, check `src/config/schema.wcl` or `src/scripting/mod.rs` before using it.

</never>

</boundaries>

## Reference

### Getting started

_What vmlab is, the three artifacts you touch, and the everyday lifecycle._

Orientation for newcomers: the lab file, templates and wscript in one screen, then the golden path from `validate` to `destroy`.

- [Start here](references/concept_start_here.md)
- [vmlab.wcl](references/entity_vmlab_wcl.md)
- [Bring a lab up and tear it down](references/process_golden_path.md)

### How vmlab works

_The moving parts: daemons, QEMU processes, guest channels, and where state lives on disk._

The big picture first, then the daemon model and the on-disk layout — machine-wide state versus each lab's disposable `.vmlab/`.

- [How vmlab fits together](references/concept_architecture.md)
- [Daemon model](references/concept_daemon_model.md)
- [Filesystem layout](references/fact_paths_table.md)
- [.vmlab/](references/entity_dot_vmlab.md)
- [Template store](references/entity_template_store.md)

### Labs & networking

_Declare machines and the virtual networks that connect them._

The `vmlab.wcl` topology surface: the lab, VM and container blocks, shared folders and media, then the network fabric — segments, DHCP/DNS, routing, NAT and traffic rules.

- [lab {} block](references/entity_labs.md)
- [vm {} block](references/entity_vms.md)
- [container {} block](references/entity_container_block.md)
- [nic {} block](references/entity_nic_block.md)
- [share {} block](references/entity_shares.md)
- [media {} block](references/entity_media.md)
- [Networking model](references/concept_networking.md)
- [segment {} block](references/entity_segment_block.md)
- [segment {} sub-blocks](references/fact_segment_subblocks.md)

### Templates & distribution

_Build reusable disk images and move them between machines._

Templates end to end: what they are, declaring and building them, how clones boot from them, scratch VMs, and pushing/pulling over OCI registries.

- [Templates](references/concept_templates.md)
- [template {} block](references/entity_template_block.md)
- [source {} build source](references/entity_template_sources.md)
- [Template build flow](references/concept_template_builds.md)
- [Build a disk template](references/process_build_template.md)
- [Linked clones](references/concept_linked_clones.md)
- [Scratch VMs](references/concept_scratch_vms.md)
- [OCI distribution](references/concept_oci.md)
- [OCI artifact model](references/fact_oci_artifact.md)
- [Distribute a template over an OCI registry](references/process_distribute_oci.md)

### Lab containers

_OCI containers as first-class lab machines, each in its own micro-VM._

The container story (PRD §18): why micro-VMs, the `container {}` block, and the wscript `Container` handle.

- [Lab containers](references/concept_lab_containers.md)
- [container {} block](references/entity_container_block.md)
- [Container](references/entity_container_api.md)

### Automation

_Drive guests: provision scripts, event handlers, playbooks, and the wscript language + API underneath._

Start with the overview — which of the four automation surfaces fits which job — then the wscript language, the host API, and the provision/event/playbook machinery.

- [Automating labs](references/concept_automation_overview.md)

#### The wscript language

_The statically typed scripting language: types, functions, matching, modules, stdlib._
- [wscript: overview](references/concept_wscript_overview.md)
- [wscript: types & values](references/concept_wscript_types.md)
- [wscript: functions & control flow](references/concept_wscript_functions.md)
- [wscript: pattern matching & errors](references/concept_wscript_matching.md)
- [wscript: modules & prelude](references/concept_wscript_modules.md)
- [wscript: List & Map methods](references/fact_wscript_collections.md)
- [wscript: string methods](references/fact_wscript_strings.md)
- [wscript: not in v1](references/fact_wscript_limits.md)

#### The vmlab API

_The Lab / Vm / Segment handles and their method groups, plus the result types._
- [Lab](references/entity_lab_api.md)
- [Vm](references/entity_vm_api.md)
- [Vm: lifecycle & state methods](references/fact_vm_lifecycle.md)
- [Vm: snapshot methods](references/fact_vm_snapshots.md)
- [Vm: keyboard & mouse methods](references/fact_vm_input.md)
- [Vm: screen, image matching & OCR methods](references/fact_vm_vision.md)
- [Vm: guest agent methods (exec, files, terminal, stats)](references/fact_vm_agent.md)
- [vmlab-agent](references/entity_vmlab_agent.md)
- [Segment](references/entity_seg_api.md)
- [Match](references/entity_match_type.md)
- [ExecResult](references/entity_exec_result_type.md)

#### Provisions, events & playbooks

_The declared automation: provision scripts on `up`, lifecycle event handlers, and config-weave playbooks._
- [Provisions & event handlers](references/concept_provisions.md)
- [provision {} block](references/entity_provision_block.md)
- [on "event" {} handler](references/entity_on_handler.md)
- [Event](references/entity_event_type.md)
- [Lifecycle events](references/fact_events.md)
- [Playbooks (config-weave)](references/concept_playbooks.md)
- [playbook {} block](references/entity_playbook_block.md)

### The web console

_Manage labs from the browser: the vmlab-web server, the console UI, its API, and proxied guest web pages._

Everything around `vmlab-web`: the console tour, launching and securing the server, the REST + WebSocket API, and `web {}` blocks that proxy guest HTTP UIs into the console.

- [The web console](references/concept_web_console.md)
- [vmlab-web](references/entity_vmlab_web.md)
- [Serve the web console](references/process_serve_web_console.md)
- [vmlab-web: the REST + WebSocket API](references/fact_web_api.md)
- [web {} block](references/entity_web_block.md)

### Operations & hosting

_Run vmlab anywhere: host config, profiles, containers, WSL2, and the network fast path._

Host-side concerns: the optional host config, guest OS profiles, hosting vmlab itself in a container or on WSL2, eBPF acceleration, and what `vmlab validate` checks.

- [Host config](references/concept_host_config.md)
- [Guest OS profiles](references/concept_profiles.md)
- [Shipped guest OS profiles](references/fact_profiles_table.md)
- [Running vmlab in a container](references/concept_containers.md)
- [Run vmlab in a container](references/process_run_in_container.md)
- [WSL2](references/concept_wsl2.md)
- [Network fast path (eBPF)](references/concept_fastpath.md)
- [What `vmlab validate` checks](references/fact_validate_checks.md)

### Appendix: reference tables

_The full vmlab.wcl schema and other lookup tables._

Lookup material: the complete reflected `vmlab.wcl` schema (every block and attribute) and the key-chord names. The CLI reference and glossary follow as their own chapters.

- [The vmlab.wcl schema](references/fact_schema_reference.md)
- [Keyboard chord names](references/fact_key_chords.md)

- [CLI reference](references/cli_ref.md) — every `vmlab` subcommand, its arguments and switches

- [Glossary](references/glossary_ref.md) — terms and definitions

- [Related skills](references/related_ref.md) — cross-references to other wskills

## Views

Beyond this skill, the wskill ships these views — build them with `just render` in the wskill folder:

- **book** (`wdoc/book/main.wcl`)
- **ai skill** (`wdoc/skill/main.wcl`)
- **presentation** — vmlab in a nutshell — an overview deck. (`wdoc/presentation/main.wcl`)
- **training** — Learn vmlab — a hands-on lesson series. (`wdoc/training/main.wcl`)
