# Event

_data type_

The payload passed to an event handler's fn handle(event, lab): name, vm, data (JSON as text).

The argument to `fn handle(event: Event, lab: Lab)` in an [event handler](../references/entity_on_handler.md).

```rust
struct Event { name: string, vm: string, data: string }   // data = JSON payload as text
```

See [the lifecycle events](../references/fact_events.md) for the `name` values and what each carries.

## Related

- [on "event" {} handler](../references/entity_on_handler.md)

- [Lifecycle events](../references/fact_events.md)

- [Vm](../references/entity_vm_api.md)

[← Back to SKILL.md](../SKILL.md)
