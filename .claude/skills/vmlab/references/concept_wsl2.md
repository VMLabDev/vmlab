# WSL2

_WSL2 is a first-class host — no tap/bridge/macvlan and no CAP_NET_ADMIN; reach guests via port forwards._

WSL2 is a first-class host — no tap/bridge/macvlan and no `CAP_NET_ADMIN`. Enable
nested virtualisation in `.wslconfig` (KVM needs it); reach guests from Windows via
port forwards (WSL's localhost forwarding bridges them); use
`vmlab console <vm> --tcp` for a Windows-side VNC viewer. `$XDG_RUNTIME_DIR` is
created at daemon start if absent (falls back to `/tmp/vmlab-<uid>`). The ext4 VHDX
grows as `.vmlab/` clones grow, so the `host.disk_low` watchdog matters more here.


## Related

- [Running vmlab in a container](../references/concept_containers.md)

- [Networking model](../references/concept_networking.md)

[← Back to SKILL.md](../SKILL.md)
