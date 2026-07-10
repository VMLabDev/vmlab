# vmlab — CLI reference

The vmlab command-line interface: lab lifecycle, per-VM power, snapshots, guest execution and scripting, console and logs, template builds and OCI distribution, and the daemons.

## vmlab validate

WCL schema + semantic validation of the lab, with no side effects. Run after editing vmlab.wcl and before `up`.

```console
vmlab validate
```

## vmlab up

Create linked clones, boot the VMs (a subset is optional), and run provision scripts in declaration order.

| Argument | Required | Description |
| --- | --- | --- |
| vm… | optional | Optional VMs to bring up; omit for the whole lab. |

```console
vmlab up
vmlab up dc01 client01
```

## vmlab down

Graceful stop (guest agent → ACPI → kill). Linked clones are retained.

| Argument | Required | Description |
| --- | --- | --- |
| vm… | optional | Optional VMs to stop; omit for the whole lab. |

| Switch | Value | Description |
| --- | --- | --- |
| --force | — | Skip the graceful ladder and kill immediately. |

```console
vmlab down
```

## vmlab destroy

Stop the lab and DELETE its linked clones, lab-local state and dynamic network config. Destructive.

```console
vmlab destroy
```

## vmlab status

Show lab / VM / segment state, IPs and ready flags.

```console
vmlab status
```

## vmlab vm

Per-VM power control.

### vmlab vm start

Start a single VM.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab vm start dc01
```

### vmlab vm stop

Stop a single VM gracefully.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

| Switch | Value | Description |
| --- | --- | --- |
| --force | — | Kill immediately instead of the graceful ladder. |

```console
vmlab vm stop dc01
```

### vmlab vm restart

Restart a single VM.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab vm restart dc01
```

## vmlab container

Per-container lifecycle, exec and logs (OCI containers run as micro-VMs, PRD §18).

### vmlab container start

Start a single container.

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

```console
vmlab container start web
```

### vmlab container stop

Stop a container gracefully (stop signal → guest shutdown → kill).

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

| Switch | Value | Description |
| --- | --- | --- |
| --force | — | Kill immediately. |

```console
vmlab container stop web
```

### vmlab container restart

Restart a single container.

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

```console
vmlab container restart web
```

### vmlab container destroy

Stop and delete the container's writable overlay + pinned image digest; named volumes survive. A later `up` re-resolves the image.

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

```console
vmlab container destroy web
```

### vmlab container exec

Run a command inside the container; exits with the command's exit code.

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |
| cmd… | required | Command + args (after --). |

| Switch | Value | Description |
| --- | --- | --- |
| --timeout | — | Seconds (default 120). |

```console
vmlab container exec web -- nginx -t
```

### vmlab container logs

Container stdout/stderr (the serial console log).

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

| Switch | Value | Description |
| --- | --- | --- |
| -f, --follow | — | Stream as it grows. |
| -n, --lines | — | Tail length (default 100). |

```console
vmlab container logs web -f
```

### vmlab container ip

The container's DHCP lease (errors on an air-gapped container).

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

```console
vmlab container ip web
```

## vmlab snapshot

Online (running: disk+RAM+device state) or offline (powered off: disk only) snapshots, per current power state. Restoring an online snapshot resumes running.

### vmlab snapshot create

Create a snapshot. Omitting --vm snapshots every VM in the lab (best-effort, not coordinated).

| Argument | Required | Description |
| --- | --- | --- |
| name | required | Snapshot name. |

| Switch | Value | Description |
| --- | --- | --- |
| --vm | VM | Target a single VM; omit for the whole lab. |

```console
vmlab snapshot create clean --vm dc01
```

### vmlab snapshot restore

Restore a snapshot.

| Argument | Required | Description |
| --- | --- | --- |
| name | required | Snapshot name. |

| Switch | Value | Description |
| --- | --- | --- |
| --vm | VM | Target a single VM; omit for the whole lab. |

```console
vmlab snapshot restore clean --vm dc01
```

### vmlab snapshot list

