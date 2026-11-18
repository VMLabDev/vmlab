# Legacy x86 template verification on helios

Card #3 (VML-40) covers the 30 template directories whose definitions use
`windows-9x`, `windows-legacy`, or `templeos`. The definitions came from
vmlab-templates `next` at `e717c78054279938f370dcb6b570b1c487d5f45a`.
The CLI and hypervisor were built on helios from vmlab `next` at
`8d7ee0b39d7bd16c33d0ea5dd03c7c7c9e12e61a`. The host build used
`cargo build --bin vmlab` and `cargo build -p vmlab-hv` with separate
target directories under the card's run directory.

Evidence is under `/home/vmlab/src/stc/card-3` on helios. The
`inventory.tsv` file records each definition's name, guest architecture,
version prefix, missing local input, and installed versions at the start
of the run. The fresh build column uses that definition's version prefix.
The boot, exec and snapshot columns apply to the exact sealed version
shown. The store records installer origins and disk hashes. The build
hypervisor is unverified for most existing versions. The four phase 4
gate images match the disk hashes recorded in the
[QEMU hardware captures](../../hv/docs/hwref). The comparison is in
`qemu-capture-hashes.log`.

The first boot probe allows 150 seconds for `vmlab up --wait`, then checks
`vmlab status -v` for `ready=yes` and `hypervisor=vmlab`. Some images
have no agent stamp and report `ready=yes` with `agent=-`. In these cases,
the table records a running VM and marks agent readiness as unverified.
The exec probe prints the guest version or, for TempleOS, a HolyC test
string. The snapshot probe creates and restores an online snapshot and
repeats the exec probe. Labs pin the template version and disable automatic
agent updates. The matrix names each command trace. VM and daemon logs
are under `state/vmlab`.

## Host and media

The run used the `vmlab` account, a private runtime and host
configuration under the card directory, and the account's existing
template store. It reused the guest asset bundle at
`/home/vmlab/src/stc/soak-vml-12/fw-release`. The test labs used
vmlab-hv and the host setting `fastpath = "off"`.

The pinned template tree has no local installer media. The local media
paths below are relative to each definition. Windows for Workgroups
3.11 additionally requires the sealed MS-DOS 6.22 base and generates
`./win311/` from `../iso/Windows-3.11-stock.zip`. The definition
explicitly disables the agent, so its agent readiness and exec checks
need a different acceptance test even after the media is supplied.
Windows 95 OSR2 installation and sealing from real media belong to
card #77.

The FreeDOS installer was downloaded and verified by the definition's
`fetch-deps.sh`. The TempleOS ISO matched the definition's pinned hash.
See `evidence/freedos-1.3/build.log` and `prefetch/templeos.log`.
The first FreeDOS build started while other builds were running because
the process check used an invalid regular expression. It was canceled
through the private supervisor before sealing. The corrected process
check deferred both public-media rebuilds while other builds were active.
See `rebuild.log`, `rebuild-final.log`, and `host-observations.log`.

Installed controls were opened as disposable lab overlays. The card
added zero template versions and zero bytes to the template store.
Observed free space on `/home` and `/var/build` stayed above 300 GB.

## Template matrix

The build column gives the reason the fresh build remains open. Missing
ISO files belong in the templates checkout's `iso/` directory. The control
version is installed locally. A trace filename is relative to
`evidence/<template directory>/`.

In card #3's run, 14 of 27 installed controls passed agent readiness,
exec, and online snapshot restore with post-restore exec. Those include all four
phase 4 gate images. Three definitions had no installed control.
Both Windows 98 controls passed after using the explicit
`C:\COMMAND.COM /c ver` invocation from the legacy test pattern. The
initial bare `ver` attempt failed with error 2, as recorded in
`evidence/{windows-98,windows-98-se}/dos-boot.log`.

