# Fuzz coverage of the parsers of hostile input

Card #382, 2026-11-21. Card #11 (T8) added cargo-fuzz targets for qcow2,
hv-proto and agent-proto. This review lists every parser of input that a
guest, a template or disk image, a registry or a network peer controls, in
vmlab, vmlab-hv and the guest agents, marks which have a fuzz target, adds
three targets, and records what their first bounded runs found.

The list was made by reading the code at `next` (8cbcb25c). "Target" is the
cargo-fuzz target that reaches the parser: **yes**, **part** (the target
reaches it but leaves named paths out) or **no**. No parser in the repository
has a proptest, quickcheck or other randomised test.

## 1. Targets

| Code | Target | Crate | Added by | Reaches |
|---|---|---|---|---|
| T1 | `qcow2` | `fuzz/` | #11 | `Qcow2Image::open` read-only, `read_at`, `snapshots` |
| T2 | `hv_proto` | `fuzz/` | #11 | `vmlab_hv_proto::codec` client and server messages, with a round trip |
| T3 | `agent_proto` | `guest/agent-proto/fuzz/` | #11 | `FrameDecoder`, `HostMsg` and `AgentMsg` JSON, frame round trip |
| T4 | `parse` | `hv/crates/vmlab-hv-sail-ir/fuzz/` | earlier | the Sail IR text parser (build-time input, not in `just fuzz`) |
| T5 | `vnet_offload` | `fuzz/` | #382 | `vmlab_hv_proto::net`: `Header::from_metadata`, `validate`, `segments`, `flow_hash` |
| T6 | `net_frame` | `fuzz/` | #382 | the userspace fabric: header views, ICMP builders, `DhcpServer::handle`, `DnsServer::handle`, `RuleSet::eval` and `eval_return` |
| T7 | `oci_document` | `fuzz/` | #382 | `parse_manifest_or_index` and what a pull reads from it, `TemplateConfig::from_json`/`vars`/`into_meta`, `ImageConfig`, `parse_bearer_challenge`, both reference grammars |

`just fuzz <seconds>` runs T1, T2, T3, T5, T6 and T7. T6 and T7 reach private
modules of the main crate through `vmlab::fuzzing` (`src/fuzzing.rs`), which is
compiled under `--cfg fuzzing` only.

## 2. The list

### 2.1 vmlab-hv: input from the guest through a device

The split virtqueue itself is walked by the external `virtio-queue` crate,
MMIO instructions are decoded by KVM, and bzImage loading is the external
`linux-loader`. The list has the in-tree code only.

| Code | Parser | Entry | Input from | Target |
|---|---|---|---|---|
| P1 | virtio-fs FUSE server and passthrough backend | `virtio/fs/fuse/server/sync_io.rs:31` `handle_message`, `virtio/fs/passthrough.rs` | guest, every byte, acts on the host file system | no |
| P2 | virtio-blk descriptor chains, indirect tables, request header, discard ranges | `virtio/block.rs:297`, `:494`, `:556`, `:725` | guest | no |
| P3 | virtio-net TX, control queue | `virtio/net.rs:1157`, `:1204`, `:1564` | guest | part: T5 reaches the header decode and the offload completion, not the queue code |
| P4 | virtio-net RX from the switch socket, RX checksum parse | `virtio/net.rs:804`, `:1038`, `virtio/net/checksum.rs:117` | other guests, through the switch | no |
| P5 | vnet stream record: virtio-net header, GSO and checksum completion | `vmlab-hv-proto/src/net.rs:43`, `:86`, `:124`, `:136`, `:343` | guest (TX header), other guests (RX) | **yes, T5** |
| P6 | virtio-gpu 2D commands, backing lists | `virtio/gpu/mod.rs:804`, `:1088`, `:1159` | guest | no |
| P7 | virtio-gpu 3D command stream (handed to virglrenderer) | `virtio/gpu/renderer.rs:456`, `:1098` | guest | no |
| P8 | virtio-snd control and PCM messages | `virtio/sound/mod.rs:254`, `:472`, `:611`, `:751` | guest | no |
| P9 | virtio-console TX and control | `virtio/console.rs:562`, `:680`, `:746` | guest | no |
| P10 | virtio-pci transport, legacy transitional I/O | `virtio/pci.rs:711`, `:1029`, `virtio/transitional.rs` | guest MMIO and PIO | no |
| P11 | AHCI command headers, FIS, PRDT | `ahci/mod.rs:350`, `:562`, `:628` | guest | no |
| P12 | IDE task file, BMDMA PRD walk; ATA and ATAPI packets | `ide/mod.rs:328`, `:499`, `:897`, `ata/mod.rs:61`, `ata/atapi.rs:128` | guest | no |
| P13 | floppy controller commands | `floppy/mod.rs:312`, `:415`, `:796` | guest PIO | no |
| P14 | xHCI TRBs, input contexts, event ring segments | `usb/xhci.rs:388`, `:616`, `:716`, `:847`, `usb/ring.rs:71` | guest memory | no |
| P15 | UHCI frame list, QH and TD chains | `usb/uhci.rs:376`, `:396`, `:428` | guest memory | no |
| P16 | USB setup packets: keyboard, tablet, host passthrough | `usb/keyboard.rs:149`, `usb/tablet.rs:176`, `usb/host/mod.rs:350` | guest | no |
| P17 | HDA CORB verbs, buffer descriptor lists | `hda/mod.rs:297`, `:501`, `hda/codec.rs:31` | guest | no |
| P18 | e1000 TX descriptors and TSO, RX | `nic/e1000/mod.rs:684`, `:734`, `:1205`, `:944` | guest (TX), other guests (RX) | no |
| P19 | PCnet init block, descriptors | `nic/pcnet/mod.rs:344`, `:501`, `:565`, `:687` | guest, other guests | no |
| P20 | TPM TIS FIFO, swtpm response length | `tpm/mod.rs:135`, `:317`, `tpm/backend.rs:112` | guest, the swtpm socket | no |
| P21 | pflash CFI commands | `pflash/mod.rs:78`, `:229` | guest MMIO | no |
| P22 | Cirrus blitter, standard VGA and Bochs registers | `display/cirrus/mod.rs:330`, `display/cirrus/blit.rs:109`, `display/mod.rs:350` | guest | no |
| P23 | fw_cfg DMA descriptor, the SMM negotiation wrapper | `vmlab-hv-firmware/src/fw_cfg.rs:221`, `vmlab-hv-vmm/src/smm/fwcfg.rs:167` | guest | no |
| P24 | shared-memory display header (vdisplay) | `vdisplay.rs:215`, `:287` | guest | no |
| P25 | Apple SMC, UART registers, vfio config writes | `applesmc.rs:83`, `serial.rs:336`, `vfio.rs:303` | guest | no |

