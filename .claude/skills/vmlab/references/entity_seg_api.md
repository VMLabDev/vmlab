# Segment

_api object_

Runtime network-rule mutation: DNS, blocking, redirects and forwards on a live segment.

| Method | Returns | Notes |
| --- | --- | --- |
| `seg.name()` | `string` |  |
| `seg.dns_set(name: string, ip: string)` | `Result[int, string]` | Static DNS entry → rule id |
| `seg.dns_sinkhole(pattern: string)` | `Result[int, string]` | Wildcards OK; always NXDOMAIN |
| `seg.dns_clear(rule_id: int)` | `Result[bool, string]` |  |
| `seg.block(cidr: string)` | `Result[int, string]` | CIDR or bare IP |
| `seg.block_port(cidr: string, proto: string, port: int)` | `Result[int, string]` | proto: `"tcp"` / `"udp"` / `"icmp"` |
| `seg.unblock(rule_id: int)` | `Result[bool, string]` |  |
| `seg.redirect(from: string, to: string)` | `Result[int, string]` | DNAT `"ip[:port]"` → `"ip[:port]"` |
| `seg.forward(host_port: int, vm: string, guest_port: int)` | `Result[int, string]` | TCP only; VM needs a lease already |
| `seg.rules()` | `Result[string, string]` | JSON list of rules |
| `seg.route_to(other)` / `seg.unroute_to(other)` | `Result[unit, string]` | **Always Err — not yet available from scripts** |

## Related

- [Networking model](../references/concept_networking.md)

- [segment {} block](../references/entity_segment_block.md)

- [Lab](../references/entity_lab_api.md)

[← Back to SKILL.md](../SKILL.md)
