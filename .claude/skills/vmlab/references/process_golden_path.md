# Bring a lab up and tear it down

## Purpose

The everyday lifecycle: validate, boot with provisions, inspect, stop.

## Prerequisites

- A vmlab.wcl exists (found by walking up from cwd).
- Referenced templates are in the store or are pullable registry refs.

## Flowchart

![diagram](../_wdoc/process_golden_path-diagram-1.svg)

## Steps

### Step 1: Validate

```console
$ vmlab validate
```

> [!NOTE]
> **Always validate first**
> Schema + semantic checks with no side effects, including a full wscript type-check of every provision/handler. Run it after every edit.

Run `vmlab validate`. Fix any schema or semantic error before booting.

### Step 2: Bring it up

```console
$ vmlab up
```

Run `vmlab up` to create linked clones, boot the VMs and run provisions in declaration order. A provision failure fails the run. Pass VM names to bring up only a subset.

### Step 3: Inspect

```console
$ vmlab status
$ vmlab logs -f
```

`vmlab status` shows VM/segment state, IPs and ready flags. `vmlab logs -f` follows lab events; `vmlab logs <vm>` shows one VM's QEMU/serial output.

### Step 4: Stop

```console
$ vmlab down          # graceful; clones retained
$ vmlab destroy       # stop + DELETE clones and lab-local state
```

> [!WARNING]
> **down vs destroy**
> `down` keeps the linked clones so you can resume. `destroy` deletes them and lab-local `.vmlab/` state — only when you mean it.

Use `vmlab down` for a graceful stop that retains clones. Use `vmlab destroy` only to delete the clones and lab-local state entirely.

> [!TIP]
> **Verification**
> `vmlab status` reports the expected VM/segment states and `ready` flags; provisions completed without failing `vmlab up`.

## Related

- [Start here](../references/concept_start_here.md)

- [lab {} block](../references/entity_labs.md)

- [vm {} block](../references/entity_vms.md)

- [Provisions & event handlers](../references/concept_provisions.md)

- [What `vmlab validate` checks](../references/fact_validate_checks.md)

[← Back to SKILL.md](../SKILL.md)