| Template directory | Arch | Build on vmlab-hv | Control version | Boot to agent-ready | Exec | Online snapshot round trip | Trace |
| --- | --- | --- | --- | --- | --- | --- | --- |
| dos-6.22 | x86 | Open, `MS-DOS-6.22-bootable.iso` | — | Open | Open | Open | — |
| freedos-1.3 | x86 | Open, build slot unavailable | `1.3.0` | Pass | Pass | Pass | `verify-boot.log` |
| templeos | x86_64 | Open, build slot unavailable | `5.03.2` | Pass | Pass | Pass | `control-boot.log` |
| windows-2000 | x86 | Open, `Windows-2000-Professional-SP4.iso` | — | Open | Open | Open | — |
| windows-3.11 | x86 | Open, stock ZIP and DOS base | — | Agent disabled | Unavailable | Open | — |
| windows-7 | x86_64 | Open, `en_windows_7_enterprise_x64_dvd_x15-70749.iso` | `7600.0.1` | Pass | Pass | Pass | `control-boot.log` |
| windows-7-x86 | x86 | Open, `en_windows_7_enterprise_x86_dvd_x15-70745.iso` | `7600.0.2` | Pass | Pass | Pass | `control-boot.log` |
| windows-8 | x86_64 | Open, `en_windows_8_enterprise_x64_dvd_917522.iso` | `9200.0.0` | Running, no agent | Fail | Restore timed out | `control-boot.log` |
| windows-8-x86 | x86 | Open, `en_windows_8_enterprise_x86_dvd_917587.iso` | `9200.0.1` | Pass | Pass | Pass | `boot-retry-boot.log` |
| windows-8.1 | x86_64 | Open, `en_windows_8_1_enterprise_x64_dvd_2971902.iso` | `9600.0.0` | Running, no agent | Fail | Restore passed, no agent check | `verify-boot.log` |
| windows-8.1-x86 | x86 | Open, `en_windows_8_1_enterprise_x86_dvd_2972289.iso` | `9600.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-95 | x86 | Open, `win95_en.iso` | `4.00.950.4` | Agent wait timed out | Open | Open | `dos-boot.log` |
| windows-95-osr2 | x86 | Deferred to #77 | `4.00.1111.3` | Pass with KVM override and longer wait (F2) | Pass | Pass | Card #83, see F2 |
| windows-95-osr25 | x86 | Open, `win95c_osr25.iso` | `4.03.1216.4` | Agent wait timed out | Open | Open | `dos-boot.log` |
| windows-98 | x86 | Open, `win98_fe_en.iso` | `4.10.1998.3` | Pass | Pass | Pass | `win98-retry-boot.log` |
| windows-98-se | x86 | Open, `win98_se_en.iso` | `4.10.2222.5` | Pass | Pass | Pass | `win98-retry-boot.log` |
| windows-server-2008 | x86_64 | Open, `en_windows_server_2008_x64_dvd_x14-26714.iso` | `6001.0.0` | Running, no agent | Fail | Restore passed, no agent check | `verify-boot.log` |
| windows-server-2008-core | x86_64 | Open, `en_windows_server_2008_x64_dvd_x14-26714.iso` | `6001.0.0` | Running, no agent | Fail | Restore passed, no agent check | `verify-boot.log` |
| windows-server-2008-r2 | x86_64 | Open, `en_windows_server_2008_r2_x64_dvd_x15-50365.iso` | `7601.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2008-r2-core | x86_64 | Open, `en_windows_server_2008_r2_x64_dvd_x15-50365.iso` | `7601.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2008-x86 | x86 | Open, `en_windows_server_2008_x86_dvd_x14-26710.iso` | `6001.0.0` | Running, no agent | Fail | Restore passed, no agent check | `verify-boot.log` |
| windows-server-2008-x86-core | x86 | Open, `en_windows_server_2008_x86_dvd_x14-26710.iso` | `6001.0.0` | Running, no agent | Fail | Restore passed, no agent check | `verify-boot.log` |
| windows-server-2012 | x86_64 | Open, `en_windows_server_2012_vl_x64_dvd_917758.iso` | `9200.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2012-core | x86_64 | Open, `en_windows_server_2012_vl_x64_dvd_917758.iso` | `9200.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2012-r2 | x86_64 | Open, `en_windows_server_2012_r2_x64_dvd_2707946.iso` | `9600.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2012-r2-core | x86_64 | Open, `en_windows_server_2012_r2_x64_dvd_2707946.iso` | `9600.0.0` | Pass | Pass | Pass | `verify-boot.log` |
| windows-server-2016 | x86_64 | Open, `en_windows_server_2016_x64_dvd_9327751.iso` | `14393.0.0` | Pass | Fail | Restore passed, post-restore exec failed | `boot-retry-boot.log` |
| windows-server-2016-core | x86_64 | Open, `en_windows_server_2016_x64_dvd_9327751.iso` | `14393.0.1` | Agent wait timed out | Open | Open | `verify-boot.log` |
| windows-vista | x86_64 | Open, `en_windows_vista_enterprise_x64_dvd_vl_x13-17316.iso` | `6002.0.0` | Running, no agent | Fail | Restore timed out | `verify-boot.log` |
| windows-vista-x86 | x86 | Open, `en_windows_vista_ee_x86_dvd_vl_x13-17271.iso` | `6002.0.0` | Running, no agent | Fail | Restore timed out | `verify-boot.log` |

