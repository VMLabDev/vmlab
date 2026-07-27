# Provisions & event handlers

_provision {} scripts run on `vmlab up` (failure is fatal); on "event" {} handlers react to lifecycle events (failure is logged)._

Provision scripts are declared inside the `vm {}`/`container {}` they configure and
run on `vmlab up` in declaration order; event handlers are lab-level and react to
lifecycle events. Both are wscript files — see [the provision block](../references/entity_provision_block.md)
and [the event handler block](../references/entity_on_handler.md) for the declarations.


**Provision failures fail `vmlab up`**; **handler failures are logged, never fatal.**
A machine's steps gate `depends_on` on it: dependents wait for its provisions and
playbooks to finish. Inside the script, `lab.this_vm()` is the owning VM and
`lab.vm("name")` reaches any other machine, so a script that orchestrates several
guests is declared on whichever one should gate it. See
[the event list](../references/fact_events.md) for every event name.


## Examples

### A crash event handler

fn handle(event, lab): screenshot the crashed VM. Failures here are logged, never fatal.

```rust
use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("crash handler fired for " + event.vm + " (" + event.name + ")")
    let Ok(vm) = lab.vm(event.vm) else { return }
    match vm.screenshot("") {
        Ok(path) => lab.log("saved crash screenshot: " + path),
        Err(e)   => lab.log("could not screenshot: " + e),
    }
}
```

## Related

- [Automating labs](../references/concept_automation_overview.md)

- [lab {} block](../references/entity_labs.md)

- [provision {} block](../references/entity_provision_block.md)

- [on "event" {} handler](../references/entity_on_handler.md)

- [wscript: overview](../references/concept_wscript_overview.md)

- [wscript: pattern matching & errors](../references/concept_wscript_matching.md)

- [Lifecycle events](../references/fact_events.md)

[← Back to SKILL.md](../SKILL.md)
