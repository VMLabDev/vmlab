# The vmlab.wcl schema

A complete reference of the `vmlab.wcl` (and host `config.wcl`) schema, reflected straight from `src/config/schema.wcl` / `host_schema.wcl` with WCL's reflection builtins (`child_types` / `type_fields`) and the wdoc `type_table` component — so it can never drift from the code. Each block lists its attributes (type, whether required, description), any nested blocks, and a worked example. Descriptions are the fields' `@doc` annotations.

## `lab` block

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Lab name (DNS label, ≤63 chars); the inline block label |
| `gui` | `bool` | no | Default for all VMs: open a VNC viewer on `up`; VM `gui` overrides |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `segments` | `segment` | yes | Virtual L2 network segments in this lab |
| `vms` | `vm` | yes | The VMs in this lab |
| `containers` | `container` | yes | OCI containers in this lab, each run in a micro-VM |
| `provisions` | `provision` | yes | wscript provision scripts run on `vmlab up`, in declaration order |
| `playbooks` | `playbook` | yes | config-weave playbooks applied on `vmlab up`, interleaved with provisions in declaration order |
| `handlers` | `on` | yes | Lifecycle event handlers (failures are logged, never fatal) |
| `records` | `record` | yes | Lab-wide static DNS entries (wildcards allowed) |
| `sinkholes` | `sinkhole` | yes | Lab-wide DNS sinkholes |

Example:

```wcl
lab "demo" {
  gui = true                       // lab-wide default: show each guest's screen
  vm "box" {
    template = "x86_64/linux-modern"
    memory   = 2GiB
    nic { nat = true }
  }
}
```

### `segment` (in `lab`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Segment name (DNS label); unique per lab; the inline block label |
| `subnet` | `utf8` | no | CIDR; auto-allocated as a /24 from the host pool if omitted |
| `global` | `bool` | no | Owned by the supervisor and shared across labs |
| `dhcp` | `bool` | no | Enable DHCP (default true) |
| `nat` | `bool` | no | Enable NAT/internet egress for this segment (default false) |
| `mtu` | `i64` | no | Link MTU (576–65535); default jumbo (9000) on nat/global, else 1500 |
| `routes_to` | `list<utf8>` | no | Names of other segments to route to — daemon inter-segment routing opt-in |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `dns` | `dns` | no | DNS service override: hand out another server, or opt out |
| `connect` | `connect` | no | Cross-host segment peer over TCP (PSK from host config) |
| `routes` | `route` | yes | Guest routes pushed via DHCP option 121 |
| `records` | `record` | yes | Static DNS entries for this segment (wildcards allowed) |
| `forwards` | `forward` | yes | Host→guest port forwards |
| `block_rules` | `block` | yes | L3 block rules at the switch |
| `redirect_rules` | `redirect` | yes | L3 DNAT redirect rules |
| `sinkholes` | `sinkhole` | yes | DNS sinkhole rules |

Example:

```wcl
segment "corp" {
  subnet = "10.50.0.0/24"          // omit to auto-allocate a /24 from the host pool
  nat    = true                    // internet egress for this segment
  record { name = "dc01" ip = "10.50.0.10" }
}
```

#### `dns` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `server` | `utf8` | no | IPv4 of the DNS server to hand out via DHCP instead of the daemon |
| `enabled` | `bool` | no | Hand out a DNS server at all (default true); false suppresses the DHCP option |

Example:

```wcl
dns { server = "10.50.0.10" }      // hand out a DC as the resolver via DHCP
dns { enabled = false }            // …or suppress DNS on the segment entirely
```

#### `connect` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `host` | `utf8` | yes | Remote supervisor `host[:port]` to bridge this segment with (required) |

Example:

```wcl
connect { host = "helios:9999" }   // bridge this segment to a peer supervisor (PSK)
```

#### `route` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `dest` | `utf8` | yes | Destination CIDR, e.g. `10.60.0.0/24` (required) |
| `via` | `utf8` | yes | Gateway IPv4 the route points at (required) |

Example:

```wcl
route { dest = "10.60.0.0/24" via = "10.50.0.254" }   // pushed via DHCP option 121
```

#### `record` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | DNS name to resolve; wildcards allowed, e.g. `*.internal` (required) |
| `ip` | `utf8` | yes | IPv4 address the name resolves to (required) |