Paths without a crate name are under `hv/crates/vmlab-hv-devices/src/`.

### 2.2 vmlab-hv: input from a disk image, a snapshot or a socket peer

| Code | Parser | Entry | Input from | Target |
|---|---|---|---|---|
| P26 | qcow2 header, L1/L2, refcounts, snapshot table | `vmlab-disk/src/qcow2/mod.rs:203`, `qcow2/snapshot.rs:136` | template, disk image | part, T1: not the read-write open with `recovery.rs:12`, writes, refcount allocation, snapshot apply and delete, `read_vmstate`, backing chains |
| P27 | raw and format probe, `info`, `convert`, `commit` | `vmlab-disk/src/raw.rs:17`, `qcow2/mod.rs:1497`, `ops.rs:158` | template, disk image | no |
| P28 | vmstate: header, sections, RAM regions, FPU, XSAVE, MSRs, LAPIC, each device's `restore_state` | `vmlab-hv-vmm/src/snapshot/format.rs:178`, `:214`, `:318`, `snapshot.rs:920` to `:1017` | the vmstate area of a qcow2, so a template | no |
| P29 | control protocol, machine description | `vmlab-hv-proto/src/codec.rs:51`, `fd.rs:103`, `machine.rs` | a control socket client | yes, T2 (the fd-passing reader shares the decoder) |
| P30 | native display protocol | `vmlab-hv-display/src/protocol.rs:287`, `transport.rs:72`, `core.rs:454` | a display socket client | no |
| P31 | RFB client messages | `vmlab-hv-display/src/rfb.rs:80`, `:121` | a display socket client | no |
| P32 | gdb remote protocol (external `gdbstub`) | `vmlab-hv-proto/src/gdb.rs:60` | a TCP peer, when debugging is on | no |
| P33 | Sail IR | `vmlab-hv-sail-ir/src/lib.rs:712` | build-time model | yes, T4 |

### 2.3 vmlab: frames a guest sends into the userspace fabric

| Code | Parser | Entry | Target |
|---|---|---|---|
| P34 | Ethernet, ARP, IPv4, UDP, TCP, ICMP views and ICMP reply builders | `src/net/frame.rs:109` to `:681` | **yes, T6** |
| P35 | DHCP server, BOOTP options | `src/net/dhcp.rs:148`, `:465` | **yes, T6** |
| P36 | DNS server, query names | `src/net/dns.rs:423`, `:539` | **yes, T6** |
| P37 | L3 rules: block, redirect, return path, block replies | `src/net/rules.rs:275`, `:327`, `:457`, `:520` | **yes, T6** |
| P38 | vnet stream framing on a switch port | `src/net/framing.rs:72`, `:126` | part, T5: the record body, not the async length-prefix reader |
| P39 | NAT engine: guest TCP state machine, MSS option, UDP flows, ICMP | `src/net/nat/mod.rs:194`, `nat/vtcp.rs:103`, `:128`, `:488`, `:613` | no |
| P40 | router between segments, TTL, ICMP errors | `src/net/router.rs:198`, `:297`, `:324` | no |
| P41 | switch ingress, MAC learning; gateway ARP and IPv4 dispatch | `src/net/switch.rs:440`, `src/net/gateway.rs:374` | no |
| P42 | eBPF fast path classifier (shared logic crate) | `ebpf/fastpath-logic/src/lib.rs:80`, `:192`, `:240` | no |