## Follow-up cards

### F1. Stage licensed media for the legacy template rebuilds

At vmlab-templates `e717c780`, enter any definition marked open for
media in the matrix. Check its local inputs against
`templates/iso/README.md` and `inventory.tsv` in the helios run directory.
Supply the listed installer and any required inputs through the
operator's media flow, run the definition's dependency script if it has
one, then run `vmlab template build` with the vmlab-hv host default and
verify the resulting sealed version. Rebuild MS-DOS 6.22 before Windows
for Workgroups 3.11.

### F2. Retained Windows 95 OSR2 control boots on KVM

The sealed `x86/windows-95-osr2@4.00.1111.3` records `accel = "tcg"`,
which vmlab-hv refuses for x86. Set `hypervisor = "vmlab"` and
`accel = "kvm"` on the VM. The clone inherits the sealed `pentium3`
CPU model and the Windows 9x profile's Cirrus VGA. Card #3 stopped its
KVM probe after 150 seconds (`evidence/windows-95-osr2/osr2-kvm-boot.log`).

On helios, card #83 let that configuration continue. Using card #3's
binaries, a fresh clone reached `vm.ready` 160 seconds after
`vm.starting`. It spent part of the boot in the Cirrus VGA option ROM
before SeaBIOS booted the hard disk. A separate standard VGA probe
reached readiness in 97 seconds. With Cirrus, `vmlab exec guest --
'C:\COMMAND.COM' /c ver` returned `Windows 95. [Version 4.00.1111]`
before and after creating and restoring an online snapshot.

The retest used disposable overlays and `agent_update = false`. Its
daemon, VM and event logs are under `/home/vmlab/src/stc/o83/s/vmlab`
on helios. Allow up to 300 seconds for `vmlab up --wait` when repeating
the probe. Card #77 covers the rebuild from installation media.

### F3. Restore agent coverage for legacy Windows controls

Boot the listed sealed versions for `windows-8`, `windows-8.1`, both
Vista architectures, and the four Server 2008 editions without the R2
suffix. Run `vmlab up --wait`, `vmlab status -v`, and
`vmlab exec guest -- cmd /c ver`. The VM reports `ready=yes` with
`agent=-` and exec refuses because the template records no agent.
Rebuild the affected images with the definition's current agent setup.
See the boot trace referenced in each matrix row.

### F4. Diagnose agent readiness timeouts on legacy Windows

Pin `x86/windows-95@4.00.950.4`,
`x86/windows-95-osr25@4.03.1216.4`, or
`x86_64/windows-server-2016-core@14393.0.1`. Run
`vmlab up --wait` with the vmlab-hv backend for 150 seconds and inspect
`vmlab status -v`. The VM is running with `ready=no`.
See `evidence/windows-95/dos-boot.log`,
`evidence/windows-95-osr25/dos-boot.log`, and
`evidence/windows-server-2016-core/verify-boot.log`.
The first Windows 8 x86 probe also hit the 150-second bound.
Its 300-second retry passed all three checks. See
`evidence/windows-8-x86/{control,boot-retry}-boot.log`.

### F5. Investigate snapshot restore timeouts on agentless Windows

For 64-bit Windows 8 and both Vista architectures, boot the pinned
version in the matrix, run
`vmlab snapshot create round-trip --vm guest`, and then
`vmlab snapshot restore round-trip --vm guest`. Create exits 0 and
restore exceeds the 75-second command bound. See
`evidence/windows-8/control-boot.log` and
`evidence/{windows-vista,windows-vista-x86}/verify-boot.log`.

### F6. Investigate Server 2016 exec after agent readiness

Pin `x86_64/windows-server-2016@14393.0.0` and allow up to 300 seconds
for `vmlab up --wait`. It exceeded the original 150-second boot bound
and reached agent readiness on the retry. The first
`vmlab exec guest -- cmd /c ver` timed out. Snapshot create and restore
exited 0 and the second exec exited 137.
See `evidence/windows-server-2016/{verify,boot-retry}-boot.log`.
