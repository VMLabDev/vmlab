# Lifecycle events

Events fire `on "<event>" {}` handlers and arrive in `fn handle(event: Event, lab: Lab)`. Handler failures are logged, never fatal.

| Event | Meaning |
| --- | --- |
| `vm.starting` | A VM has begun booting |
| `vm.ready` | The guest agent is responding |
| `vm.stopped` | A VM powered off cleanly |
| `vm.crashed` | A VM died unexpectedly (includes closing a `gui` window) |
| `container.starting` | A container's micro-VM has begun booting |
| `container.ready` | The container process started (and its healthcheck passed, when declared) |
| `container.stopped` | A container stopped for good (carries `exit_code`) |
| `container.crashed` | A container exited abnormally (`restarting: true` when the restart policy respawns it) |
| `container.unhealthy` | A container's healthcheck failed `retries` times in a row |
| `lab.up` | The lab finished coming up |
| `lab.down` | The lab stopped |
| `snapshot.created` | A snapshot was taken |
| `snapshot.restored` | A snapshot was restored |
| `template.built` | A template build sealed into the store |
| `lab.daemon_crashed` | A lab daemon died (no auto-restart) |
| `host.disk_low` | Free disk fell below `disk_low_percent` |

## Related

- [Provisions & event handlers](../references/concept_provisions.md)

- [on "event" {} handler](../references/entity_on_handler.md)

- [Event](../references/entity_event_type.md)

[← Back to SKILL.md](../SKILL.md)