### 2.4 vmlab: what a guest sends over the agent channel and the display

| Code | Parser | Entry | Target |
|---|---|---|---|
| P43 | agent frames and control messages | `guest/agent-proto` `FrameDecoder`, `src/labd/vm_agent.rs:2358` | yes, T3 |
| P44 | records inside agent channels: file RPC responses, watch records, app records | `guest/agent-proto/src/fileops.rs:361`, `watch.rs:153`, `app.rs:130` | no |
| P45 | guest-reported paths of the workspace syncer | `src/labd/workspace/ignore.rs:441`, `scan.rs:388`, `apply.rs:73` | no |
| P46 | boot identity, playbook reports (guest `exec` output) | `src/labd/boot_id.rs:99`, `:110`, `:127`, `src/labd/playbook.rs:319` | no |
| P47 | container init events | `src/labd/container_ctl.rs:82`, `:177`, `guest/cinit-proto` | no |
| P48 | X11 connection setup of a shared app | `src/appshare/x11.rs:322`, `:476` | no |
| P49 | RDP credentials reply | `src/appshare/rdp.rs:40` | no |
| P50 | RFB server messages (QEMU relays guest pixels, cursor, clipboard) | `src/vnc.rs:89`, `:224`, `:267`, `:425` | no |
| P51 | recorder core: rectangles and cursor shapes | `src/record/core.rs:251`, `:288`, `:295` | no |
| P52 | native frames as vmlab consumes them | `src/hvdisplay.rs:77`, `:326` | no (the message decode is P30) |
| P53 | QMP replies and events | `src/qmp/client.rs:474`, `:505` | no |

### 2.5 vmlab: what a registry, a template or an image supplies

| Code | Parser | Entry | Target |
|---|---|---|---|
| P54 | manifest and index, variant annotations | `src/oci/manifest.rs:342`, `:373`, `:487`, `:519` | **yes, T7** |
| P55 | template config blob, packed VARS | `src/oci/config_blob.rs:215`, `:276`, `:295` | **yes, T7** |
| P56 | container image config | `src/oci/image/model.rs:31` | **yes, T7** |
| P57 | `Www-Authenticate` challenge | `src/oci/auth.rs:239` | **yes, T7** |
| P58 | references (template and image) | `src/oci/reference.rs:29`, `src/oci/image/reference.rs:58` | **yes, T7** (local input mostly) |
| P59 | token, tag list and catalog JSON, `Link` header | `src/oci/client.rs:1530`, `:1747`, `:1799`, `:1816` | no |
| P60 | catalog providers: Docker Hub and GitHub JSON, the GitHub packages page | `src/oci/catalog.rs:145`, `:261`, `:319`, `:381` | no |
| P61 | layer flatten: tar in gzip or zstd, whiteouts, path normalising | `src/oci/image/flatten.rs:67`, `:110`, `:267` | no |
| P62 | chunk assembly (zstd, capped window) | `src/oci/chunking.rs:167` | no |
| P63 | template archive unpack (tar in zstd) | `src/template/store.rs:909`, through `TemplateStore::import` `:656` | no |
| P64 | template metadata WCL, also the round trip of registry strings through `to_wcl` | `src/template/meta.rs:338`, `:424` | no |
| P65 | backing file of a qcow2 header, `qemu-img info` JSON | `src/template/store.rs:837`, `src/template/qimg.rs:358` | no |
| P66 | version pins against registry tags | `src/template/store.rs:1040`, `:1088`, `:1118` | no |
| P67 | image `/etc/passwd` and `/etc/group` in the guest init | `guest/cinit/src/users.rs:25`, `:45`, `:62` | no |

### 2.6 Guest agents: input from the host channel

The host is the trusted side here. The parsers are listed because two of them
are in C and HolyC.

| Code | Parser | Entry | Target |
|---|---|---|---|
| P68 | Rust agent: frames, `HostMsg`, file RPC requests, watch requests, clipboard helper | `guest/agent/src/main.rs:211`, `mux.rs:510`, `fileops.rs`, `watch/mod.rs:122`, `clipboard.rs:194` | part, T3: frames and `HostMsg`, not the records |
| P69 | cinit: control commands, kernel command line, container spec | `guest/cinit/src/ctl.rs:230`, `cmdline.rs:22` | no |
| P70 | legacy C agent: wire decoder and JSON parser | `guest/agent-legacy/src/wire.c:40`, `:73`, `json.c:149`, `:240`, `:303` | no (conformance tests only) |
| P71 | TempleOS agent: frame reader and JSON parser in HolyC | `guest/agent-templeos/VmlabAgt.HC:73`, `:222` | no, and no native way to fuzz it |

