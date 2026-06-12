# Host config, profiles, paths & daemons

## Host config (`~/.config/vmlab/config.wcl`)

Optional — absence means all defaults. All XDG paths respect their env vars.

```wcl
host {
  subnet_pool      = "10.213.0.0/16"   // segment auto-allocation pool (default shown)
  dns_suffix       = "vmlab.internal"  // suffix for auto-registered VM names (default)
  dns_upstream     = "1.1.1.1"         // upstream resolver ip[:port]; default: host resolver
  disk_low_percent = 10                // host.disk_low watchdog threshold (default 10)
  psk              = "secret"          // pre-shared key for cross-host segment peering (§9.2)
  viewer           = "vncviewer {}"    // console viewer command; {} = target
  oci_chunk_size   = "512M"            // OCI push layer chunk size (default 512M)
}
```

## Filesystem layout

| Path | Contents |
|---|---|
| `<labdir>/vmlab.wcl` | Lab definition — found by walking up from cwd |
| `<labdir>/.vmlab/` | Lab-local working data: linked clones, snapshots, built media, screenshots (gitignore it) |
| `~/.local/share/vmlab/templates/<arch>/<name>/<version>/` | Template store (`disk.qcow2` + `template.wcl`) |
| `~/.cache/vmlab/artefacts/` | Downloaded ISO/qcow2 + built-media cache (content-addressed) |
| `~/.local/state/vmlab/` | Daemon state + logs (`vmlabd.log`, per-lab/per-VM JSON-line logs) |
| `~/.config/vmlab/config.wcl` | Host config (optional) |
| `~/.config/vmlab/profiles/*.wcl` | User profile overrides/additions |
| `$XDG_RUNTIME_DIR/vmlab/vmlabd.sock` | Supervisor socket (fallback `/tmp/vmlab-<uid>/` without XDG_RUNTIME_DIR) |
| `$XDG_RUNTIME_DIR/vmlab/labs/<lab>/control.sock` | Lab daemon socket (+ per-VM QMP/agent/NIC/VNC sockets beside it) |

## Daemon model

- **Supervisor (`vmlabd`)** — one per user, **auto-started by the CLI** on
  first use. Owns the lab registry, global segments, cross-host peering,
  serialised template-store writes, host watchdogs, aggregated events.
- **Lab daemon** — one per running lab, spawned by the supervisor on
  `vmlab up`, reaped on `down`/`destroy`. Owns QEMU processes, QMP/agent
  channels, the userspace network fabric, snapshots, the wisp runtime.
- If a lab daemon dies, the supervisor emits `lab.daemon_crashed` and marks
  the lab failed — **no auto-restart** (restart policy belongs to `on`
  handlers). Manual control: `vmlab daemon start|stop|status`.

## Shipped guest OS profiles (PRD §5.3)

Hardware defaults; precedence is **VM block > template metadata > profile >
defaults**. Override or extend by dropping `*.wcl` into
`~/.config/vmlab/profiles/`.

| Profile | Machine | Firmware | Secure boot | TPM | Disk bus | NIC | Display | CPUs/Mem |
|---|---|---|---|---|---|---|---|---|
| `windows-11` | q35 | ovmf | yes | yes | virtio | virtio-net-pci | qxl | 4 / 8G |
| `windows-server` | q35 | ovmf | no | yes | virtio | virtio-net-pci | qxl | 4 / 8G |
| `windows-legacy` | pc | seabios | no | no | ide | e1000 | std | 2 / 2G |
| `linux-modern` | q35 | ovmf | no | no | virtio | virtio-net-pci | virtio-gpu | 2 / 4G |
| `linux-generic` | q35 | seabios | no | no | virtio | virtio-net-pci | std | 2 / 2G |
| `custom` | — nothing assumed; supply everything via VM/template attributes + `qemu_args` | | | | | | | |

Source of truth: PRD §3–4, §5.3; `src/config/host_schema.wcl`,
`src/profiles/shipped.wcl`, `src/paths.rs`.
