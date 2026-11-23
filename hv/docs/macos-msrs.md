# The MSRs vmlab-hv answers for a macOS guest

macOS reads Intel model-specific registers that KVM has no model for. KVM
answers such an access with a general-protection fault (#GP). QEMU users worked around this with the host kernel setting
`kvm.ignore_msrs=1`, which makes KVM answer every unknown MSR of every guest
on the host. vmlab-hv needs no host setting. For a machine whose description
says `msr_policy = "macos"` it answers the MSRs in the table below itself,
and nothing else.

vmlab sets `msr_policy = "macos"` for a macOS guest only
(`src/labd/hypervisor/vmlab_hv/describe.rs`). Every other guest keeps the
default, `fault`, where KVM's behaviour is unchanged and vmlab-hv sees no MSR
exit at all.

## Mechanism

For a `macos` machine vmlab-hv enables `KVM_CAP_X86_USER_SPACE_MSR` with the
reason mask `KVM_MSR_EXIT_REASON_UNKNOWN`. KVM then hands an access to an MSR
it does not know to vmlab-hv as a `KVM_EXIT_X86_RDMSR` or
`KVM_EXIT_X86_WRMSR` exit, in place of injecting #GP. MSRs that KVM models
never reach vmlab-hv. An access that KVM rejects for another reason (an
invalid value, an MSR filter) keeps its #GP.

`crates/vmlab-hv-vmm/src/macos.rs` holds the table (`MACOS_MSRS`) and the
decision (`msr_action`):

- A read of a table entry returns 0.
- A write to a table entry is dropped where the register is writable on real
  hardware, and raises #GP where the register is read-only there.
- Every other unknown MSR raises #GP, as it does for any other guest.

The first access to each index is logged once per machine. The set of logged
indices is saved in a snapshot, so a restore or a reset logs nothing twice.

```text
kvm: macOS MSR 0x000000e7 (IA32_MPERF) answered in user space
kvm: macOS guest faulted on unhandled MSR 0x00000123 (read)
```

The second line names an MSR outside the table. If a later macOS version
panics early in boot, look for this line in the machine's vmlab-hv log: it
gives the index to add here and to `MACOS_MSRS`.

## The list

Every read returns 0. That is the value QEMU's guests saw under
`kvm.ignore_msrs=1`, and for each of these registers it is also a value real
hardware can show: a counter that has not counted, or a status with no bit
set. A test (`document_lists_exactly_the_table`) fails when this table and
`MACOS_MSRS` differ.

| MSR | Name | Read | Write | Reason |
|---|---|---|---|---|
| `0xe7` | `IA32_MPERF` | 0 | dropped | Counts at the TSC rate while the core is in C0. XNU's power statistics sample it with `IA32_APERF` for each CPU. 0 is a counter that has not counted. The register is writable on hardware, so a write succeeds. The value is not stored, because the read stays 0. |
| `0xe8` | `IA32_APERF` | 0 | dropped | Counts at the delivered frequency while the core is in C0. Read and written as `IA32_MPERF`. |
| `0x3f8` | `MSR_PKG_C3_RESIDENCY` | 0 | #GP | Time the package spent in C3. XNU's power statistics read all the residency counters. 0 says the package never entered the state, which is true for a vCPU. Read-only on hardware. |
| `0x3f9` | `MSR_PKG_C6_RESIDENCY` | 0 | #GP | Package C6 residency counter. As `0x3f8`. |
| `0x3fa` | `MSR_PKG_C7_RESIDENCY` | 0 | #GP | Package C7 residency counter. As `0x3f8`. |
| `0x3fc` | `MSR_CORE_C3_RESIDENCY` | 0 | #GP | Time the core spent in C3. As `0x3f8`, per core. |
| `0x3fd` | `MSR_CORE_C6_RESIDENCY` | 0 | #GP | Core C6 residency counter. As `0x3fc`. |
| `0x3fe` | `MSR_CORE_C7_RESIDENCY` | 0 | #GP | Core C7 residency counter. As `0x3fc`. |
| `0x60d` | `MSR_PKG_C2_RESIDENCY` | 0 | #GP | Package C2 residency counter. As `0x3f8`. |
| `0x61d` | `MSR_LLC_FLUSHED_RESIDENCY_TIMER` | 0 | #GP | XNU's name for it (`MSR_IA32_LLC_FLUSHED_RESIDENCY_TIMER`). XNU reads it with the package residency counters as the time the last-level cache was flushed. 0 says never. |
| `0x621` | `MSR_RING_PERF_STATUS` | 0 | #GP | The ratio the uncore (ring) runs at, `MSR_UNCORE_PERF_STATUS` in Intel's manual. A status register, read-only on hardware. XNU reports it in its power statistics and does not act on it. |
| `0x630` | `MSR_PKG_C8_RESIDENCY` | 0 | #GP | Package C8 residency counter. As `0x3f8`. |
| `0x631` | `MSR_PKG_C9_RESIDENCY` | 0 | #GP | Package C9 residency counter. As `0x3f8`. |
| `0x632` | `MSR_PKG_C10_RESIDENCY` | 0 | #GP | Package C10 residency counter. As `0x3f8`. |
| `0x64f` | `MSR_CORE_PERF_LIMIT_REASONS` | 0 | dropped | Status and log bits that say why the core frequency was limited (thermal, power, current). 0 says nothing limits it. The log bits are cleared by writing 0 on hardware, so a write succeeds and has nothing to clear. |
| `0x6b0` | `MSR_GRAPHICS_PERF_LIMIT_REASONS` | 0 | dropped | The same register for the processor graphics. As `0x64f`. |

## Where the list comes from

The list is every index a boot of `x86_64/macos-tahoe` (macOS 26.7.2, build
25G332) to agent-ready touched that KVM did not handle, on helios (an AMD
host) with `kvm.ignore_msrs=N`. vmlab-hv logged them itself through the
mechanism above. `macos-verification.md` records the runs.

The 16 indices are the ones XNU's power statistics read, in the published
XNU source: `osfmk/i386/Diagnostics.c` (`diagCall64`'s power statistics and
`cpu_powerstats`) and `osfmk/i386/pmCPU.c`. The names in the table follow
`osfmk/i386/proc_reg.h` without its `IA32_` prefix where Intel's manual has
none. That code only reads them, and it reads them with `rdmsr_carefully`,
which survives a #GP and leaves its result unset. vmlab-hv answers them
anyway, so the statistics hold a defined 0, and so that a reader that is not
careful (a closed kernel extension) gets the same answer.

KVM answers the RAPL energy registers that the same code reads (`0x611`,
`0x619`, `0x639`, `0x641`) with 0 on its own, so they never reach vmlab-hv
and are not in the table.

The list is not complete for every macOS version or every code path. A boot
on an Intel host can differ, because KVM models a few more Intel MSRs there.
That only shortens what reaches vmlab-hv. The acceptance test
(`macos_tahoe_p1_agent_display_keyboard_network_reset_snapshot_stop`) fails
when the guest touches an MSR outside the table, and names it.