Example:

```wcl
record { name = "srv" ip = "10.50.0.5" }     // wildcards OK: name = "*.internal"
```

#### `forward` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `host_port` | `i64` | yes | Host port to listen on (1–65535); unique across the lab (required) |
| `to` | `utf8` | yes | Target as `vm:port`; the VM must be declared (required) |
| `proto` | `utf8` | no | Protocol: `tcp` (default) \| `udp` \| `both` |

Example:

```wcl
forward { host_port = 13389 to = "dc01:3389" proto = "tcp" }
```

#### `block` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `cidr` | `utf8` | yes | IPv4 CIDR to drop traffic to/from (required) |
| `proto` | `utf8` | no | Protocol to scope the rule: `tcp` \| `udp` \| `icmp` |
| `port` | `i64` | no | Port to scope the rule (1–65535); requires `proto` |

Example:

```wcl
block { cidr = "192.0.2.0/24" proto = "tcp" port = 443 }
```

#### `redirect` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `from` | `utf8` | yes | Match destination as `ip[:port]` (required) |
| `to` | `utf8` | yes | Rewrite destination to `ip[:port]` (required) |
| `proto` | `utf8` | no | Protocol to scope the rule: `tcp` \| `udp` |

Example:

```wcl
redirect { from = "10.50.0.254:53" to = "10.50.0.10:53" proto = "udp" }
```

#### `sinkhole` (in `segment`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `pattern` | `utf8` | yes | DNS name pattern to sink; wildcards allowed (required) |
| `mode` | `utf8` | no | Response: `nxdomain` (default) \| `zero` (resolve to 0.0.0.0) |

Example:

```wcl
sinkhole { pattern = "*.telemetry.com" mode = "nxdomain" }   // or mode = "zero"
```

### `vm` (in `lab`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | VM name (DNS label); unique per lab; the inline block label |
| `template` | `utf8` | yes | `<arch>/<name>[@<version>]`, `scratch`, or an OCI registry ref (required) |
| `arch` | `utf8` | no | Architecture; required for `scratch` and registry references |
| `profile` | `utf8` | no | Guest OS profile (hardware defaults); required for `scratch` |
| `cpus` | `i64` | no | vCPU count (> 0); inherited from template→profile if omitted |
| `memory` | `std.ByteSize` | no | RAM as a byte size, e.g. `8GiB`/`512MiB`; inherited if omitted |
| `disk` | `std.ByteSize` | no | Primary disk size, e.g. `64GiB` — scratch VMs only (rejected on cloned VMs) |
| `cdrom` | `utf8` | no | Path to an ISO to attach as a CD-ROM (relative to lab root) |
| `floppy` | `utf8` | no | Path to a floppy image to attach (relative to lab root) |
| `depends_on` | `list<utf8>` | no | VM names to wait for before this one (no cycles) |
| `nested` | `bool` | no | Enable nested virtualisation (host CPU passthrough) |
| `gui` | `bool` | no | Open a VNC viewer on `up`; the VM always runs headless |
| `display` | `utf8` | no | QEMU display string; inherited from template→profile if omitted |
| `firmware` | `utf8` | no | Firmware: `ovmf` \| `seabios`; inherited from template→profile |
| `tpm` | `bool` | no | Enable a TPM 2.0 device; inherited from template→profile |
| `secure_boot` | `bool` | no | Enable secure boot (OVMF only); inherited from template→profile |
| `qemu_args` | `list<utf8>` | no | Raw QEMU flags appended last — escape hatch |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `gpu` | `gpu` | no | GPU acceleration (passthrough / virgl / vulkan) |
| `nics` | `nic` | yes | Network interfaces; no NICs = air-gapped (shares need ≥1 NIC) |
| `extra_disks` | `disk` | yes | Additional disks beyond the primary disk |
| `shares` | `share` | yes | SMB shared folders (require ≥1 NIC) |
| `media` | `media` | yes | ISO/floppy images built from a folder |
| `web` | `web` | yes | HTTP UIs served in the guest, proxied into the web console (require ≥1 NIC) |

Example:

```wcl
vm "dc01" {
  template = "x86_64/windows-2025"
  cpus     = 4
  memory   = 8GiB
  nic   { segment = "corp" ip = "10.50.0.10" }
  share { host = "./src" guest = "D:\\src" }
}
```

