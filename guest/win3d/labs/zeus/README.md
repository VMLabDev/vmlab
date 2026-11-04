# labs/zeus

Labs and scripts for the Zeus laptop runs (Intel Iris Xe and NVIDIA RTX 4060
Max-Q) and the AMD regression run on helios. `docs/zeus.md` explains the runs
and their results.

| Lab | Host | Host GPU it names |
|---|---|---|
| `intel/vmlab.wcl` | Zeus | `Intel1` |
| `nvidia/vmlab.wcl` | Zeus | `Nvidia1` |
| `amd/vmlab.wcl` | helios | `AMD1` |

## vmlab version and the host config

The lab files use the vmlab 0.25.0-alpha form: `gpu { mode = "vulkan"
host_gpu = "<name>" }`. The name resolves to a PCI address through the host
config, `~/.config/vmlab/config.wcl`, on the machine that runs the lab.

Zeus runs vmlab 0.28.0-alpha (since 2026-10-19), and its host config,
`~/.config/vmlab/config.wcl`, is:

```
import <vmlab-host.wcl>

// Host GPUs on zeus. A lab's `gpu {}` names one with `host_gpu`, or takes the default.
gpu "Intel1"  { address = "0000:00:02.0" }                  // Iris Xe RPL-P (i915/ANV)
gpu "Nvidia1" {                                             // RTX 4060 Laptop GPU (nvidia)
  address     = "0000:01:00.0"
  default     = true
}
```

Check the addresses with `lspci -D | grep -Ei 'vga|3d|display'`.
Install the vmlab-hv bundle on Zeus; the lab selects it explicitly.
Its patched library and render server replace the old QEMU host assets.
`labs/zeus/vmlab-iso.sh` changes only `XDG_RUNTIME_DIR` and
`XDG_STATE_HOME`, so its isolated supervisor reads the same host config.

Other tools here: `keepawake/` (keeps the NVIDIA GPU out of runtime D3, no
root), `probe-order.sh` and `envprobe.sh` (single probes in a kept lab).
`run.sh --help` lists its `WIN3D_*` switches.

## Evidence

The 2026-10-17 evidence under [docs/evidence/zeus/](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/zeus/) was taken on vmlab 0.24
with the earlier lab form, `gpu { mode = "vulkan" address = "0000:..." }`. Each run's
copy of the lab file is its `lab-vmlab.wcl` (for example
[docs/evidence/zeus/intel/05-fix-0021/lab-vmlab.wcl](https://github.com/VMLabDev/vmlab-win3d/blob/229203dd8d82c261f4a7a676afb3259fc30ad9ff/docs/evidence/zeus/intel/05-fix-0021/lab-vmlab.wcl)).
