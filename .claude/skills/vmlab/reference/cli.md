# vmlab CLI reference

Lab-scoped verbs locate the lab by walking up from cwd to the nearest
`vmlab.wcl` (like git). Daemons auto-start as needed — no setup step.

## Lab lifecycle

```sh
vmlab validate                 # WCL schema + semantic validation, no side effects
vmlab up [vm...]               # create linked clones, boot (subset optional), run provisions
vmlab down [vm...] [--force]   # graceful stop (agent → ACPI → kill); clones retained
vmlab destroy                  # stop + DELETE clones, lab-local state, dynamic net config
vmlab status                   # lab/VM/segment state, IPs, ready flags
```

## Per-VM power

```sh
vmlab vm start <vm>
vmlab vm stop <vm> [--force]
vmlab vm restart <vm>
```

## Snapshots

Online (VM running: disk+RAM+device state) or offline (powered off: disk
only) per current power state. Restoring an online snapshot resumes running.

```sh
vmlab snapshot create <name> [--vm <vm>]   # omit --vm = every VM in the lab (best-effort, not coordinated)
vmlab snapshot restore <name> [--vm <vm>]
vmlab snapshot list <vm>                    # (name, taken_at, power_state)
vmlab snapshot delete <vm> <name>
```

## Guest execution & scripting

```sh
vmlab exec <vm> -- <cmd> [args...]   # run via guest agent, prints stdout/stderr
vmlab script <script.wisp>           # ad-hoc wisp script against the running lab (fn main(lab: Lab))
```

## Console & logs

```sh
vmlab console <vm>          # launch VNC viewer (host config `viewer` command)
vmlab console <vm> --tcp    # forward VNC over localhost TCP instead (WSL2 / remote viewers)
vmlab logs [lab/][vm]       # JSON-line logs; lab events or one VM's QEMU/serial
vmlab logs -f               # follow
vmlab logs -n 50            # lines of history (default 100)
```

Networking (segments, forwards, routes, filtering/redirection) is
declarative in `vmlab.wcl`; there is no `vmlab net` CLI. Runtime rule
mutation is available from wisp scripts via the `Segment` API (see
wisp-api.md).

## Templates (store + builds)

```sh
vmlab template build [-f <file>] [<name>]   # build template {} blocks (default ./vmlab.wcl; name = just one)
vmlab template list [--json]                # --json: full metadata array (ref, sizes in bytes, RFC 3339 created)
vmlab template rm <arch>/<name>@<version> [--force]   # exact version required; --force if clones back it
vmlab template export <arch>/<name>[@<ver>] <out.tar.zst>
vmlab template import <archive.tar.zst> [--overwrite]
```

## OCI distribution

```sh
vmlab template login <registry> -u <user> -p <password>
vmlab template push <arch>/<name>[@<ver>] <registry>/<repo>:<tag>
vmlab template pull <registry>/<repo>:<tag> [--arch <arch>]   # --arch required for multi-arch indexes
```

Media (ISO/floppy from a folder) is declared inline with `media {}` blocks
in VM/template definitions — there is no `vmlab media` CLI.

## Daemon (normally automatic)

```sh
vmlab daemon start    # start supervisor (auto-started by any other verb anyway)
vmlab daemon stop     # stop supervisor and all lab daemons
vmlab daemon status   # supervisor version + running labs (name/state/pid/root)
```

Source of truth: PRD §12; `src/cli/mod.rs`, `src/template/cli.rs`,
`src/cli/daemon.rs`.