#### `gpu` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `mode` | `utf8` | yes | Mode: `passthrough` \| `virgl` \| `vulkan` (required) |
| `address` | `utf8` | no | Host PCI address, e.g. `0000:01:00.0` — required for `passthrough` |

Example:

```wcl
gpu { mode = "passthrough" address = "0000:01:00.0" }   // or mode = "virgl" | "vulkan"
```

#### `nic` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `segment` | `utf8` | no | Segment name to attach to; required unless `nat = true` |
| `nat` | `bool` | no | Shorthand: attach to the per-lab built-in NAT segment |
| `ip` | `utf8` | no | Static IPv4 (becomes a DHCP reservation); must be in the subnet, unique |
| `gateway` | `bool` | no | Make this NIC the segment gateway; it must own the subnet's first usable address |
| `mac` | `utf8` | no | Fixed MAC, e.g. `52:54:00:ab:cd:ef`; generated and persisted otherwise |
| `isolated` | `bool` | no | Port isolation: reach gateway/forwards but not segment neighbours |

Example:

```wcl
nic { segment = "corp" ip = "10.50.0.10" mac = "52:54:00:aa:bb:cc" }
nic { nat = true }                       // per-lab built-in NAT segment shorthand
nic { segment = "dmz" isolated = true }  // port isolation
```

#### `disk` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Disk identifier; the inline block label |
| `size` | `std.ByteSize` | no | Blank disk size, e.g. `10GiB`; one of `size`/`from` is required |
| `from` | `utf8` | no | Folder copied onto a fresh FAT filesystem; one of `size`/`from` is required |

Example:

```wcl
disk "data"      { size = 10GiB }         // extra blank disk
disk "formatted" { from = "./payload/" }  // folder copied onto a fresh FAT filesystem
```

#### `share` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `host` | `utf8` | yes | Host directory to share; must exist (required) |
| `guest` | `utf8` | yes | Guest mount path, e.g. `/mnt/src` or `D:\data` (required) |
| `readonly` | `bool` | no | Mount read-only (default false) |
| `smb1` | `bool` | no | Enable the SMB1 dialect + auth relaxation for XP/2003-era guests |
| `name` | `utf8` | no | Share name; derived from the guest path if omitted |
| `transport` | `utf8` | no | Transport: auto (default; virtiofs when host + guest support it, else SMB) \| virtiofs \| smb |

Example:

```wcl
share { host = "./src"  guest = "/mnt/src" }
share { host = "~/data" guest = "D:\\data" readonly = true }
share { host = "./old"  guest = "X:" smb1 = true }   // legacy dialect for XP/2003
```

#### `media` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `kind` | `utf8` | yes | Image kind: `iso` \| `floppy` (required) |
| `from` | `utf8` | yes | Source folder built into the image; must exist (required) |
| `label` | `utf8` | no | Volume label for the image |

Example:

```wcl
media { kind = "iso"    from = "./unattend/" label = "CIDATA" }
media { kind = "floppy" from = "./drivers/"  label = "DRV" }
```

#### `web` (in `vm`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Page name (DNS label); unique per machine; the inline block label |
| `port` | `i64` | yes | Guest TCP port serving the HTTP UI (1–65535) (required) |
| `path` | `utf8` | no | Initial path opened in the console (default `/`) |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `auth` | `auth` | no | Credentials the proxy injects so the guest app's own login never prompts |

