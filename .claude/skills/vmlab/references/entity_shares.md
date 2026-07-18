# share {} block

_wcl block_

Mounts a host folder into a guest — over virtiofs when host and guest support it, falling back to SMB served by the lab daemon at the segment gateway.

A `share {}` exposes a host folder to a guest. The default `transport = "auto"`
uses **virtiofs** (a vhost-user-fs device — no guest networking involved,
snapshot-safe) when both the host and the guest profile support it, and falls
back to **SMB** served by the lab daemon at the segment gateway
(`\\<gateway>\<share>`) otherwise; pin `transport = "virtiofs"` or `"smb"` to
force one. SMB credentials are auto-generated per lab and persisted in
`.vmlab/smb/creds` (rotated only by `destroy`). The guest agent mounts the
share once the VM is ready — Linux mounts the virtiofs tag directly, Windows
mounts it via WinFsp.


```wcl
share { host = "./src"  guest = "/mnt/src" }                  // auto-mounted when ready
share { host = "~/data" guest = "D:\\data" readonly = true }  // drive letter on Windows
share { host = "./old"  guest = "X:" smb1 = true }            // legacy SMB1 for XP/2003
share { host = "./out"  guest = "/mnt/out" transport = "smb" } // force a transport
// `name = "..."` is optional (derived from the guest path if omitted)
```

Share contents live on the host and are **outside snapshot scope**. The SMB
path requires a NIC on a segment (validation error otherwise); virtiofs works
without guest networking. On Windows the agent mounts as SYSTEM (visible to
provisions and `vmlab exec`); with SMB, interactive users double-click the
auto-dropped `vmlab-shares` desktop script once to authenticate their own
session. Container `volume {}` blocks ride the same virtiofs transport (CIFS
fallback) — see the [container block](../references/entity_container_block.md).


## Related

- [vm {} block](../references/entity_vms.md)

- [nic {} block](../references/entity_nic_block.md)

- [Daemon model](../references/concept_daemon_model.md)

- [container {} block](../references/entity_container_block.md)

[← Back to SKILL.md](../SKILL.md)
