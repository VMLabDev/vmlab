# How vmlab fits together

_The big picture: front ends, the two-tier daemon, QEMU processes, the userspace network fabric, templates and clones, and the three guest channels._

Every vmlab feature hangs off one spine: front ends talk a JSON-lines protocol
to a small daemon tree, which owns the QEMU processes and everything around
them.


![diagram](../_wdoc/concept_architecture-diagram-1.svg)

Both front ends speak the same unix-socket protocol to the supervisor; each running lab gets its own daemon, and each daemon owns its lab's QEMU processes.

**Front ends.** The `vmlab` CLI and the [vmlab-web](../references/entity_vmlab_web.md) server
are peers: both speak the same protocol, so anything the CLI does the web
console can do. Daemons auto-start on first use — there is no setup step.

**The daemon tier.** The supervisor `vmlabd` tracks the host-wide lab
registry and global concerns; one **lab daemon** per running lab owns that
lab's QEMU processes, its network fabric, snapshots, events and shared
folders. A lab daemon crashing affects only its lab — see
[the daemon model](../references/concept_daemon_model.md).

**Machines.** Each VM is a QEMU process booted from a
[linked clone](../references/concept_linked_clones.md) — a copy-on-write qcow2 over a sealed
[template](../references/concept_templates.md) in the store. Lab containers are the same
picture in miniature: the OCI image becomes the root filesystem of a tiny
micro-VM, so "everything is a machine" holds for networking, snapshots and
automation alike.

**Networking.** The fabric is userspace by default — the lab daemon \*is\* the
switch, DHCP server, DNS server, router and NAT for its segments — which is
why vmlab runs unprivileged ([networking model](../references/concept_networking.md)); eBPF
tiers accelerate it where the host allows.

**Guest channels.** Two doors into a running guest, both daemon-owned: QMP
(power, device control) and [vmlab-agent](../references/entity_vmlab_agent.md) — the
first-party agent carrying readiness, shells, exec, file transfer, metrics,
IP/OS reporting, shutdown and playbooks over virtio-serial with no guest
network.

**Where things live.** Machine-wide state (the template store, daemon
sockets, host config) sits under the user's home directories
([paths](../references/fact_paths_table.md), [template store](../references/entity_template_store.md)); each
lab keeps its disposable working data in its own
[`.vmlab/`](../references/entity_dot_vmlab.md) beside `vmlab.wcl`.


## Related

- [Daemon model](../references/concept_daemon_model.md)

- [Networking model](../references/concept_networking.md)

- [Templates](../references/concept_templates.md)

- [Linked clones](../references/concept_linked_clones.md)

- [vmlab-agent](../references/entity_vmlab_agent.md)

- [vmlab-web](../references/entity_vmlab_web.md)

- [Filesystem layout](../references/fact_paths_table.md)

- [.vmlab/](../references/entity_dot_vmlab.md)

- [Template store](../references/entity_template_store.md)

[← Back to SKILL.md](../SKILL.md)