### `container` (in `lab`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `id` | `identifier` | no | Name used to connect the shape (`a -> b`) and to anchor others to it. |
| `class` | `list<utf8>` | no | Style classes — text and SVG paint via the `class` system. |
| `link` | `utf8` | no | Link the shape to an in-site page (bare page name, or `site:page`). Wraps it in a clickable `<a>`; an unknown page fails the build like a bad prose link. |
| `stroke` | `utf8` | no | Optional chrome — outline colour of the background rect that makes the group visible. |
| `fill` | `utf8` | no | Optional chrome — fill colour of the background rect that makes the group visible. |
| `padding` | `f64` | no | Inset between the chrome and the child shapes. |
| `width` | `f64` | no | Declared interior width (when no layout/anchor sizes it). |
| `height` | `f64` | no | Declared interior height (when no layout/anchor sizes it). |
| `layout` | `symbol` | no | Layout mode: `:free` (default, manual) / `:grid` / `:layered` / `:force` / `:radial`. |
| `columns` | `i64` | no | Number of columns for `:grid` layout. |
| `cell_width` | `f64` | no | Grid cell width for `:grid` layout. |
| `cell_height` | `f64` | no | Grid cell height for `:grid` layout. |
| `gap` | `f64` | no | Gap between cells in `:grid` layout. |
| `direction` | `symbol` | no | Flow direction for `:layered`: `:top_to_bottom` (default) / `:left_to_right`. |
| `layer_gap` | `f64` | no | Spacing between ranks (layers) in `:layered` layout. |
| `node_gap` | `f64` | no | Spacing between nodes within a rank in `:layered` layout. |
| `iterations` | `i64` | no | `:force` relaxation steps (default 300). |
| `repulsion` | `f64` | no | `:force` node repulsion strength (default 9000). |
| `link_distance` | `f64` | no | `:force` ideal edge-to-edge length (default 60). |
| `gravity` | `f64` | no | `:force` centering pull (default 0.05). |
| `seed` | `i64` | no | `:force` random seed for reproducible layouts (default 1). |
| `hub` | `identifier` | no | `:radial` hub shape id (defaults to the highest-degree shape). |
| `radius` | `f64` | no | `:radial` radius of the first ring (default: auto-fit to shape sizes). |
| `ring_gap` | `f64` | no | `:radial` added radius per successive ring (default 120). |
| `start_angle` | `f64` | no | `:radial` angle (radians) of the first shape on each ring (default -PI/2, i.e. top). |
| `anchor_left` | `f64` | no | Fractional anchor (0–1) pinning the left edge to the parent box. |
| `anchor_right` | `f64` | no | Fractional anchor (0–1) pinning the right edge to the parent box. |
| `anchor_top` | `f64` | no | Fractional anchor (0–1) pinning the top edge to the parent box. |
| `anchor_bottom` | `f64` | no | Fractional anchor (0–1) pinning the bottom edge to the parent box. |
| `connect_points` | `list<AnchorSide>` | no | Which sides (`:left`/`:right`/`:top`/`:bottom`) edges attach to. |
| `icon` | `utf8` | no | Icon-badge icon (a `pack.name`). |
| `icon_size` | `f64` | no | Icon-badge size. |
| `icon_pos` | `IconPos` | no | Icon-badge position (`:center` / `:top_left` / …). |
| `icon_class` | `list<utf8>` | no | Icon-badge style classes. |
| `edges` | `list<Edge>` | yes | Edges connecting child shapes (`a -> b`). |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `children` | `SvgBlock` | yes | The child shapes laid out by the container. |

Example:

```wcl
container "web" {
  image      = "nginx:1.27"              // docker.io shorthand; @sha256:… pins
  mode       = :workload                  // :workload (default) | :idle
  depends_on = ["db"]                    // VM or container names — one namespace
  restart    = "on-failure"              // "no" (default) | "on-failure" | "always"
  nic    { segment = "corp" ip = "10.50.0.20" }
  env    { name = "MODE" value = "prod" }
  volume { name = "data" target = "/var/lib/data" }
  port   { host = 18080 container = 80 }
  healthcheck { command = ["curl", "-fsS", "http://localhost/"] interval = 5s }
}
```

#### `nic` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `segment` | `utf8` | no | Segment name to attach to; required unless `nat = true` |
| `nat` | `bool` | no | Shorthand: attach to the per-lab built-in NAT segment |
| `ip` | `utf8` | no | Static IPv4 (becomes a DHCP reservation); must be in the subnet, unique |
| `gateway` | `bool` | no | Make this NIC the segment gateway; it must own the subnet's first usable address |
| `mac` | `utf8` | no | Fixed MAC, e.g. `52:54:00:ab:cd:ef`; generated and persisted otherwise |
| `isolated` | `bool` | no | Port isolation: reach gateway/forwards but not segment neighbours |

Example:

```wcl
nic { segment = "corp" ip = "10.50.0.10" mac = "52:54:00:aa:bb:cc" }
nic { nat = true }                       // per-lab built-in NAT segment shorthand
nic { segment = "dmz" isolated = true }  // port isolation
```

#### `env` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Variable name (required) |
| `value` | `utf8` | yes | Variable value (required) |

