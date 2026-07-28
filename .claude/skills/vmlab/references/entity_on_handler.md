# on "event" {} handler

_wcl block_

Binds a wscript file to a lifecycle event; handler failures are logged, never fatal.

An `on "<event>" {}` block reacts to a lifecycle event by running a wscript file (`fn handle(event, lab)`).

```wcl
on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
on "vm.crashed"    { run = "scripts/page-oncall.ws"  targets = ["dc01", "srv01"] }
on "host.disk_low" { run = "scripts/alert.ws" }
```

`targets` narrows a handler to named machines; leave it out and the handler runs for **every** occurrence of the event. It is only legal on machine-scoped events — `vm.*`, `container.*` and `snapshot.*`. Declaring it on a lab-wide event (`lab.up`, `host.disk_low`, `template.built`, …) is a validation error: \*event "x" is lab-wide and cannot declare targets\*.

**Handler failures are logged, never fatal.** See [the event list](../references/fact_events.md) for every event name and [the Event type](../references/entity_event_type.md) for the handler payload.

## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [provision {} block](../references/entity_provision_block.md)

- [Lifecycle events](../references/fact_events.md)

- [Event](../references/entity_event_type.md)

[← Back to SKILL.md](../SKILL.md)
