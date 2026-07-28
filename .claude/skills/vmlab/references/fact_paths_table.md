# Filesystem layout

All XDG paths respect their environment variables.

| Path | Contents |
| --- | --- |
| `<labdir>/vmlab.wcl` | Lab definition — found by walking up from cwd |
| `<labdir>/.vmlab/` | Lab-local working data: linked clones, snapshots, built media, screenshots (gitignore it) |
| `~/.local/share/vmlab/templates/<arch>/<name>/<version>/` | Template store (`disk.qcow2` + `template.wcl`) |
| `~/.cache/vmlab/artefacts/` | Downloaded ISO/qcow2 + built-media cache (content-addressed) |
| `~/.local/state/vmlab/` | Daemon state + logs (`vmlabd.log`, per-lab/per-VM JSON-line logs). `events.jsonl` and `lab.log` roll over at 16 MiB, keeping one `.1` generation |
| `~/.config/vmlab/config.wcl` | Host config (optional) |
| `~/.config/vmlab/profiles/*.wcl` | User profile overrides/additions |
| `$XDG_RUNTIME_DIR/vmlab/vmlabd.sock` | Supervisor socket (fallback `/tmp/vmlab-<uid>/`) |
| `$XDG_RUNTIME_DIR/vmlab/labs/<lab>/control.sock` | Lab daemon socket (+ per-VM QMP/agent/NIC/VNC sockets beside it) |

Control sockets are owner-only: their parent directory is created `0700` in one
step (no world-traversable window on a fresh `/tmp/vmlab-<uid>`) and each socket
is `0600`. If that directory already exists and belongs to another uid, vmlab
refuses to put sockets there rather than trusting it.


## Related

- [Host config](../references/concept_host_config.md)

- [Template store](../references/entity_template_store.md)

- [.vmlab/](../references/entity_dot_vmlab.md)

[← Back to SKILL.md](../SKILL.md)