Example:

```wcl
env { name = "PGUSER" value = "app" }
```

#### `volume` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `host` | `utf8` | no | Host path to bind-mount, relative to the lab root; one of `host`/`name` is required |
| `name` | `utf8` | no | Named volume kept under the lab dir, shared by name, retained until lab destroy; one of `host`/`name` |
| `target` | `utf8` | yes | Absolute mount path inside the container (required) |
| `read_only` | `bool` | no | Mount read-only (default false) |

Example:

```wcl
volume { name = "data"  target = "/var/lib/data" }               // named, lab-scoped
volume { host = "./www" target = "/srv/www" read_only = true }   // host bind
```

#### `port` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `host` | `i64` | yes | Host port to listen on (1–65535); unique across the lab (required) |
| `container` | `i64` | yes | Container port to forward to (1–65535) (required) |
| `proto` | `utf8` | no | Protocol: `tcp` (default) \| `udp` \| `both` |

Example:

```wcl
port { host = 18080 container = 80 proto = "tcp" }   // sugar for a segment forward
```

#### `healthcheck` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `command` | `list<utf8>` | yes | Probe command run inside the container (exec form); exit 0 = healthy (required) |
| `interval` | `std.Duration` | no | Time between probes, e.g. `10s` (default 10s) |
| `timeout` | `std.Duration` | no | Per-probe timeout (default 5s) |
| `retries` | `i64` | no | Consecutive failures before unhealthy (default 3) |
| `start_period` | `std.Duration` | no | Grace period after start before failures count (default 10s) |

Example:

```wcl
healthcheck {
  command      = ["curl", "-fsS", "http://localhost/"]   // exit 0 = healthy
  interval     = 10s
  timeout      = 5s
  retries      = 3
  start_period = 10s
}
```

#### `web` (in `container`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Page name (DNS label); unique per machine; the inline block label |
| `port` | `i64` | yes | Guest TCP port serving the HTTP UI (1–65535) (required) |
| `path` | `utf8` | no | Initial path opened in the console (default `/`) |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `auth` | `auth` | no | Credentials the proxy injects so the guest app's own login never prompts |

### `provision` (in `lab`)

Provision script run during `vmlab up`. Optional vms list scopes
the script for depends_on satisfaction.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `script` | `utf8` | yes | Path to the `.ws` file; must exist and compile; the inline label |
| `vms` | `list<utf8>` | no | VM names this script is scoped to (gates their `depends_on`) |

Example:

```wcl
provision "scripts/setup.ws" { }                     // runs on `vmlab up`, in order
provision "scripts/join.ws"  { vms = ["client01"] }  // scoped: gates depends_on
```

### `playbook` (in `lab`)

config-weave playbook applied on `vmlab up` (interleaved with provisions
in declaration order) and runnable on demand via `vmlab playbook check|apply`.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `utf8` | yes | Playbook folder (contains `playbook.wcl`), relative to the lab root; the inline label |
| `play` | `utf8` | yes | Play name inside the playbook to run (required) |
| `vms` | `list<utf8>` | no | VM/container names this playbook targets; empty/absent = every machine |

### `on` (in `lab`)

Event handler binding.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `event` | `utf8` | yes | Event name to handle, e.g. `vm.crashed`; the inline block label |
| `run` | `utf8` | yes | Path to the handler `.ws` file; must exist and compile (required) |
| `targets` | `list<utf8>` | no | Optional VM/container names; empty handles every occurrence of the event |

Example:

```wcl
on "vm.crashed"    { run = "scripts/collect-dumps.ws" }
on "host.disk_low" { run = "scripts/alert.ws" }
```

### `record` (in `lab`)

Static DNS entry (wildcards allowed in name).

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | DNS name to resolve; wildcards allowed, e.g. `*.internal` (required) |
| `ip` | `utf8` | yes | IPv4 address the name resolves to (required) |

Example:

```wcl
record { name = "srv" ip = "10.50.0.5" }     // wildcards OK: name = "*.internal"
```

### `sinkhole` (in `lab`)

DNS sinkhole: NXDOMAIN by default, or 0.0.0.0 with mode = "zero".

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `pattern` | `utf8` | yes | DNS name pattern to sink; wildcards allowed (required) |
| `mode` | `utf8` | no | Response: `nxdomain` (default) \| `zero` (resolve to 0.0.0.0) |

