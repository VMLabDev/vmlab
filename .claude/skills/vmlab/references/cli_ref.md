# vmlab — CLI reference

The vmlab command-line interface: lab lifecycle, per-VM and per-container control, snapshots, guest execution and scripting, playbooks, console and logs, template builds and OCI distribution, the network fast path, and the daemons.

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

## vmlab pull

Download missing registry templates/images for the lab's machines without starting anything.

| Argument | Required | Description |
| --- | --- | --- |
| machine… | optional | Optional machines to pull for; omit for all. |

```console
vmlab pull
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

Per-VM power control and interaction: power, screenshot, keys/mouse, OCR and image matching.

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

### vmlab vm destroy

Destroy one VM: stop it and delete its clone (config retained; a later `up` rebuilds it).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab vm destroy dc01
```

### vmlab vm screenshot

Capture a running VM's screen to a PNG file.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| path | required | Output PNG path. |

```console
vmlab vm screenshot dc01 screen.png
```

### vmlab vm sendkeys

Send a key chord (see the key-chord reference).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| chord | required | Chord, e.g. `ctrl-alt-delete`. |

```console
vmlab vm sendkeys dc01 ctrl-alt-delete
```

### vmlab vm mouse-move

Move the mouse pointer to absolute screen coordinates.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| x | required | X coordinate. |
| y | required | Y coordinate. |

```console
vmlab vm mouse-move dc01 640 400
```

### vmlab vm click

Click a mouse button, optionally first moving to x,y (omit to click at the current position).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| x | optional | Move here first (optional). |
| y | optional | Move here first (optional). |

| Switch | Value | Description |
| --- | --- | --- |
| --button | left\|right\|middle | Button to click (default left). |

```console
vmlab vm click dc01 640 400
vmlab vm click dc01 --button right
```

### vmlab vm drag

Press, drag from x1,y1 to x2,y2, and release the left button.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| x1 | required | Start X. |
| y1 | required | Start Y. |
| x2 | required | End X. |
| y2 | required | End Y. |

```console
vmlab vm drag dc01 100 100 400 300
```

### vmlab vm ocr

OCR the VM's screen (optionally a region) and print the recognised text.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

| Switch | Value | Description |
| --- | --- | --- |
| --region | X Y W H | Restrict to a region (four values). |

```console
vmlab vm ocr dc01
vmlab vm ocr dc01 --region 0 0 800 100
```

### vmlab vm find-image

Search the screen for a template image; prints match coordinates.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| image | required | Template image path (PNG/PPM). |

| Switch | Value | Description |
| --- | --- | --- |
| --threshold | 0.0–1.0 | Match threshold (default 0.9). |
| --region | X Y W H | Restrict the search to a region (four values). |

```console
vmlab vm find-image dc01 ok-button.png
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

### vmlab container shell

Attach an interactive shell inside the container's PID namespace (the workload is PID 1). Over the vmlab-agent virtio-serial channel — no container network needed. Ctrl-\] detaches.

| Argument | Required | Description |
| --- | --- | --- |
| container | required | Container name. |

```console
vmlab container shell web
```

## vmlab lab

Manage running labs host-wide, by name (not the cwd's lab).

### vmlab lab list

List every tracked lab: name, state, and directory.

| Switch | Value | Description |
| --- | --- | --- |
| --json | — | Emit a JSON array instead of a table. |

```console
vmlab lab list
```

### vmlab lab info

Detailed status (VMs and segments) of a running lab.

| Argument | Required | Description |
| --- | --- | --- |
| lab | required | Lab name. |

```console
vmlab lab info ad-demo
```

### vmlab lab stop

Gracefully stop a running lab; clones retained.

| Argument | Required | Description |
| --- | --- | --- |
| lab | required | Lab name. |

| Switch | Value | Description |
| --- | --- | --- |
| --force | — | Hard kill instead of the graceful ladder. |

```console
vmlab lab stop ad-demo
```

### vmlab lab destroy

Stop a lab and DELETE its clones and local state. Destructive.

| Argument | Required | Description |
| --- | --- | --- |
| lab | required | Lab name. |

```console
vmlab lab destroy ad-demo
```

## vmlab snapshot

Online (running: disk+RAM+device state) or offline (powered off: disk only) snapshots, per current power state. Restoring an online snapshot resumes running. Containers snapshot identically to VMs.

### vmlab snapshot create

Create a snapshot. Omitting --vm snapshots every VM and container in the lab (best-effort, not coordinated).

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

## vmlab playbook

Run config-weave playbooks (declared with `playbook {}` blocks) against lab machines. Exit codes mirror config-weave: 0 ok, 1 step error, 2 validation, 3 reboot still required after bounded retries.

### vmlab playbook list

List the lab's playbook declarations and any in-flight runs.

```console
vmlab playbook list
```

### vmlab playbook check

Report drift without changing the guest (re-pushes the playbook first).

| Argument | Required | Description |
| --- | --- | --- |
| machine | required | Machine (\[lab/\]name — VM or container). |

| Switch | Value | Description |
| --- | --- | --- |
| --playbook | PATH | Playbook folder, when several target this machine. |
| --play | NAME | Play name, when several target this machine. |

```console
vmlab playbook check dc01
```

### vmlab playbook apply

Push the playbook and converge the guest (auto-reboots when a step demands it).

| Argument | Required | Description |
| --- | --- | --- |
| machine | required | Machine (\[lab/\]name — VM or container). |

| Switch | Value | Description |
| --- | --- | --- |
| --playbook | PATH | Playbook folder, when several target this machine. |
| --play | NAME | Play name, when several target this machine. |

```console
vmlab playbook apply dc01
```

## vmlab exec

Run a command in a guest and print its stdout/stderr over the vmlab-agent channel.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| cmd… | required | Command and arguments, after `--`. |

```console
vmlab exec dc01 -- ipconfig /all
```

## vmlab shell

Attach an interactive shell inside a VM: root bash on Linux, SYSTEM PowerShell (ConPTY) on Windows. Rides the vmlab-agent virtio-serial channel, so it works with no guest network. Each attach is a fresh, independent session; Ctrl-\] detaches. Needs a template built with the agent (`agent_version` in its meta).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab shell dc01
```

