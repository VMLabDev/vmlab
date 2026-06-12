# Lab definition reference (vmlab.wcl)

The lab file is `vmlab.wcl`, located by walking up from cwd. Lab-local
working data (clones, snapshots, built media, screenshots) lives in
`.vmlab/` beside it — gitignore that. Every file starts with
`import <vmlab.wcl>`.

## Minimal lab

```wcl
import <vmlab.wcl>

lab "demo" {
  gui = true                  // optional, lab-wide default: show each guest's
                              // screen in a QEMU window (VM `gui` overrides;
                              // silently headless when no display server)
  vm "box" {
    template = "x86_64/linux-modern"
    memory   = "2G"
    nic { nat = true }
  }
}
```

Closing a `gui` window kills that VM (QEMU semantics) — it surfaces as
`vm.crashed`. The VNC socket (`vmlab console`) is available either way.

## Worked example (examples/ad-lab/vmlab.wcl)

```wcl
import <vmlab.wcl>

lab "ad-lab" {

  segment "corp" {
    subnet = "10.50.0.0/24"
    // Hand out the DC as DNS instead of the daemon (AD owns DNS).
    dns { server = "10.50.0.10" }
    route { dest = "10.60.0.0/24" via = "10.50.0.254" }
  }

  segment "dmz" { }          // zero-config: auto subnet, daemon DHCP/DNS

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    cpus     = 4
    memory   = "8G"
    nic { segment = "corp" ip = "10.50.0.10" }   // static → DHCP reservation
  }

  vm "client01" {
    template   = "x86_64/windows-11@26100.1"     // version pin
    depends_on = ["dc01"]                        // boot ordering
    nic { segment = "corp" }                     // dynamic lease
  }

  vm "buildbox" {
    template = "x86_64/linux-modern"
    nic { nat = true }
  }

  provision "scripts/setup.wisp" { vms = ["dc01"] }

  on "vm.crashed"    { run = "scripts/collect-dumps.wisp" }
  on "host.disk_low" { run = "scripts/alert.wisp" }
}
```

## `segment` block

```wcl
segment "name" {
  subnet    = "10.0.0.0/24"      // CIDR; auto-allocated from host pool if omitted
  global    = true               // owned by the supervisor, shared across labs (§9.2)
  dhcp      = true               // default true
  nat       = true               // internet egress for this segment (default false)
  routes_to = ["other_segment"]  // daemon inter-segment routing opt-in

  dns { server = "10.0.0.10" }   // hand out this server via DHCP instead of daemon DNS
  dns { enabled = false }        // or suppress DNS entirely

  connect { host = "helios:9999" }              // cross-host peer supervisor (PSK from host config)

  route   { dest = "10.60.0.0/24" via = "10.50.0.254" }   // pushed via DHCP option 121
  record  { name = "srv" ip = "10.0.0.5" }                // static DNS; wildcards OK ("*.internal")
  forward { host_port = 3389 to = "dc01:3389" proto = "tcp" }  // proto: "tcp" (default) | "udp" | "both"
  block   { cidr = "192.0.2.0/24" }                       // optional: proto = "tcp"|"udp"|"icmp", port = 443
  redirect { from = "10.0.0.1:80" to = "10.0.0.2:8080" }  // DNAT "ip[:port]"; optional proto
  sinkhole { pattern = "*.telemetry.com" mode = "nxdomain" }  // mode: "nxdomain" (default) | "zero"
}
```

`record` and `sinkhole` blocks may also appear at lab level (lab-wide).

## `vm` block

```wcl
vm "name" {
  template = "x86_64/linux-modern"   // "<arch>/<name>[@<version>]", "scratch", or registry ref
  arch     = "x86_64"                // REQUIRED for scratch and registry references
  profile  = "linux-modern"          // guest OS profile (see host-config.md)
  gui      = true                    // open QEMU's own display window; headless fallback
  cpus     = 4
  memory   = "8G"
  disk     = "80G"                   // primary disk size — scratch VMs only
  cdrom    = "./isos/drivers.iso"    // paths relative to lab root
  floppy   = "./unattend.img"
  depends_on  = ["dc01"]             // wait for these VMs (and their scoped provisions) first
  nested      = true                 // nested virtualisation
  display     = "virtio-gpu"
  firmware    = "ovmf"               // "ovmf" | "seabios"
  tpm         = true
  secure_boot = true
  qemu_args   = ["-machine", "q35,smm=on"]   // escape hatch, appended last

  gpu { mode = "passthrough" address = "0000:01:00.0" }   // mode: "passthrough"|"virgl"|"vulkan"

  nic { segment = "corp" ip = "10.50.0.10" mac = "52:54:00:aa:bb:cc" }
  nic { nat = true }                 // shorthand: per-lab built-in NAT segment
  nic { segment = "dmz" isolated = true }   // port isolation: guest can't reach segment neighbours

  disk "data"      { size = "10G" }               // extra blank disk
  disk "formatted" { from = "./folder/" }         // folder copied onto a fresh FAT filesystem

  share { host = "./src"   guest = "/mnt/src" }                          // SMB, auto-mounted when ready
  share { host = "~/data"  guest = "D:\\data" readonly = true }          // drive letter on Windows
  share { host = "./old"   guest = "X:" smb1 = true }                    // legacy dialect for XP/2003
  // share also takes name = "..." (derived from guest path if omitted)

  media { kind = "iso" from = "./unattend/" label = "UNATTEND" }   // folder → ISO/floppy, content-addressed cache
}
```

Hardware precedence: **VM block > template metadata > profile > defaults**.

SMB shares: served by the lab daemon at the segment gateway
(`\\<gateway>\<share>`); credentials auto-generated per lab; guest agent
mounts them once the VM is ready. Share contents are outside snapshot scope.
The VM must have a NIC on a segment (validation error otherwise).

## Provisions and event handlers

```wcl
provision "scripts/setup.wisp" { }                  // run on `vmlab up`, in declaration order
provision "scripts/join.wisp"  { vms = ["client01"] }  // scoped: gates depends_on on these VMs

on "vm.crashed"    { run = "scripts/collect-dumps.wisp" }
on "host.disk_low" { run = "scripts/alert.wisp" }
```

Events: `vm.starting`, `vm.ready`, `vm.stopped`, `vm.crashed`, `lab.up`,
`lab.down`, `snapshot.created`, `snapshot.restored`, `template.built`,
`lab.daemon_crashed`, `host.disk_low`. Provision failures fail `vmlab up`;
handler failures are logged, never fatal.

## What `vmlab validate` checks

WCL schema; template refs exist in store (or registry ref + explicit
`arch`); NIC segments are declared; static IPs inside the declared subnet;
no duplicate static IPs/MACs; no `depends_on` cycles; provision/handler
script files exist AND compile (full wisp type-check); scratch VMs have
`arch` + `profile` + `disk`.

Source of truth: PRD §5, §7.5, §8; `src/config/schema.wcl`;
`examples/ad-lab/vmlab.wcl`.
