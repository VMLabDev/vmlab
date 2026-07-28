# lab {} block

_wcl block_

Declares a lab: a set of VMs plus the virtual networks connecting them, in vmlab.wcl.

A **lab** is declared with a `lab {}` block in `vmlab.wcl`, located by walking up
from the current directory (like git). Lab-local working data — linked clones,
snapshots, built media, screenshots — lives in `.vmlab/` beside it; gitignore that
directory. Every WCL file in a lab starts with `import <vmlab.wcl>`.


## Minimal lab

```wcl
import <vmlab.wcl>

lab "demo" {
  gui = true                  // lab-wide default: open a VNC viewer on each guest
                              // when `vmlab up` runs interactively (VM `gui`
                              // overrides; skipped when there is no display)
  vm "box" {
    template = "x86_64/linux-modern"
    memory   = 2GiB
    nic { nat = true }
  }
}
```

VMs always run headless; `gui` only decides whether the CLI opens a viewer for
you. Closing the viewer merely disconnects — the VM keeps running, and you can
reattach any time with `vmlab console`.


## Examples

### A small multi-segment lab

Two segments (one with the DC as DNS, one zero-config), three VMs with static and dynamic leases, a scoped provision and crash handlers.

```wcl
import <vmlab.wcl>

lab "ad-lab" {
  segment "corp" {
    subnet = "10.50.0.0/24"
    dns { server = "10.50.0.10" }                 // AD owns DNS
    route { dest = "10.60.0.0/24" via = "10.50.0.254" }
  }
  segment "dmz" { }                               // zero-config: auto subnet, daemon DHCP/DNS

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    cpus = 4  memory = 8GiB
    nic { segment = "corp" ip = "10.50.0.10" }    // static → DHCP reservation

    provision "scripts/setup.ws" { }              // runs once dc01 is ready
  }
  vm "client01" {
    template   = "x86_64/windows-11@26100.1"      // version pin
    depends_on = ["dc01"]                         // boot ordering
    nic { segment = "corp" }                      // dynamic lease
  }
  vm "buildbox" {
    template = "x86_64/linux-modern"
    nic { nat = true }
  }

  on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
  on "host.disk_low" { run = "scripts/alert.ws" }
}
```

## Related

- [vm {} block](../references/entity_vms.md)

- [segment {} block](../references/entity_segment_block.md)

- [Networking model](../references/concept_networking.md)

- [Provisions & event handlers](../references/concept_provisions.md)

- [Daemon model](../references/concept_daemon_model.md)

- [vmlab.wcl](../references/entity_vmlab_wcl.md)

- [.vmlab/](../references/entity_dot_vmlab.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