Example:

```wcl
sinkhole { pattern = "*.telemetry.com" mode = "nxdomain" }   // or mode = "zero"
```

## `template` block

Template definition, buildable with `vmlab template build`.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Template name, e.g. `linux-modern`; the inline block label |
| `arch` | `utf8` | yes | Architecture — selects the QEMU system emulator (required) |
| `version` | `utf8` | yes | Version string, non-empty; name+arch+version is unique (required) |
| `registry` | `utf8` | no | Full OCI repo to publish to / version-bump against |
| `profile` | `utf8` | no | Guest OS profile (hardware defaults) for the build VM |
| `cpus` | `i64` | no | vCPU count for the build VM; inherited by clones |
| `memory` | `std.ByteSize` | no | RAM for the build VM, e.g. `8GiB`; inherited by clones |
| `disk` | `std.ByteSize` | no | Working disk size for the build, e.g. `64GiB`; required for `scratch` source |
| `display` | `utf8` | no | QEMU display string for the build VM |
| `firmware` | `utf8` | no | Firmware: `ovmf` \| `seabios` |
| `tpm` | `bool` | no | Enable a TPM 2.0 device |
| `secure_boot` | `bool` | no | Enable secure boot (OVMF only) |
| `nested` | `bool` | no | Enable nested virtualisation for the build VM |
| `gui` | `bool` | no | Watch the build VM via a VNC viewer |
| `qemu_args` | `list<utf8>` | no | Raw QEMU flags for the build VM — escape hatch |
| `first_boot` | `utf8` | no | wscript run on first instantiation of a clone, before ready |
| `agent` | `bool` | no | Bake the vmlab-agent service into the image (default true) |

#### Child blocks

| Slot | Accepts | Multiple | Description |
| --- | --- | --- | --- |
| `source` | `source` | no | What the build starts from — exactly one of four forms (required) |
| `media` | `media` | yes | ISO/floppy images attached to the build |
| `provisions` | `provision` | yes | Provision scripts that drive the build |
| `playbooks` | `playbook` | yes | config-weave playbooks applied to the build VM, interleaved with provisions in declaration order; steps stream as structured build progress |
| `nics` | `nic` | yes | NICs for the build VM (optional; the build VM may be air-gapped) |
| `extra_disks` | `disk` | yes | Additional disks attached during the build |

Example:

```wcl
template "linux-modern" {
  arch    = "x86_64"
  version = "1.0"
  profile = "linux-modern"
  disk    = 20GiB                  // working disk size for the build
  source "iso" { url = "https://releases.ubuntu.com/.../x.iso" sha256 = "abc123…" }
  provision "scripts/install.ws" { }
}
```

### `source` (in `template`)

Template build source: exactly one of the four forms.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `kind` | `utf8` | yes | Source kind: `iso` \| `qcow2` \| `template` \| `scratch`; the inline label |
| `path` | `utf8` | no | Local file path — `iso`/`qcow2`; mutually exclusive with `url` |
| `url` | `utf8` | no | Remote artefact URL — `iso`/`qcow2`; requires `sha256` |
| `sha256` | `utf8` | no | SHA-256 of the remote artefact; required with `url` |
| `from` | `utf8` | no | Source template `<arch>/<name>[@<version>]` — kind `template` (layered build) |

Example:

```wcl
source "iso"      { path = "./isos/win11.iso" }           // local installer ISO
source "iso"      { url = "https://…" sha256 = "…" }      // downloaded + verified
source "qcow2"    { path = "./base.qcow2" }               // existing disk as base
source "template" { from = "x86_64/linux-modern@1.0" }    // layered build
source "scratch"  { }                                     // blank disk
```

### `media` (in `template`)

ISO/floppy image built from a folder.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `kind` | `utf8` | yes | Image kind: `iso` \| `floppy` (required) |
| `from` | `utf8` | yes | Source folder built into the image; must exist (required) |
| `label` | `utf8` | no | Volume label for the image |

Example:

```wcl
media { kind = "iso"    from = "./unattend/" label = "CIDATA" }
media { kind = "floppy" from = "./drivers/"  label = "DRV" }
```

### `provision` (in `template`)