List a VM's snapshots (name, taken_at, power_state).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab snapshot list dc01
```

### vmlab snapshot delete

Delete a VM's snapshot.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| name | required | Snapshot name. |

```console
vmlab snapshot delete dc01 clean
```

## vmlab exec

Run a command in a guest via the guest agent and print its stdout/stderr.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| cmd… | required | Command and arguments, after `--`. |

```console
vmlab exec dc01 -- ipconfig /all
```

## vmlab script

Run an ad-hoc wscript script against the running lab (entry point `fn main(lab: Lab)`).

| Argument | Required | Description |
| --- | --- | --- |
| script.ws | required | Path to the wscript file. |

```console
vmlab script scripts/test.ws
```

## vmlab console

Launch a VNC viewer for a VM (host config `viewer` command), or forward VNC over localhost TCP.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

| Switch | Value | Description |
| --- | --- | --- |
| --tcp | — | Forward VNC over a localhost TCP port instead of launching the viewer (WSL2 / remote viewers). |

```console
vmlab console dc01
vmlab console dc01 --tcp
```

## vmlab logs

Print JSON-line logs: lab events, or one VM's QEMU/serial output.

| Argument | Required | Description |
| --- | --- | --- |
| \[lab/\]\[vm\] | optional | Lab events (default) or a specific VM's logs. |

| Switch | Value | Description |
| --- | --- | --- |
| -f, --follow | — | Follow the log as it grows. |
| -n, --lines | N | Lines of history (default 100). |

```console
vmlab logs -f
vmlab logs dc01 -n 50
```

## vmlab template

Build, manage, and distribute disk templates. Local refs are `<arch>/<name>[@<version>]`; remote refs are `host/repo:tag`.

### vmlab template build

Build the template {} blocks in a file (default ./vmlab.wcl). Name one to build just it.

| Argument | Required | Description |
| --- | --- | --- |
| name | optional | A single template to build; omit to build all. |

| Switch | Value | Description |
| --- | --- | --- |
| -f, --file | FILE | WCL file containing the template {} blocks (default ./vmlab.wcl). |

```console
vmlab template build
vmlab template build -f templates.wcl linux-modern
```

### vmlab template list

List templates in the store.

| Switch | Value | Description |
| --- | --- | --- |
| --json | — | Emit the full metadata array (ref, sizes in bytes, RFC 3339 created). |

```console
vmlab template list --json
```

### vmlab template rm

Remove a template from the store. The exact version is required.

| Argument | Required | Description |
| --- | --- | --- |
| <arch>/<name>@<version> | required | Exact store ref including version. |

| Switch | Value | Description |
| --- | --- | --- |
| --force | — | Remove even if linked clones back it. |

```console
vmlab template rm x86_64/linux-modern@1.0
```

### vmlab template export

Export a stored template to a portable archive.

| Argument | Required | Description |
| --- | --- | --- |
| <arch>/<name>\[@<ver>\] | required | Store ref to export. |
| out.tar.zst | required | Output archive path. |

```console
vmlab template export x86_64/linux-modern@1.0 linux.tar.zst
```

### vmlab template import

Import a template archive into the store.

| Argument | Required | Description |
| --- | --- | --- |
| archive.tar.zst | required | Archive to import. |

| Switch | Value | Description |
| --- | --- | --- |
| --overwrite | — | Replace an existing store entry. |

```console
vmlab template import linux.tar.zst
```

### vmlab template login

Log in to an OCI registry (persists to ~/.docker/config.json; existing docker logins are reused).

| Argument | Required | Description |
| --- | --- | --- |
| registry | required | Registry host, e.g. ghcr.io. |

| Switch | Value | Description |
| --- | --- | --- |
| -u, --user | USER | Registry username. |
| -p, --password | TOKEN | Password or token. |

```console
vmlab template login ghcr.io -u myuser -p <token>
```

### vmlab template push

Push a stored template to a registry as an OCI artifact (chunked, multi-arch capable).

| Argument | Required | Description |
| --- | --- | --- |
| <arch>/<name>\[@<ver>\] | required | Local store ref. |
| registry/repo:tag | required | Remote registry ref. |

```console
vmlab template push x86_64/linux-modern@1.0 ghcr.io/owner/linux-modern:1.0
```

### vmlab template pull

Pull a template from a registry into the store. --arch is required for multi-arch indexes.

| Argument | Required | Description |
| --- | --- | --- |
| registry/repo:tag | required | Remote registry ref. |

| Switch | Value | Description |
| --- | --- | --- |
| --arch | ARCH | Required when the remote is a multi-arch index. |

```console
vmlab template pull ghcr.io/owner/linux-modern:1.0 --arch x86_64
```

## vmlab daemon

Manage the supervisor daemon. It is auto-started by any other verb, so this is rarely needed.

### vmlab daemon start

Start the supervisor (auto-started by any other verb anyway).

```console
vmlab daemon start
```

### vmlab daemon stop

Stop the supervisor and all lab daemons.

```console
vmlab daemon stop
```

### vmlab daemon status

Show supervisor version and running labs (name/state/pid/root).

```console
vmlab daemon status
```

[← Back to SKILL.md](../SKILL.md)
