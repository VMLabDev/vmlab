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
vmlab start <vm>
vmlab stop <vm> [--force]
vmlab restart <vm>
```

## Snapshots

Online (VM running: disk+RAM+device state) or offline (powered off: disk
only) per current power state. Restoring an online snapshot resumes running.

```sh
vmlab snapshot <name> [--vm <vm>]    # omit --vm = every VM in the lab (best-effort, not coordinated)
vmlab restore <name> [--vm <vm>]
vmlab snapshots <vm>                 # list (name, taken_at, power_state)
vmlab snapshot-delete <vm> <name>
```

## Guest execution & scripting

```sh
vmlab exec <vm> -- <cmd> [args...]   # run via guest agent, prints stdout/stderr
vmlab run <script.wisp>              # ad-hoc wisp script against the running lab (fn main(lab: Lab))
vmlab wispi [out]                    # write wisp interface file for LSP (default vmlab.wispi)
```

## Console & logs

```sh
vmlab console <vm>          # launch VNC viewer (host config `viewer` command)
vmlab console <vm> --tcp    # forward VNC over localhost TCP instead (WSL2 / remote viewers)
vmlab logs [lab/][vm]       # JSON-line logs; lab events or one VM's QEMU/serial
vmlab logs -f               # follow
vmlab logs -n 50            # lines of history (default 100)
```

## Runtime network rules (lab must be running)

```sh
vmlab net rules                                      # list L3 rules across segments
vmlab net block <segment> <cidr>                     # drop traffic to CIDR/IP
vmlab net redirect <segment> <from> <to>             # DNAT ip[:port] -> ip[:port]
vmlab net forward <segment> <host_port> <vm> <guest_port>   # host → guest port forward (TCP)
```

## Templates (store + builds)

```sh
vmlab template build [-f <file>] [<name>]   # build template {} blocks (default ./vmlab.wcl; name = just one)
vmlab template list
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

## Media

```sh
vmlab media build iso <folder> <out.iso> [-l <label>]
vmlab media build floppy <folder> <out.img> [-l <label>]
```

## Daemon (normally automatic)

```sh
vmlab daemon start    # start supervisor (auto-started by any other verb anyway)
vmlab daemon stop     # stop supervisor and all lab daemons
vmlab daemon status   # supervisor version + running labs (name/state/pid/root)
```

Source of truth: PRD §12; `src/cli/mod.rs`, `src/cli/net.rs`,
`src/template/cli.rs`, `src/cli/media.rs`, `src/cli/daemon.rs`.
