# nic {} block

_wcl block_

Attaches a VM to a segment, with optional static IP/MAC, NAT shorthand or port isolation.

A `nic {}` block inside a `vm {}` attaches that guest to one segment.

```wcl
nic { segment = "corp" ip = "10.50.0.10" mac = "52:54:00:aa:bb:cc" }  // static lease + fixed MAC
nic { nat = true }                       // shorthand: per-lab built-in NAT segment (no segment {} needed)
nic { segment = "dmz" isolated = true }  // port isolation: can't reach segment neighbours
```

A static `ip` must fall inside the segment's subnet, and static IPs/MACs must be unique across the lab (validation errors otherwise). A VM declaring any `share {}` must have a NIC on a segment.

## Related

- [vm {} block](../references/entity_vms.md)

- [segment {} block](../references/entity_segment_block.md)

- [Networking model](../references/concept_networking.md)

[← Back to SKILL.md](../SKILL.md)
