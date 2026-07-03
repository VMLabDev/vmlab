# share {} block

_wcl block_

Mounts a host folder into a guest over SMB, served by the lab daemon at the segment gateway.

A `share {}` exposes a host folder to a guest over SMB, served by the lab daemon at
the segment gateway (`\\<gateway>\<share>`). Credentials are auto-generated per lab
and persisted in `.vmlab/smb/creds` (rotated only by `destroy`). The guest agent
mounts the share once the VM is ready.


```wcl
share { host = "./src"  guest = "/mnt/src" }                  // auto-mounted when ready
share { host = "~/data" guest = "D:\\data" readonly = true }  // drive letter on Windows
share { host = "./old"  guest = "X:" smb1 = true }            // legacy dialect for XP/2003
// `name = "..."` is optional (derived from the guest path if omitted)
```

Share contents are **outside snapshot scope**. The VM must have a NIC on a segment
(validation error otherwise). On Windows the agent mounts as SYSTEM (visible to
provisions and `vmlab exec`); interactive users double-click the auto-dropped
`vmlab-shares` desktop script once to authenticate their own session.


## Related

- [vm {} block](../references/entity_vms.md)

- [nic {} block](../references/entity_nic_block.md)

- [Daemon model](../references/concept_daemon_model.md)

[← Back to SKILL.md](../SKILL.md)