### 2.7 Local input, lower hostility

Listed so the list is complete. None has a target.

| Code | Parser | Entry | Input from |
|---|---|---|---|
| P72 | daemon wire protocol | `src/proto/server.rs:134`, `:207`, `src/proto/vocab.rs:141` | a local socket client, same user |
| P73 | lab file, template file, host config, profiles (WCL) | `src/config/mod.rs:137`, `:223`, `src/config/host.rs:214`, `src/profiles/mod.rs:176` | the user's own files; a cloned repository's lab file is the hostile case |
| P74 | wscript lexer, parser and compiler | `vendor/wscript-compiler` `lexer.rs:21`, `parser.rs:38`, `lib.rs:101` | the user's scripts; a template's first-boot script runs only when trusted |
| P75 | USB sysfs scan, selectors | `src/usb/mod.rs:163`, `:195`, `:265` | a physical USB device's strings |
| P76 | fast-path helper requests | `src/net/fastpath/helper/proto.rs:198` | the unprivileged daemon, to the privileged helper |
| P77 | `.git` files, `worktree.json`, ports record | `src/freeze/mod.rs:74`, `src/instance.rs:100`, `src/freeze/ports.rs:246` | the checkout |
| P78 | sync ledger, ignore files | `src/labd/workspace/ledger.rs:255`, `ignore.rs:157` | host files; an ignore file can come from the guest by sync |
| P79 | Xauthority, smbd log | `src/appshare/x11.rs:95`, `src/smb/server.rs:308` | host files |

## 3. The choice of three

Ranked by how hostile the input is, the parsers with no target were, at the
start of this card:

1. P1, the FUSE server with its passthrough backend.
2. P2, P6 and P11 to P15: the device models that walk guest memory.
3. P5, P34 to P37: what a guest NIC sends, on both sides of the switch socket.
4. P44, the records inside the agent channels.
5. P54 to P57, P61: what a registry serves.
6. P28, the vmstate a template can carry.

The three targets of this card are T5 (P5), T6 (P34 to P37) and T7 (P54 to
P58). The reasons:

- T5 and T6 take the bytes of a guest's NIC with no setup, in the process
  that runs every lab (the lab daemon) and in vmlab-hv, and their code is
  hand-written offset arithmetic. They are the most hostile input that a
  target can reach today with a byte slice.
- T7 covers the third adversary the card names, a registry, which had no
  target of any kind.
- P1 and the device models rank higher and got no target here. A target for
  them is not a harness of a few lines: `vmlab-hv-devices` links virglrenderer
  (the `hv-virgl-build` step), the slice-backed `Reader` and `Writer` of the
  FUSE transport are `#[cfg(test)] pub(crate)`, and a fuzzed passthrough file
  system must run in a sandbox so that a path escape cannot touch the host
  that runs the fuzzer. That is its own card: N1 and N2 below.

### Next targets

| Code | Target to add | Why | Cost |
|---|---|---|---|
| N1 | FUSE server over a temporary directory, in a bwrap sandbox (P1) | most hostile parser without a target | a `hv/fuzz` crate that links virgl, `Reader::from_slices` behind `cfg(fuzzing)`, a sandboxed runner |
| N2 | device models driven by a script of register writes over a guest-memory image: virtio-blk (P2), AHCI and IDE (P11, P12), xHCI and UHCI (P14, P15), virtio-gpu 2D (P6), e1000 RX and TSO (P18) | guest memory walkers, several with `unsafe` | the same `hv/fuzz` crate, one harness shape for all |
| N3 | agent channel records (P44) | guest input on every file transfer and every sync | small: three `RecordDecoder`s in the existing `guest/agent-proto/fuzz` crate |
| N4 | vmstate decode and each device's `restore_state` (P28) | a template can carry a vmstate | small for `snapshot::format`, which takes a byte slice |
| N5 | layer flatten and template unpack (P61, P63) | registry tar with hostile paths | medium: temporary files, no `sqfstar` needed for the survey pass |
| N6 | recorder core and RFB client (P50, P51) | guest-influenced through QEMU | recorder small, RFB client needs a reader that is not a `UnixStream` |
| N7 | X11 setup (P48), NAT TCP state machine (P39), legacy C agent (P70) | see the list | medium each |

## 4. The runs

RUNS-PLACEHOLDER

## 5. Findings

FINDINGS-PLACEHOLDER
