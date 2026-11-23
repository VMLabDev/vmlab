# Review: the lab file, host config and template definitions (parsing and validation)

Card #532. A bug-hunting review of how vmlab reads and validates `vmlab.wcl`,
`~/.config/vmlab/config.wcl`, guest profiles and template metadata, and of
where the secrets in them go. It lists faults and makes no fix. Reviewed at
`next` commit `665b6a4b`. All paths are from the repository root, and all line
numbers are from that commit.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped.

Severity words: **data loss**, **crash** (of a host process), **security**
(against a hostile guest, a hostile registry or another local user),
**wrong result**, **cosmetic**.

## Who is trusted

The findings are ranked against these three facts, which the code states:

- A lab file is as trusted as a build script. `qemu_args` appends any QEMU
  flag (`src/qemu/cmdline.rs:1019`), and `provision` runs wscript on the
  host. So a path in a lab file that leaves the lab root is not a privilege
  gain by itself. Such gaps are ranked **wrong result**.
- A template pulled from a registry is not trusted. Its host scripts run
  only when its repository is in `host.trusted_template_repositories`
  (`src/template/trust.rs:30-45`). Everything else in its metadata is
  accepted from any registry. A gap here is ranked **security**.
- The host config is trusted, and it holds two secrets: `psk` and
  `apple_smc_osk`.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | security | A pulled template's `display` string becomes a QEMU `-device` argument unchecked, from any registry (fixed, card #540) |