Provision script run during `vmlab up`. Optional vms list scopes
the script for depends_on satisfaction.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `script` | `utf8` | yes | Path to the `.ws` file; must exist and compile; the inline label |
| `vms` | `list<utf8>` | no | VM names this script is scoped to (gates their `depends_on`) |

Example:

```wcl
provision "scripts/setup.ws" { }                     // runs on `vmlab up`, in order
provision "scripts/join.ws"  { vms = ["client01"] }  // scoped: gates depends_on
```

### `playbook` (in `template`)

config-weave playbook applied on `vmlab up` (interleaved with provisions
in declaration order) and runnable on demand via `vmlab playbook check|apply`.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `path` | `utf8` | yes | Playbook folder (contains `playbook.wcl`), relative to the lab root; the inline label |
| `play` | `utf8` | yes | Play name inside the playbook to run (required) |
| `vms` | `list<utf8>` | no | VM/container names this playbook targets; empty/absent = every machine |

### `nic` (in `template`)

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `segment` | `utf8` | no | Segment name to attach to; required unless `nat = true` |
| `nat` | `bool` | no | Shorthand: attach to the per-lab built-in NAT segment |
| `ip` | `utf8` | no | Static IPv4 (becomes a DHCP reservation); must be in the subnet, unique |
| `gateway` | `bool` | no | Make this NIC the segment gateway; it must own the subnet's first usable address |
| `mac` | `utf8` | no | Fixed MAC, e.g. `52:54:00:ab:cd:ef`; generated and persisted otherwise |
| `isolated` | `bool` | no | Port isolation: reach gateway/forwards but not segment neighbours |

Example:

```wcl
nic { segment = "corp" ip = "10.50.0.10" mac = "52:54:00:aa:bb:cc" }
nic { nat = true }                       // per-lab built-in NAT segment shorthand
nic { segment = "dmz" isolated = true }  // port isolation
```

### `disk` (in `template`)

Additional disk: blank by size, or pre-formatted from a folder.

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `name` | `utf8` | yes | Disk identifier; the inline block label |
| `size` | `std.ByteSize` | no | Blank disk size, e.g. `10GiB`; one of `size`/`from` is required |
| `from` | `utf8` | no | Folder copied onto a fresh FAT filesystem; one of `size`/`from` is required |

Example:

```wcl
disk "data"      { size = 10GiB }         // extra blank disk
disk "formatted" { from = "./payload/" }  // folder copied onto a fresh FAT filesystem
```

## `host` block

| Property | Type | Required | Description |
| --- | --- | --- | --- |
| `subnet_pool` | `utf8` | no | Segment auto-allocation pool (CIDR); default `10.213.0.0/16` |
| `dns_suffix` | `utf8` | no | Suffix for auto-registered VM names; default `vmlab.internal` |
| `dns_upstream` | `utf8` | no | Upstream resolver `ip[:port]`; default: the host resolver |
| `disk_low_percent` | `i64` | no | `host.disk_low` watchdog threshold percent (0–100); default 10 |
| `psk` | `utf8` | no | Pre-shared key for cross-host segment links |
| `trunk_port` | `i64` | no | TCP listen port for inbound cross-host segment trunks; default 13947 |
| `viewer` | `utf8` | no | VNC viewer command; `{}` is replaced by the target |
| `fastpath` | `utf8` | no | Network fast path: `auto` (probe; default), `off`, `sockmap`, or `afxdp` |
| `oci_chunk_size` | `std.ByteSize` | no | OCI layer chunk size for template push; default `512MiB` |
| `config_weave_bin_dir` | `utf8` | no | Directory holding config-weave guest binaries; default `~/.local/share/config-weave/bin` |

Example:

```wcl
host {
  subnet_pool      = "10.213.0.0/16"   // segment auto-allocation pool (default shown)
  dns_suffix       = "vmlab.internal"
  dns_upstream     = "1.1.1.1"
  disk_low_percent = 10
  viewer           = "vncviewer {}"    // {} = target
  oci_chunk_size   = 512MiB
}
```

## Related

- [lab {} block](../references/entity_labs.md)

- [vm {} block](../references/entity_vms.md)

- [Networking model](../references/concept_networking.md)

- [Templates](../references/concept_templates.md)

- [Host config](../references/concept_host_config.md)

- [What `vmlab validate` checks](../references/fact_validate_checks.md)

[← Back to SKILL.md](../SKILL.md)
