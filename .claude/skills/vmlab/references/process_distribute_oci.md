# Distribute a template over an OCI registry

## Purpose

Push a stored template to a registry and pull it on another machine.

## Prerequisites

- The template exists in the local store.
- You can authenticate to the registry.

## Flowchart

![diagram](../_wdoc/process_distribute_oci-diagram-1.svg)

## Steps

### Step 1: Log in

```console
$ vmlab template login ghcr.io -u myuser -p <token>
```

> [!NOTE]
> **Docker creds reused**
>
> If the machine already has a `docker login` for the registry, no separate login is needed.

Run `vmlab template login <registry>` (persists to ~/.docker/config.json).

### Step 2: Push

```console
$ vmlab template push x86_64/linux-modern@1.0 ghcr.io/owner/linux-modern:1.0
```

Push the local store ref to a registry ref. The qcow2 is chunked into zstd layers (512 MiB default). Push the same name from each arch to build a multi-arch index.

### Step 3: Pull elsewhere

```console
$ vmlab template pull ghcr.io/owner/linux-modern:1.0 --arch x86_64
```

> [!WARNING]
> **Arch is never assumed**
>
> Pulling an ambiguous multi-arch index without --arch is an error.

On the target machine, `vmlab template pull <ref>` reassembles the chunks and verifies the whole-image SHA-256 before installing to the store. A registry ref used directly in a lab is pulled on `vmlab up` if absent.

> [!TIP]
> **Verification**
>
> `vmlab template list` on the target machine shows the pulled `<arch>/<name>@<version>` ref.

## Related

- [OCI distribution](../references/concept_oci.md)

- [OCI artifact model](../references/fact_oci_artifact.md)

- [Templates](../references/concept_templates.md)

[← Back to SKILL.md](../SKILL.md)
