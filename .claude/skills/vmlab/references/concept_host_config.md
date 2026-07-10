# Host config

_Optional ~/.config/vmlab/config.wcl tunes the subnet pool, DNS, disk watchdog, viewer command and OCI chunk size._

The host config at `~/.config/vmlab/config.wcl` is optional — absence means all defaults. All XDG paths respect their environment variables.

```wcl
host {
  subnet_pool      = "10.213.0.0/16"   // segment auto-allocation pool (default shown)
  dns_suffix       = "vmlab.internal"  // suffix for auto-registered VM names
  dns_upstream     = "1.1.1.1"         // upstream resolver ip[:port]; default: host resolver
  disk_low_percent = 10                // host.disk_low watchdog threshold (default 10)
  psk              = "secret"          // pre-shared key for cross-host segment peering
  viewer           = "vncviewer {}"    // console viewer command; {} = target
  oci_chunk_size   = 512MiB            // OCI push layer chunk size (default 512M)
}
```

See [the filesystem layout](../references/fact_paths_table.md) for where vmlab keeps the store, cache, state and sockets.

## Related

- [Daemon model](../references/concept_daemon_model.md)

- [Guest OS profiles](../references/concept_profiles.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
