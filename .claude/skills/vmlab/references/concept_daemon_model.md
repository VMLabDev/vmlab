# Daemon model

_A two-tier daemon: the supervisor vmlabd (one per user) plus one lab daemon per running lab, auto-started by the CLI._

vmlab runs a **two-tier daemon**, auto-started by the CLI — there is no setup step.


| Tier | Scope | Owns |
| --- | --- | --- |
| Supervisor (`vmlabd`) | One per user, auto-started on first CLI use | Lab registry, global segments, cross-host peering, serialised template-store writes, host watchdogs, aggregated events |
| Lab daemon | One per running lab; spawned on `up`, reaped on `down`/`destroy` | QEMU processes, QMP/agent channels, the userspace network fabric, snapshots, the wscript runtime |

If a lab daemon dies, the supervisor emits `lab.daemon_crashed` and marks the lab
failed — **there is no auto-restart** (restart policy belongs to `on` handlers).
Manual control: `vmlab daemon start | stop | status`.


## Related

- [How vmlab fits together](../references/concept_architecture.md)

- [lab {} block](../references/entity_labs.md)

- [Host config](../references/concept_host_config.md)

- [Networking model](../references/concept_networking.md)

[← Back to SKILL.md](../SKILL.md)
