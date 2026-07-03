# vm {} block

_wcl block_

Declares one guest: its template, hardware, NICs, disks, shares and media.

A `vm {}` block declares one guest. Hardware precedence is **VM block > template metadata > profile > defaults**.

```wcl
vm "name" {
  template = "x86_64/linux-modern"   // "<arch>/<name>[@<version>]", "scratch", or registry ref
  arch     = "x86_64"                // REQUIRED for scratch and registry references
  profile  = "linux-modern"          // guest OS profile (hardware defaults)
  gui      = true                    // open QEMU's own display window; headless fallback
  cpus     = 4
  memory   = 8GiB
  disk     = 80GiB                   // primary disk size — scratch VMs only
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
  nic { segment = "dmz" isolated = true }   // port isolation: can't reach segment neighbours

  disk "data"      { size = 10GiB }               // extra blank disk
  disk "formatted" { from = "./folder/" }         // folder copied onto a fresh FAT filesystem

  share { host = "./src"  guest = "/mnt/src" }                  // SMB, auto-mounted when ready
  share { host = "~/data" guest = "D:\\data" readonly = true }  // drive letter on Windows
  share { host = "./old"  guest = "X:" smb1 = true }            // legacy dialect for XP/2003

  media { kind = "iso" from = "./unattend/" label = "UNATTEND" }   // folder → ISO/floppy
}
```

Extra `disk {}` and a `gpu {}` block are declared inline; networking is per-`nic {}` (see [the NIC block](../references/entity_nic_block.md)). A VM must have a NIC on a segment if it declares any shares (validation error otherwise).

## Related

- [lab {} block](../references/entity_labs.md)

- [nic {} block](../references/entity_nic_block.md)

- [share {} block](../references/entity_shares.md)

- [segment {} block](../references/entity_segment_block.md)

- [Guest OS profiles](../references/concept_profiles.md)

- [Templates](../references/concept_templates.md)

- [Scratch VMs](../references/concept_scratch_vms.md)

- [The vmlab.wcl schema](../references/fact_schema_reference.md)

[← Back to SKILL.md](../SKILL.md)
