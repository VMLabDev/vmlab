# segment {} block

_wcl block_

Declares a virtual L2 switch and its subnet, DHCP, NAT, routing and peering options.

A `segment {}` block declares one virtual L2 switch. The lab daemon supplies its
DHCP, DNS, NAT, routing and L3 filtering (see [the networking model](../references/concept_networking.md)).


```wcl
segment "name" {
  subnet    = "10.0.0.0/24"      // CIDR; auto-allocated from host pool if omitted
  global    = true               // owned by the supervisor, shared across labs
  dhcp      = true               // default true
  nat       = true               // internet egress for this segment (default false)
  routes_to = ["other_segment"]  // daemon inter-segment routing opt-in
}
```

Inside a segment go the DNS, routing, forwarding and filtering sub-blocks — see [the segment sub-blocks](../references/fact_segment_subblocks.md). `record` and `sinkhole` may also appear at lab level (lab-wide).

## Related

- [Networking model](../references/concept_networking.md)

- [segment {} sub-blocks](../references/fact_segment_subblocks.md)

- [nic {} block](../references/entity_nic_block.md)

- [Segment](../references/entity_seg_api.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