| F2 | security | The trusted-repository check is a text prefix on a repository that may hold `..` segments (fixed, card #548) |
| F3 | security | The Apple SMC OSK is in QEMU's argv, which every local user can read. **Fixed by card #544.** |
| F4 | data loss | Two `disk` blocks with one name give a VM two drives on one image file (fixed, card #558) |
| F5 | security | A validation error prints login passwords and `secret = true` playbook values from the lab file (fixed, card #545) |
| F6 | security | `HostConfig`'s `Debug` output holds the trunk PSK in the clear (fixed, card #545) |
| F7 | wrong result | `host.subnet_pool` is parsed and never read |
| F8 | crash, wrong result | A segment subnet of any prefix length is accepted, and `gateway_ip` overflows on the last address (fixed, card #564) |
| F9 | wrong result | Paths in QEMU option values are not comma-escaped at most sites |
| F10 | wrong result | Machine and segment names that differ only in case pass, and share one DNS name |
| F11 | wrong result | A `dns_upstream` that does not parse falls back to the host resolver without a word |
| F12 | wrong result | An explicit share `name` is unchecked and is written into `smb.conf` as a section header |
| F13 | wrong result | `@dev` arguments are not validated: a relative `workspace_guest`, a missing `workspace` |
| F14 | wrong result | Port 0 as a forward or redirect target, and an ICMP block rule with a port, pass validation |
| F15 | wrong result | `oci_chunk_size` is bounded only when a push starts |
| F16 | cosmetic | The lab digest blanks any text that looks like a span, also inside a declared string |

### F1. A pulled template's `display` becomes a QEMU `-device` (security)

- Where: `src/qemu/resolve.rs:475-494` (`display_device_name`, the
  `other => other.to_string()` arm at `:493`), `:633-638` (the VM, then the
  template, then the profile supply the string), `src/qemu/cmdline.rs:903-905`
  and `:947-949` (`arg(&mut a, "device", d.clone())`),
  `src/oci/config_blob.rs:239` (`display: self.display`, no check),
  `src/template/meta.rs:634` and `:786`, `src/config/hypervisor.rs:117-125`
  and `:335-346`.
- Why it is a fault: the display name is a free string. Five names are
  mapped, and any other string is passed to QEMU as the whole value of a
  `-device` argument. For a VM that declares no `display`, the string comes
  from the template's metadata, and a pull copies it from the registry's
  config blob without a check. So a template from any registry chooses one
  QEMU device and all its properties on the host that clones it. QEMU's
  generic loader device (`loader,file=<host path>,addr=<address>,force-raw=on`)
  copies a host file into guest memory, so the template's own guest image can
  read any file the vmlab user can read and send it out over a NAT segment.
  The host's trust list does not apply: it gates `first_boot_script` only.
  vmlab-hv refuses an unknown display device, but that does not protect the
  default case. With no explicit hypervisor on the VM or in the host config,
  a refusal makes `HypervisorDefaults::select` choose QEMU
  (`src/config/hypervisor.rs:117-125`), so the hostile string itself selects
  the hypervisor that accepts it. A variant's `display` is the same path
  (`src/template/meta.rs:328`). A lab file's own `display` reaches the same
  code. That is no gain for a lab file (it has `qemu_args`).
- Proof:
  1. Unit test `a_template_display_names_a_known_device_or_is_refused` in
     `src/qemu/resolve.rs`: resolve a VM that declares no `display` against a
     `TemplateMeta` with
     `display = Some("loader,file=/etc/hostname,addr=0x1000000,force-raw=on")`.
     It must be an error. Today `display_device` is that string.
  2. Unit test in `src/oci/config_blob.rs`:
     `TemplateConfig::from_json` of a blob with that `display`, then
     `into_meta`, must be an error. Today it returns the metadata.
  3. By hand: push such a template to a local registry that is not in the
     trust list, pull it, declare `vm "a" { template = "<ref>" arch = "x86_64" }`,
     run `vmlab up`, and read the QEMU command line in the machine's log. It
     holds `-device loader,file=...`.
- I did not check which QEMU devices other than `loader` give a guest host
  data without a second argument.
- Fixed in card #540. A template's `display`, the base's or a variant's, is
  one of a closed list of names (`TEMPLATE_DISPLAYS` in
  `src/qemu/resolve.rs`): vmlab's five names and QEMU's display devices by
  name, with no properties. `TemplateConfig::into_meta` refuses any other
  string at the pull, `resolve_vm` refuses it for metadata that is in the
  store already or came from an archive, and a layered build refuses it in
  its source. A `template {}` block that declares another string is a
  validation error, because the build would record it. The lab file's own
  `display` is unchanged. Both proof tests failed before the fix (the
  resolver returned the `loader` string, `into_meta` returned the metadata)
  and pass after it. Proof 3 was not run by hand.

### F2. The trust list is a text prefix on an unnormalised repository (security)

- Where: `src/template/trust.rs:40-45` (`repository_trusted`),
  `src/oci/reference.rs:30-76` (`Reference::parse`),
  `src/oci/client.rs:693-698` and `:961-965` (the two calls),
  `src/oci/client.rs:1672-1674` and `:2061` (the request URL is built with
  `format!`), `src/config/model.rs:1415-1421` (a lab's registry reference is
  kept as written).
- Why it is a fault: a namespace entry such as the default
  `ghcr.io/vmlabdev/vmlab-templates/` trusts every reference whose
  `host/repository` text starts with it. `Reference::parse` accepts any
  characters in the repository, `..` segments included. The same text goes
  into the request path as `/v2/{repository}/manifests/{tag}`, and `reqwest`
  parses the URL with the `url` crate, which removes dot segments as the
  WHATWG rules require. So the reference
  `ghcr.io/vmlabdev/vmlab-templates/../../other/tpl:1` passes the trust check
  and the request goes to `/v2/other/tpl/manifests/1`. The repository that
  answers is not the one the host trusted, and its `first_boot_script` runs
  on the host. The earlier registry review recorded the lax grammar as a note
  (`docs/reviews/templates-registry.md` N10). It did not connect it to the
  trust check.
- Proof:
  1. Unit test `a_dot_segment_does_not_inherit_a_namespace_trust` in
     `src/template/trust.rs`:
     `repository_trusted(&["ghcr.io/vmlabdev/vmlab-templates/".into()], "ghcr.io", "vmlabdev/vmlab-templates/../../other/tpl")`
     must be `false`. Today it is `true`.
  2. Unit test in `src/oci/reference.rs`:
     `Reference::parse("ghcr.io/a/../b:1")` must be an error. Today it is `Ok`.
  3. Test against the in-process registry the OCI client tests use: serve
     `other/tpl` with a `first_boot_script`, trust `<host>/trusted/ns/`, and
     pull `<host>/trusted/ns/../../other/tpl:1`. The pull must refuse the
     script. Today it installs it.
- I did not check what a token service does with the unnormalised name in
  the `scope` parameter. ghcr.io may refuse the token, which would stop the
  pull there. A registry that serves anonymous pulls without a token is not
  protected by that.
- Fixed in card #548. A repository is plain when every `/`-separated
  segment is non-empty, holds only ASCII letters, digits, `.`, `_` and `-`,
  and is not `.` or `..` (`repository_is_plain` in `src/oci/reference.rs`).
  That also excludes `%2e%2e`, `\`, `?` and `#`, which the URL parser reads
  as a dot segment, a separator and the end of the path. `Reference::parse`
  refuses a repository that is not plain, so no request is built from one,
  and `repository_trusted` returns `false` for one, so the trust check holds
  for a `Reference` built without the parser too. Host and tag are checked
  as before. All three proof tests failed before the fix (the trust check
  returned `true`, the parse was `Ok`, the pull installed the script) and
  pass after it. Proof 3 is
  `a_dot_segment_reference_does_not_run_another_repositorys_script` in
  `src/oci/client.rs`: it puts the fake registry behind a transport that
  removes dot segments with the `url` crate, as the real transport does.

### F3. The Apple SMC OSK is in QEMU's argv (security)

**Fixed by card #544.** The argv holds `-readconfig <run dir>/applesmc.cfg`
in place of the `-device` value. The QEMU adapter writes that file with mode
0600 before the spawn and removes it when QMP answers or the start fails.
QEMU's output is still filtered for the key. A key that holds a double quote
is refused at launch, because the file format has no escape for one.

- Where: `src/qemu/cmdline.rs:541-548`
  (`format!("isa-applesmc,osk={}", …)`), `src/config/host.rs:26-34`.
- Why it is a fault: the host config treats the OSK as a secret. Its `Debug`
  output is redacted, the host config's diagnostics blank the source, and
  `src/qemu/process.rs:19-34` redacts it in the log header and in QEMU's
  output. But the key is one element of the QEMU command line, and on Linux
  `/proc/<pid>/cmdline` is readable by every local user unless the host
  mounts `/proc` with `hidepid`. So every local account can read the key for
  as long as a macOS VM runs.
- Proof:
  1. Unit test `the_osk_is_not_an_argv_element` in `src/qemu/cmdline.rs`:
     build the argv of a VM with `apple_smc = true` and a key, and assert that
     no element contains the key. Today the `-device` value holds it.
  2. By hand: start a VM with `apple_smc = true`, and as another user run
     `tr '\0' ' ' < /proc/<qemu pid>/cmdline`.

### F4. Two `disk` blocks with one name share one image (data loss)

- Where: `src/config/validate.rs:1920-1941` (`check_disk_block`, no name
  check), `src/labd/vm.rs:828-832` (an existing path is skipped) and
  `:944-949` (each block becomes a drive), `src/qemu/cmdline.rs:769-771`.
- Why it is a fault: a disk's file is `disk-<name>.qcow2` in the machine's
  directory. Validation does not compare the names of a VM's `disk` blocks.
  For `disk "data" { size = 10GiB }` written twice, the first block creates
  the file, the second finds it and continues, and the launch gets two
  writable drives on the same file. QEMU's image lock refuses the second, so
  that VM does not start and the error names a lock. vmlab-hv is the default
  for a new x86_64 VM, and `vmlab-disk` takes no lock on an image (a search of
  `hv/crates/vmlab-disk/src` for `flock`, `F_OFD_SETLK`, `LOCK_EX` and
  `try_lock` finds nothing). There the guest gets two block devices over one
  qcow2, each with its own metadata cache, and writes through both corrupt
  the image. The name is also unchecked for content: `/` makes the create
  fail with a path error, and `,` meets F9.
- Proof:
  1. Unit test `two_disks_with_one_name_are_refused` in
     `src/config/validate.rs`: a VM with two `disk "data"` blocks must give an
     issue that names the duplicate. Today `validate` returns none.
  2. Lifecycle test in `src/labd/lifecycle_tests.rs` with the fake
     hypervisor: the `Launch` of such a VM must not hold one path twice in
     `extra_disks`. Today it does.
- I did not read all of vmlab-hv's machine description checks. A search of
  `hv/crates/vmlab-hv-proto/src/machine.rs` for a duplicate-path check found
  none.
- Fixed in card #558. `validate` refuses a second `disk` block with the name
  of an earlier one in the same `vm` or `template` (`check_disk_names` in
  `src/config/validate.rs`), and `VmInstance::ensure_disks` refuses the same
  before it creates or launches anything, so a configuration that skipped
  validation still never reaches a launch with one image as two drives. Both
  proof tests fail before the fix and pass after. The content of a disk name
  (`/`, `,`) is not part of this fix.

### F5. A validation error prints passwords and secret values (security)

- Where: `src/config/mod.rs:144-148` and `:236-240` (`ConfigErrors` carries
  the whole source), `src/cli/validate.rs:373-374` and
  `src/cli/lab_move.rs:221` (rendered with `miette::Report`),
  `src/config/validate.rs:251-387` (most VM issues carry `vm.span`, the whole
  block), `:1546-1556` (`var.span`), `:1423-1471` (`login.span`).
- Why it is a fault: a miette report prints the source lines its label
  covers. Many issues are labelled with the span of a whole `vm` or
  `container` block, so an unrelated mistake (an unknown profile, a bad
  `depends_on`) prints every line of the block, with each
  `login { password = … }` and each `var { value = … secret = true }` in it.
  An issue about a login or a variable prints that block itself. The schema
  promises that a `secret` value is replaced "in every line, event, error and
  listing it emits for the run" (`src/config/schema.wcl:564`). The run is
  covered by `Redactor`. Validation, which runs first and in CI, is not. The
  host config already avoids this for its own secrets
  (`src/config/host.rs:360-365`).
- Proof: unit test `a_diagnostic_does_not_show_a_secret_value` in
  `src/cli/validate.rs`: a lab whose VM names an unknown profile and holds
  `login "dev" { user = "dev" password = "hunter2-login" }` and a playbook
  with `var "token" { value = "hunter2-var" secret = true }`. Format the error
  as the CLI does (`format!("{:?}", miette::Report::new(err))`) and assert
  that neither string is in it. Today both are.
- Fixed in card #545. The source a lab or template file's diagnostic renders
  from is no longer the file's text. `diagnostic_source` in
  `src/config/mod.rs` stars out the value of every `password` field and of
  every `value` beside a `secret` that is not plainly `false`, found by field
  span in the parsed document. Byte offsets and line breaks are kept, so each
  label points where it did. A file that does not parse is blanked whole, as
  the host config is. `load_lab_source`, `load_template_source` and
  `vmlab validate`'s own error use it. Not covered: a secret that the field
  refers to and does not hold (a value bound elsewhere in the file), and the diagnostics of
  profile files and template metadata (`IssueError::diagnostic` in
  `src/config/block.rs`), which still render from the file's text.

### F6. `HostConfig`'s `Debug` output holds the PSK (security)

- Where: `src/config/host.rs:50-58` (`#[derive(Debug)]`, `psk: Option<String>`).
- Why it is a fault: the OSK has a wrapper type with a redacting `Debug` and
  a test for it (`:26-34`, `:468-499`). The PSK, which authenticates
  cross-host trunks, is a plain `String` in the same struct. A search found
  no site that prints a `HostConfig` with `{:?}` today, so nothing leaks now.
  The first `tracing::debug!(?host_cfg)` or `.context(format!("{cfg:?}"))`
  will write the key into a log.
- Proof: extend `apple_smc_osk_is_optional_validated_and_hidden`
  (`src/config/host.rs:468`), or add `the_psk_is_hidden_from_debug`:
  parse `host { psk = "sekrit-psk" }` and assert
  `!format!("{host:?}").contains("sekrit-psk")`. Today the assertion fails.
- Fixed in card #545. `HostConfig::psk` is a `TrunkPsk`, a wrapper whose
  `Debug` prints `[REDACTED]`, like `AppleSmcOsk`. The supervisor reads the
  key with `expose()` where it starts the trunk listener and the global
  segments.

### F7. `host.subnet_pool` is never read (wrong result)

- Where: `src/config/host.rs:54` and `:237-239` (parsed),
  `src/labd/network.rs:27` and `:191` (`DEFAULT_POOL`, a constant),
  `src/supervisor/global.rs:577` (`GLOBAL_POOL`, a constant),
  `src/config/host_schema.wcl:15-16`.
- Why it is a fault: the host schema documents `subnet_pool` as the
  auto-allocation pool. A search of `src` for `subnet_pool` outside
  `src/config/host.rs` finds nothing. `LabNetwork::build` takes no host
  config and carves /24s out of the constant `10.213.0.0/16`. A user who sets
  the pool because `10.213.0.0/16` is in use on their network gets no error
  and no change.
- Proof:
  1. Test `an_auto_subnet_comes_from_the_hosts_pool` in
     `src/labd/network.rs`: a lab with one segment that declares no subnet,
     built under a host config with `subnet_pool = "10.99.0.0/16"`, must get a
     subnet inside `10.99.0.0/16`. The test cannot be written against today's
     signature, which is the fault.
  2. By hand: set `subnet_pool = "10.99.0.0/16"`, run `vmlab up` on such a
     lab, and read the segment's subnet in `vmlab status`. It is `10.213.x.0/24`.

### F8. Any subnet prefix is accepted, and `gateway_ip` overflows (crash, wrong result, fixed in #564)

- Where: `src/config/extract.rs:133` (`subnet` is any CIDR),
  `src/config/validate.rs:112-134` (only the overlap check),
  `:1773-1776` (`gateway_ip`: `base + 1` on a `u32`),
  `src/labd/network.rs:213` (called for every segment).
- Why it is a fault: no check bounds a declared subnet's prefix length. A
  `/32` or `/31` has no address for the gateway and none for a guest:
  `gateway_ip` returns an address outside a `/32` and the broadcast address of
  a `/31`. `0.0.0.0/0` passes and then excludes every /24 of the automatic
  pool, so the next segment without a subnet fails with "auto-subnet pool
  exhausted". For `subnet = "255.255.255.255/32"` the `+ 1` overflows. That is
  a panic where overflow checks are on (debug and test builds). The release
  profile does not set them (`Cargo.toml` has no `overflow-checks`), so there
  the gateway becomes `0.0.0.0`. The panic is reached in `validate` through
  the message at `:1750-1752` for a NIC with `ip = "255.255.255.255"`, and in
  `LabNetwork::build` for any lab with that segment.
- Proof:
  1. Unit test `a_subnet_too_small_for_a_gateway_and_a_guest_is_refused` in
     `src/config/validate.rs`: segments with `/32`, `/31` and `/0` subnets
     must each give an issue. Today none does.
  2. Unit test `gateway_ip_does_not_overflow`:
     `gateway_ip("255.255.255.255/32".parse().unwrap())` must return without
     a panic. Today it panics under `cargo test`.

### F9. Paths in QEMU option values are not comma-escaped (wrong result)

- Where: `src/qemu/cmdline.rs:744` (disk, `file.filename=`), `:790`
  (CD-ROM), `:819` (floppy), `:640` and `:649` (pflash), `:570`, `:576`,
  `:699`, `:881`, `:1005` (sockets), `:610` and `:614` (serial log). The
  escape exists at `:547`, `:588`, `:677` and `:859`.
- Why it is a fault: QEMU splits an option value at each `,`, and a literal
  comma is written `,,`. Four sites do that and at least eleven do not. The
  paths come from the lab root (any directory the user chose), from `cdrom`
  and `floppy` in the lab file, and from a `disk` block's name. A lab kept in
  `~/labs/a,b/` cannot start a VM, and the error is QEMU's about an unknown
  option. A `cdrom = "isos/win,media=disk.iso"` changes the drive's options.
- Proof: unit test `a_comma_in_a_path_is_doubled_in_every_option` in
  `src/qemu/cmdline.rs`, beside the splash test at `:1148`: build the argv
  with `primary_disk`, a CD-ROM, the floppy, the firmware files and the
  sockets under `/lab/with,comma/`, and assert that every element that holds
  the directory holds `with,,comma`. Today the splash and three others do.

### F10. Names that differ only in case share one DNS name (wrong result)

- Where: `src/config/validate.rs:247-262` and `:391-400` (a case-sensitive
  `HashSet`), `:111-119` (segments), `:2090-2105` (`check_dns_label` accepts
  upper case), `src/net/dns.rs:110` (`normalize` lowers every name).
- Why it is a fault: `vm "Web"` and `container "web"` are two machines to
  validation, with two directories, and one name to the segment's DNS, which
  lowers what it registers and what it looks up. The second registration
  replaces the first, so `web.<lab>.<suffix>` resolves to one of them and the
  other has no name. The host config already treats GPU names as
  case-insensitive (`src/config/host.rs:338-348`).
- Proof:
  1. Unit test `names_that_differ_only_in_case_are_duplicates` in
     `src/config/validate.rs`: `vm "Web"` and `container "web"` must give the
     "duplicate name" issue. Today there is none.
  2. Unit test in `src/net/dns.rs`: `register("Web", a)` then
     `register("web", b)` (`:160`), and resolve `web.<zone>`. It gives `b`,
     and no name gives `a`.

### F11. A malformed `dns_upstream` falls back without a word (wrong result)

- Where: `src/config/host.rs:243` (read as any string), `:389-404`
  (`upstream_resolver`: `parse_upstream` returning `None` leads to
  `host_resolver()`).
- Why it is a fault: `dns_upstream = "dns.corp.example"` or `"10.0.0.1:"`
  parses as a host config. `upstream_resolver` then cannot parse it and uses
  the resolver from `/etc/resolv.conf`, as if the field were not set. A user
  who set the field to keep lab queries away from the host's resolver gets
  the opposite, and no message. Every other host field with a grammar is
  checked at parse (`subnet_pool`, `trunk_port`, `fastpath`).
- Proof: unit test `a_malformed_dns_upstream_is_refused` in
  `src/config/host.rs`: `host_err("host { dns_upstream = \"dns.corp\" }\n")`
  must name `dns_upstream`. Today `HostConfig::parse` returns `Ok`.

### F12. An explicit share `name` is unchecked (wrong result)

- Where: `src/config/extract.rs:328-331` (taken as written),
  `src/config/validate.rs:316-324` (only the empty check),
  `src/smb/config.rs:254-255` (`[{name}]`, `path = {host}`).
- Why it is a fault: a derived share name is reduced to `[a-z0-9_]`
  (`derive_share_name`). An explicit one is written into `smb.conf` as a
  section header without a check. `name = "global"` adds a second `[global]`
  section whose `path` and `valid users` lines change the server's global
  settings. `homes`, `printers` and `IPC$` are other names Samba reserves. A
  `]` or a line break in the name, or a line break in the `host` path, starts
  a new line of configuration. The lab file is trusted, so this is a wrong
  result and not a privilege gain: the share, or every share of the lab,
  fails or behaves in a way nothing explains.
- Proof:
  1. Unit test `an_explicit_share_name_is_a_plain_share_name` in
     `src/config/validate.rs`: shares named `global`, `a]b` and `a\nb` must
     each give an issue. Today none does.
  2. Unit test in `src/smb/config.rs`: render a config with a share named
     `global` and assert the text holds one `[global]` line. Today it holds two.

### F13. `@dev` arguments are not validated (wrong result)

- Where: `src/config/validate.rs:2072-2088` (`check_dev_defaults`, the only
  `@dev` rule), `src/config/extract.rs:717-725`, `src/labd/lab.rs:3031-3044`
  and `:3054`.
- Why it is a fault: a share, a volume, a media folder and a disk folder
  must exist to pass `vmlab validate`. An `@dev` `workspace` need not: the
  daemon finds the missing directory at `up` and emits
  `workspace.unavailable`, and the machine runs with no syncer.
  `workspace_guest` is not checked to be an absolute path for the guest's
  family, although the same file has the check for a playbook's `guest_dir`
  (`guest_path_problem`, `:1563`). A relative value is handed to the agent as
  the guest root. A value of `/` or `C:\` makes the guest's whole file system
  the workspace.
- Proof: unit test `dev_arguments_are_checked_like_a_share` in
  `src/config/validate.rs`: `@dev(workspace = "./nope", workspace_guest = "src")`
  on a Linux-profile VM must give two issues. Today `validate` returns none.

### F14. Port 0 and an ICMP rule with a port pass validation (wrong result)

- Where: `src/config/extract.rs:295` (`port_s.parse::<u16>()`),
  `src/config/model.rs:1546-1556` (`parse_host_port`),
  `src/config/extract.rs:195-208` (`block`), `src/net/rules.rs:544-552`,
  `src/config/schema.wcl:190`.
- Why it is a fault: `forward { to = "web:0" }` and
  `redirect { from = "10.0.0.1:0" to = "10.0.0.2:0" }` are accepted. Every
  other port in the lab file is read with `Reader::port`, which requires
  1-65535. A `block` rule with `proto = "icmp"` and a `port` is accepted and
  can never match, because an ICMP packet has no destination port
  (`rules.rs:549-552`). The lab author believes the traffic is blocked. The
  schema says `port` "requires `proto`", and nothing enforces that either: a
  rule with a port and no protocol is accepted and matches TCP and UDP.
- Proof: unit test `ports_that_can_never_match_are_refused` in
  `src/config/extract.rs` with the three declarations above. Each must give
  an issue. Today none does. For the ICMP rule also a test in
  `src/net/rules.rs`: an echo request to an address inside the rule's CIDR
  must be dropped or the rule refused. Today it passes.

### F15. `oci_chunk_size` is bounded only when a push starts (wrong result)

- Where: `src/config/host.rs:291-293` (`r.size`, any value from 0),
  `src/oci/chunking.rs:79-80` (`ensure!(chunk_size > 0)`,
  `ensure!(chunk_size <= MAX_CHUNK_SIZE)`).
- Why it is a fault: `oci_chunk_size = 0` and `oci_chunk_size = 2GiB` load
  as a valid host config. The first use is `chunk_and_compress`, which a push
  reaches after its build. The error there names `chunk_size` and not the
  host config field or file. `workspace_max_file = 0` loads the same way and
  makes the syncer refuse every file that is not empty.
- Proof: unit test `an_unusable_chunk_size_is_refused_at_load` in
  `src/config/host.rs`: `host_err("host { oci_chunk_size = 0 }\n")` and
  `host_err("host { oci_chunk_size = 2GiB }\n")` must name the field. Today
  both parse.

### F16. The digest blanks span-like text inside declared strings (cosmetic)

- Where: `src/config/mod.rs:187-198` (`declaration_digest`).
- Why it is a fault: the digest is the SHA-256 of the model's `Debug` text
  after a regex replaces every `…span: (n, m)` with `span: _`. The regex runs
  over the whole text, declared strings included. Two lab files that differ
  only in such text inside a string have one digest, so `up` keeps the daemon
  that loaded the old file. The input is unlikely. The method is the fault:
  a digest over `Debug` text depends on a format that no test pins.
- Proof: unit test `a_string_that_looks_like_a_span_still_counts` in
  `src/config/mod.rs`: `DIGEST_LAB` with `qemu_args = ["span: (1, 2)"]` and
  with `qemu_args = ["span: (3, 4)"]` must have different digests. Today they
  are equal.

## Notes

No proving test, or not checked far enough to be a finding.

- N1. The three loaders test for the schema import with
  `source.contains("import <…>")` (`src/config/mod.rs:79`,
  `src/config/host.rs:215`, `src/profiles/mod.rs:236`). A file that holds the
  text only in a comment passes. If WCL then loads the file with no schema,
  unknown fields are not reported. I could not run WCL here to see what it
  does.
- N2. All three loaders pass `disk_loader()`, so `import "<path>"` in a lab
  file reads any file on the host, and a parse error may quote it. I did not
  read `wcl_lang` to see what an error shows. The earlier review has the same
  point for an imported archive's `template.wcl` (templates-registry N5).
- N3. `redact_args` and `osk_secrets` (`src/qemu/process.rs:19-34`) match
  the text `osk=` anywhere in an argument. A `qemu_args` element such as
  `kiosk=1` is cut in the log header, and its value becomes a "secret" that
  is replaced in all of QEMU's output.
- N4. vmlab-hv's `machine.json` is written with `std::fs::write` and the
  default mode (`src/labd/hypervisor/vmlab_hv/mod.rs:260`). The description
  type has a field for the OSK. vmlab-hv refuses macOS today, so no key is
  written. I did not check the mode of the directory.
- N5. A PSK of any length is accepted, also one character
  (`src/config/host.rs:247`). An empty one is refused only when a trunk is
  opened (`src/supervisor/trunk_auth.rs:31`). I did not review whether the
  handshake lets a peer test guesses offline.
- N6. A template definition's `login { password }` is written into the
  store metadata (`src/template/meta.rs:511-512`) and into the OCI config
  blob (`src/oci/config_blob.rs:153`), so a push publishes it. The schema
  says the password is plain by decision. The manual should say that a push
  publishes it. I did not read the manual.
- N7. A pulled template's `logins` are not run through `check_logins`
  (duplicate labels, two defaults). They are selectable on every clone that
  declares none.
- N8. `cpu_model` from a pulled template is passed as the start of the
  `-cpu` value (`src/qemu/cmdline.rs:512`). It can add CPU properties and
  nothing else. `cpus` and `memory` from a pulled template have no cap
  (templates-registry N12).
- N9. A lab-level `sinkhole` with an empty pattern is accepted. The
  segment-level one is refused (`src/config/validate.rs:179-183`). An empty
  pattern matches no name, so the effect is none.
- N10. `record` names, `route` gateways, `redirect` targets and `connect`
  hosts have no semantic check: a `via` outside the segment's subnet, a
  record name that is not a DNS name. I did not trace what each does at run
  time.
- N11. A forward's target must be a declared machine
  (`src/config/validate.rs:171-178`). Nothing checks that the machine has a
  NIC on the segment that declares the forward. I did not read
  `src/labd/forward_plan.rs` far enough to say what happens.
- N12. A segment named `nat` beside a `nic { nat = true }` passes
  `vmlab validate` and fails at `up` (`src/labd/network.rs:241-247`).
- N13. `firmware_width` and `firmware_height` have a floor in the extractor
  and their ceiling only in the resolver (`src/qemu/resolve.rs:548-557`).
  `vm_hardware` drops the resolver's error
  (`src/config/validate.rs:2069`), so `vmlab validate` passes a width of
  100000 and the start refuses it. The schema's `@range` decorators enforce
  nothing by themselves (`src/config/schema.wcl:15-20`), so each bound exists
  only where the extractor repeats it.
- N14. A template definition's `cpus = 0` is accepted by the extractor
  (`src/config/extract.rs:999`, `:1095`), and `memory = 0` and `disk = 0` by
  `Reader::size`. I did not find the §5.1 check the comment refers to.
- N15. `login.user` is any string, also empty or starting with `-`. On Linux
  it reaches `su -l`. I did not read `guest/agent/src/unix/login.rs` to see
  how the argument is passed.
- N16. The host GPU `address` is not checked to be a PCI address
  (`src/config/host.rs:300`). It reaches `vfio-pci,host=` and sysfs paths.
  The host config is trusted.
- N17. The lab digest covers the lab file's model only. A changed host
  config or user profile does not restart a running lab daemon, and some
  sites read the host config again at each use
  (`src/labd/vm.rs:867`, `src/oci/client.rs:694`) while others keep the copy
  from the daemon's start (`src/labd/lab.rs:3055`).
- N18. One user profile file that fails to parse fails
  `ProfileSet::load` (`src/profiles/mod.rs:176-193`), and with it every
  command that loads profiles.
- N19. `vmlab template rm` joins its arguments into a store path with no
  component check (`src/template/store.rs:218-220`, `:573-601`). With
  `--force` an unreadable metadata file does not stop the removal. The
  arguments come from the command line, not from a config file, so this is
  outside this review. The install path does check
  (`src/template/trust.rs:20-28`).
- N20. `Redactor::scrub` (`src/labd/playbook_vars.rs:213-232`) replaces a
  secret where it appears as written. A secret that the guest prints in
  another encoding (JSON-escaped, base64, URL-encoded) is not replaced.

## What I read and what I did not

Read in full: `src/config/schema.wcl`, `host_schema.wcl`, `host.rs`,
`mod.rs`, `extract.rs`, and the non-test part of `validate.rs` except
`:1100-1380` (secure boot and key store checks), which I skimmed.

Read in part: `src/config/block.rs` (`:1-520`), `src/config/model.rs`
(the parsers at `:360-420`, `:980-1110`, `:1375-1560`),
`src/config/hypervisor.rs` (`select` and the display refusal),
`src/profiles/mod.rs` (`:157-345`), `src/template/meta_schema.wcl`,
`src/template/trust.rs`, `src/template/store.rs` (`:205-610`),
`src/oci/reference.rs`, `src/oci/config_blob.rs` (`into_meta`),
`src/qemu/resolve.rs` (display and the resolved VM), `src/qemu/cmdline.rs`
(disks, display, SMC), `src/qemu/process.rs` (redaction),
`src/labd/network.rs` (`build`), `src/smb/config.rs` (the share sections),
`src/labd/playbook_vars.rs` (`Redactor`), and the call sites each finding
names.

Not read: `src/config/projection.rs`, the tests of every file, the
`wcl_lang` crate, `src/template/build.rs`, `src/template/meta.rs` beyond the
fields named above, the wscript host module, the guest agents, and the
manual. Template pull, push, import and the store's own faults are in
`docs/reviews/templates-registry.md` and are not repeated here.