## vmlab cp

Copy a file or directory between host and guest — either side may be `<vm>:<path>`. Parent directories are created. Agent transport: raw digest-verified bytes.

| Argument | Required | Description |
| --- | --- | --- |
| src | required | Host path, or <vm>:<path> to pull from the guest. |
| dest | required | <vm>:<path> to push, or a host path when pulling. |

```console
vmlab cp payload.zip dc01:C:/temp/payload.zip
vmlab cp dc01:C:/Windows/debug/netsetup.log ./netsetup.log
```

## vmlab tail

Follow a file inside a guest (`tail -F` semantics over the agent channel — survives rotation; no network, no shell required).

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |
| path | required | Guest file path. |

```console
vmlab tail web /var/log/nginx/access.log
```

## vmlab eventlog

Follow the Windows event log of a guest over the agent channel.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name (Windows guest). |

| Switch | Value | Description |
| --- | --- | --- |
| --filter | XPATH | XPath filter (default: everything on the System channel). |

```console
vmlab eventlog dc01
vmlab eventlog dc01 --filter "*[System[(EventID=4624)]]"
```

## vmlab osinfo

Print guest OS information (guest-get-osinfo) as JSON.

| Argument | Required | Description |
| --- | --- | --- |
| vm | required | VM name. |

```console
vmlab osinfo dc01
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
| --version | VER | Pin an explicit version instead of auto-incrementing (single target only). |

```console
vmlab template build
vmlab template build -f templates.wcl linux-modern
```

### vmlab template list

List templates in the store.

| Switch | Value | Description |
| --- | --- | --- |
| --json | — | Emit the full metadata array (ref, sizes in bytes, RFC 3339 created). |
| --remote | — | Also check each template's registry: adds a REMOTE column (yes/no/local). Needs network access. |

```console
vmlab template list --json
vmlab template list --remote
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

### vmlab template clean

Prune superseded builds, keeping the latest per template. Dry-run unless --yes; builds still backing a clone are skipped unless --force.

| Argument | Required | Description |
| --- | --- | --- |
| filter | optional | Limit to a family: `<arch>/<name>`, `<arch>/`, or `<name>`. Default: every template. |

| Switch | Value | Description |
| --- | --- | --- |
| --keep | N | Most-recent builds to keep per template (default 1). |
| -y, --yes | — | Actually delete; without it, only prints what would be removed. |
| --force | — | Also remove builds that still back existing clones. |

```console
vmlab template clean            # dry run
vmlab template clean x86_64/linux-modern --yes
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

### vmlab template search

Search the configured OCI registries. Uses VM registries by default; --kind container searches image registries.

| Argument | Required | Description |
| --- | --- | --- |
| query | optional | Case-insensitive repository-name filter. |

| Switch | Value | Description |
| --- | --- | --- |
| --registry | NAMESPACE | Search only this namespace instead of shared registry settings. |
| --arch | ARCH | Only return artifacts supporting this architecture. |
| --kind | vm\|container | Artifact kind to search (default vm). |
| --json | — | Emit a JSON array instead of a table. |

```console
vmlab template search alpine --arch x86_64
vmlab template search nginx --kind container
```

### vmlab template registry

Manage OCI namespace settings shared by the CLI and web console.

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

Push a stored template to a registry as an OCI artifact (chunked, multi-arch capable). Moves the `latest` tag (or `latest-prerelease` with --prerelease) and links the package to a source repo.

| Argument | Required | Description |
| --- | --- | --- |
| <arch>/<name>\[@<ver>\] | required | Local store ref. |
| registry/repo:tag | optional | Remote registry ref; defaults to the template's own `registry` field. |

| Switch | Value | Description |
| --- | --- | --- |
| --source | URL | Source repository URL to link the package to (default: the cwd's git origin when it resolves to a web URL). |
| --prerelease | — | Publish as a pre-release: move `latest-prerelease` instead of `latest`. |

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
| --overwrite | — | Overwrite an existing version in the store. |

```console
vmlab template pull ghcr.io/owner/linux-modern:1.0 --arch x86_64
```

## vmlab fastpath

Show which network fast-path tier is active (userspace / afxdp / sockmap) — and why the others are not.

```console
vmlab fastpath
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
