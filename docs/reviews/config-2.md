# Second review: the lab file, config and host config

Card #968. A second bug-hunting pass over how vmlab reads and validates
`vmlab.wcl`, `~/.config/vmlab/config.wcl` and guest profiles, how it selects a
hypervisor, and how it names a lab instance. It lists faults and makes no fix.
Reviewed at `next` commit `759d5fa7`. All paths are from the repository root,
and all line numbers are from that commit.

The first pass is `docs/reviews/config.md` (card #532, findings F1 to F16,
notes N1 to N20). This document continues its numbering, so a code names one
finding across both: F17 to F29 and N21 to N31 are new here. A finding of the
first pass is named again only where its fix is incomplete, and the text says
which.

Severity words are those of the first pass: **data loss**, **crash** (of a
host process), **security**, **wrong result**, **cosmetic**. Who is trusted is
as the first pass states it: a lab file is as trusted as a build script, a
pulled template is not trusted, and the host config is trusted and holds two
secrets.

## How the findings were checked

Every finding is from reading the code. Twelve of the thirteen were also
shown by temporary tests, run on helios in the checkout of this card with
`cargo test --all-features --lib review968 -- --nocapture --test-threads=1`.
The tests are not in the tree. They are in
`docs/reviews/config-2-probes.md` with what they printed, and each one passes
while its fault is present. The checkout was put back after the runs.

F17 was not run: it needs a supervisor started under a scratch configuration
directory. Its text says what was read.

Each finding names the test that would prove it. A finding whose test fails
to reproduce is wrong and should be dropped. No test below uses a wall-clock
limit.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F17 | wrong result | A host config that does not load is replaced by the defaults at eleven sites without a word; the supervisor then runs on the defaults until it is restarted |
| F18 | crash, wrong result | An issue in an imported WCL file carries that file's byte offsets, which are applied to the top file: a wrong `file:line:col`, no excerpt, and a panic for a profile file |
| F19 | wrong result | A share `host = "~/…"` is served from the home directory and checked under the lab root, so the example of the PRD and the manual fails `vmlab validate` |
| F20 | wrong result | `@dev(workspace_guest = "/.")`, `"/tmp/.."` and `"C:\\."` pass the root check (F13 fix incomplete) |
| F21 | wrong result | `workspace_max_file = 0` loads, and the syncer then refuses every file that is not empty (F15 fix incomplete) |
| F22 | wrong result | `subnet_pool` of any prefix and any range loads: a `/28` fails every `up`, and `0.0.0.0/0` hands out `0.0.0.0/24` (brought in by the F7 fix) |
| F23 | wrong result | A NIC `mac` that no NIC can hold passes: a multicast address, all zeros, the broadcast address |
| F24 | wrong result | A share's `guest` path is not checked: a relative path, an empty one, a Unix path on a Windows guest |
| F25 | wrong result | A registry template reference that the pull refuses passes `vmlab validate` |
| F26 | wrong result | `dns_upstream` with port 0 or the address `0.0.0.0` loads (F11 fix incomplete), and `dns_suffix` is any string |
| F27 | security | A secret under a mistyped field name is printed by the diagnostic that reports the mistake (F5 fix incomplete) |
| F28 | wrong result | Two instance ids that differ only in punctuation or case give one effective lab name |
| F29 | wrong result | A TCP forward and a UDP forward on one host port are refused as duplicates |

### F17. An unreadable host config becomes the defaults (wrong result)

- Where: `src/supervisor/mod.rs:150` (`HostConfig::load_default().unwrap_or_default()`),
  `src/supervisor/global.rs:664-675` (`GlobalSegments::new` takes the DNS
  suffix, the upstream resolver and the PSK from it),
  `src/supervisor/mod.rs:203-211` (the trunk listener starts only with a PSK),
  `src/supervisor/store.rs:642`, `src/labd/mod.rs:125`,
  `src/nocow.rs:169-171` (`.unwrap_or(true)`), `src/cli/validate.rs:90-92`
  and `:247-249`, `src/viewer.rs:75` and `:156`, `src/appshare/mod.rs:129`,
  `src/template/build.rs:2067-2069`, `src/labd/vm.rs:2472`.
- Why it is a fault: `HostConfig::parse` refuses a file for any issue, one
  mistyped field included. At the sites above the error is dropped and
  `HostConfig::default()` is used. Nothing is printed or logged. The effects:
  - The supervisor is one process per user and outlives every lab. Started
    while the file has a mistake, it builds `GlobalSegments` with no PSK, the
    suffix `vmlab.internal` and the host's resolver as upstream, and starts no
    trunk listener. After the user corrects the file the supervisor still
    runs on the defaults, because it read the file one time. A global segment
    then sends its unanswered DNS queries to the host's resolver although
    `dns_upstream` names another, which is the effect the first pass's F11 was
    about, and a peer's trunk is refused although `psk` is set.
  - `btrfs_nocow = false` beside a mistake elsewhere in the file reads as
    `true`, and the machine's disk directory is marked NOCOW.
  - `vmlab validate` reports the host config only for a lab with an Apple
    SMC or a `gpu {}` (`src/cli/validate.rs:52` and `:110`). For any other
    lab it passes, and it selects each hypervisor from the default
    `hypervisor {}` settings (`:90-92`). The comment at `:242` says
    "validation already said it", which holds only for those two cases.
  - A template push uses the default `oci_chunk_size`, and the viewer and
    shared-app clients use the default paths.
  The lab daemon does fail on the same file (`src/labd/lab.rs:566` uses `?`),
  so `vmlab up` reports the mistake. By then the supervisor has started.
- Proof:
  1. Test beside the supervisor tests that start the binary under a scratch
     `XDG_CONFIG_HOME` (`src/supervisor/mod.rs:1512`): write a `config.wcl`
     with `host { psk = "k" trunk_port = 0 }`, start the supervisor, and
     assert that it exits with an error that names `trunk_port`. Today it
     starts and listens on its socket.
  2. Unit test `an_unreadable_host_config_is_not_the_defaults`: a function
     that takes the result of `HostConfig::load_default` and returns the
     NOCOW decision must return an error or a warning for an `Err`. Today
     `decide_on_host` has no such seam, and `.unwrap_or(true)` is the fault.
- I did not run a supervisor under a broken file. The statement about the
  trunk and the resolver is from the code named above.

### F18. An issue in an imported file is placed in the top file (crash, wrong result)

- Where: `src/config/block.rs:694-704` (`line_col` slices the top file's
  text at the issue's offset), `:678-691` (`render_issues`),
  `src/profiles/mod.rs:275` (`finish(name, source, …)` with the file's own
  text), `src/config/host.rs:415-419`, `src/config/mod.rs:46-54`
  (`Issue::at`), `:128-145` (`redacted_source`), `:221-234`.
- Why it is a fault: all three loaders accept `import "<path>"`. A block of
  the imported file is extracted like a block of the top file, and the span
  of an issue in it is a pair of byte offsets into the imported file. Every
  consumer applies the offsets to the top file's text:
  - A profile file: `line_col` evaluates `&source[..offset]`. Where the
    offset is inside a multi-byte character of the top file, that is a panic
    (`src/config/block.rs:695`). The panic is in `ProfileSet::load`, which the
    CLI, the lab daemon (`src/labd/mod.rs:128`) and the supervisor's template
    builds (`src/supervisor/templates.rs:368`) call, so one user profile file
    with an import, a comment that is not ASCII and a mistake stops every
    command and every lab daemon at start.
  - The host config: the text is blanked to ASCII, so there is no panic. The
    error names a line and column of `config.wcl` where nothing is wrong.
  - A lab file: the miette label is outside the top file, so the report says
    `Failed to read contents for label … OutOfBounds` and shows no excerpt.
    Where the offset is inside the top file, the excerpt shows unrelated
    text. `redacted_source` stars the same wrong offsets, so it stars
    unrelated text of the top file. No secret of the imported file is shown,
    because the imported file's text is never the excerpt.
- Proof:
  1. Unit test `an_issue_in_an_imported_profile_file_is_an_error_not_a_panic`
     in `src/profiles/mod.rs`: an imported file with
     `profile "p" { machine = "bogus" }` and a top file whose second line is a
     comment of forty `é`. `parse_profiles` must return an error. Today it
     panics with `end byte index 41 is not a char boundary`.
  2. Unit test in `src/config/host.rs`: an imported file with a bad
     `keepawake` on its line 9 and a top file of three lines. The error must
     name the imported file, or no position. Today it says `config.wcl:3:10`.
  3. Unit test in `src/config/mod.rs`: a lab file that imports a template
     definition with `arch = "bogus"`. No issue may carry a span that is
     outside the top file. Today the span is at offset 1754 of a top file of
     176 bytes.

### F19. A `~` share is checked under the lab root (wrong result)

- Where: `src/config/validate.rs:319-328` (`file.root.join(&share.host).is_dir()`),
  `:347-351` (the other checks use `resolve_share_host`),
  `src/labd/share_plan.rs:504-514` (`~` becomes the home directory),
  `docs/vmlab-prd.md:590`, `docs/manual/pages/guide/shares.wcl:24`.
- Why it is a fault: the runtime serves `host = "~/datasets"` from
  `$HOME/datasets`. Validation tests whether `<lab root>/~/datasets` is a
  directory. So the share that the PRD and the manual give as their example
  fails with "share host path ~/datasets is not a directory", in
  `vmlab validate` and in every verb, because every side-effecting verb
  validates first (`src/cli/validate.rs:284-297`). Where a directory named
  `~` does exist under the lab root, the check passes on that directory and
  the guest gets the home directory. The checks added for the SMB findings
  (`share_host_problem`, `share_over_lab_state`) use the served path. Only
  the existence check does not.
- Proof: unit test `a_tilde_share_is_checked_where_it_is_served` in
  `src/config/validate.rs`: a lab under a temporary root with
  `share { host = "~" guest = "/mnt/h" }` must give no "is not a directory"
  issue when the home directory exists. Today it gives one, and it gives none
  after `<root>/~` is created.

### F20. A guest root spelled with dots passes (wrong result, F13 fix incomplete)

- Where: `src/config/validate.rs:2219-2225` (`is_guest_root`), `:2199-2215`,
  `:1653-1668` (`guest_path_problem` tests only how the path starts).
- Why it is a fault: the fix of F13 refuses `/` and `C:\` as
  `workspace_guest`, because the syncer would own the whole guest.
  `is_guest_root` compares bytes: a path is a root when nothing but
  separators follows the drive. `/.`, `/..`, `/tmp/..`, `C:\.` and
  `C:\Users\..` are the same directories and pass. The same holds for a
  profile's `workspace_guest` (`src/profiles/mod.rs:371`), which has no check
  at all.
- Proof: extend `dev_arguments_are_checked_like_a_share`
  (`src/config/validate.rs:3344`) with those five values. Each must give one
  issue. Today each gives none.
- I did not trace what the agent does with such a path. I assume the guest's
  file system resolves it to the root.

### F21. `workspace_max_file = 0` loads (wrong result, F15 fix incomplete)

- Where: `src/config/host.rs:346-348`, `src/labd/workspace/scan.rs:212` and
  `:487` (`side.size > max_file_bytes`).
- Why it is a fault: the first pass named this value in F15. The fix bounded
  `oci_chunk_size` only. With a cap of 0 every file of one byte or more is
  over the cap, so the syncer refuses the whole workspace file by file, and
  each refusal tells the user to raise a cap they did set.
- Proof: unit test `a_zero_workspace_cap_is_refused_at_load` in
  `src/config/host.rs`: `host_err("host { workspace_max_file = 0 }\n")` must
  name the field. Today `HostConfig::parse` returns `Ok`.

### F22. Any `subnet_pool` loads (wrong result, brought in by the F7 fix)

- Where: `src/config/host.rs:269-271`, `src/labd/network.rs:217-226`.
- Why it is a fault: before the fix of F7 the pool was never read. Now
  `LabNetwork::build` carves `/24`s from it with `pool.subnets(24)`, and the
  host config still accepts any CIDR. A pool with a prefix longer than `/24`
  yields nothing, so every lab with a segment that declares no subnet, or
  with `nat = true`, fails at `up` with "auto-subnet pool … has no available
  /24". A pool of `0.0.0.0/0` yields `0.0.0.0/24` as its first subnet, and
  `224.0.0.0/4` yields multicast addresses, which no guest can use. The lab
  file's own `subnet` has a bound since F8 (`src/config/validate.rs:123-133`).
  The pool has none.
- Proof: unit test `an_unusable_subnet_pool_is_refused_at_load` in
  `src/config/host.rs`: `host_err` for `subnet_pool = "10.0.0.0/28"`,
  `"0.0.0.0/0"` and `"224.0.0.0/4"` must name the field. Today all three
  parse.

### F23. A MAC that no NIC can hold passes (wrong result)

- Where: `src/config/model.rs:750-763` (`MacAddr::from_str`),
  `src/config/validate.rs:1853-1857` (the only check is the duplicate),
  `src/net/switch.rs:864` and `:882` (a multicast source is never learned).
- Why it is a fault: `mac = "01:00:5e:00:00:01"`, `"00:00:00:00:00:00"` and
  `"ff:ff:ff:ff:ff:ff"` pass validation. The first octet's low bit marks a
  group address, which is not valid as the address of an interface. The
  switch does not learn such a source, so every frame to that guest is
  flooded to all ports of the segment. A Linux guest refuses to bring up an
  interface whose address is not a valid unicast address. The machine then
  has no network and nothing names the `mac`. The parser also takes a signed
  octet (`"+1:+2:+3:+4:+5:+6"`), because `u8::from_str_radix` does.
- Proof: unit test `a_mac_is_a_unicast_address` in
  `src/config/validate.rs`: a NIC with each of the three addresses must give
  an issue. Today none does.
- I did not start a guest with such an address. The statement about Linux is
  from the kernel's `eth_validate_addr`, not from a run.

### F24. A share's `guest` path is not checked (wrong result)

- Where: `src/config/validate.rs:318-369` (no rule reads `share.guest`
  except the name derivation at `:329-336`), `src/smb/steps.rs:229-240`
  (`mkdir -p <guest>`, `mount … <guest>`).
- Why it is a fault: a playbook's `guest_dir` and an `@dev`
  `workspace_guest` must be absolute for the guest's family
  (`guest_path_problem`). A share's mount path has no such rule.
  `guest = "mnt/src"` on a Linux guest is created and mounted relative to
  the agent's working directory. `guest = "/mnt/x"` on a `windows-11` VM and
  an empty `guest` beside an explicit `name` pass too, and fail in the guest
  at `up`.
- Proof: unit test `a_share_guest_path_is_absolute_for_the_family` in
  `src/config/validate.rs`: `guest = "mnt/src"` and `guest = ""` on a
  `linux` profile, and `guest = "/mnt/x"` on `windows-11`, each with
  `name = "n"`, must each give an issue. Today none does.

### F25. A registry reference the pull refuses passes validation (wrong result)

- Where: `src/config/validate.rs:1140-1149` (the only rule for
  `TemplateRef::Registry` is the explicit `arch`),
  `src/config/model.rs:1407-1421` (the reference is kept as written),
  `src/oci/reference.rs` (`Reference::parse`, the grammar since F2).
- Why it is a fault: the fix of F2 made `Reference::parse` refuse a
  repository with `..`, an empty segment or other characters. Validation does
  not call it. `template = "ghcr.io/a/../b:1"` passes `vmlab validate`, and
  `up` fails at the pull with the parser's message.
- Proof: unit test `a_registry_reference_is_parsed_at_validation` in
  `src/config/validate.rs`: a VM with that template and an `arch` must give
  an issue. Today `validate` returns none, and `Reference::parse` of the same
  text is an error.

### F26. `dns_upstream` port 0 and any `dns_suffix` load (wrong result, F11 fix incomplete)

- Where: `src/config/host.rs:275-284`, `:450-457` (`parse_upstream`),
  `:272-274` (`dns_suffix`), `src/labd/network.rs:525` and `:549`,
  `src/net/dns.rs:112-114`.
- Why it is a fault: the fix of F11 accepts what parses as a socket address.
  `"10.0.0.1:0"` and `"0.0.0.0"` parse. No resolver answers there, so every
  name the zone does not hold fails, with no message. The fix of F14 refused
  port 0 in the lab file for the same reason. `dns_suffix` is taken as
  written: `""` makes the zone `<lab>.` with an empty suffix, and
  `"has space"` goes into DHCP's domain option and into the zone.
- Proof: unit test `an_unusable_dns_setting_is_refused` in
  `src/config/host.rs`: `host_err` for `dns_upstream = "10.0.0.1:0"`,
  `dns_upstream = "0.0.0.0"`, `dns_suffix = ""` and
  `dns_suffix = "has space"` must name the field. Today all four parse.

### F27. A secret under a mistyped field name is printed (security, F5 fix incomplete)

- Where: `src/config/mod.rs:149-164` (`secret_fields`: a field named
  `password`, and a `value` beside a `secret`).
- Why it is a fault: the fix of F5 stars a secret by the name of its field.
  The schema reports an unknown field with a label on that field, and the
  excerpt is the line as written. So `login "d" { user = "u" passwd = "…" }`
  prints the password in the diagnostic that reports `passwd`. A
  `var "t" { value = "…" secrt = true }` prints its value the same way. The
  cases the fix covers hold: a wrong type, a list and an expression that does
  not evaluate are starred, and no WCL parse or evaluation error quotes a
  value (checked, see the probes).
- Proof: extend `a_diagnostic_does_not_show_a_secret_value`
  (`src/cli/validate.rs:353`) with a login that writes `passwd` for
  `password`. The rendered report must not hold the value. Today it does.
- How bad: low. It needs a mistake in the one field, and the output goes to
  the terminal or the CI log of the person who wrote the file.

### F28. Two instance ids give one effective name (wrong result)

- Where: `src/instance.rs:115-145` (`effective_name`), `:166-172`
  (`belongs_to`), `src/supervisor/registry.rs:51-62`.
- Why it is a fault: the instance is reduced to lower-case letters, digits
  and single hyphens, and the hash is added only when the name is too long.
  So the worktrees `fix_1`, `fix-1` and `Fix.1` of one lab all run as
  `<lab>-fix-1`, and `lab "demo-a"` as instance `b` has the name of
  `lab "demo"` as instance `a-b`. The registry lets one root hold a name, so
  the second directory is refused with a message whose advice is to run it
  "as its own instance with `--instance <id>`", which it already is. The
  module's doc says an instance gives a directory its own name.
- Proof: unit test `two_instances_have_two_names` in `src/instance.rs`:
  `effective_name("demo", "fix_1")` and `effective_name("demo", "fix-1")`
  must differ. Today they are equal.

### F29. TCP and UDP forwards on one host port are duplicates (wrong result)

- Where: `src/config/validate.rs:226-256` (`claim` keys on port and bind
  address).
- Why it is a fault: a TCP listener and a UDP socket on one host port do not
  collide. `forward { host_port = 5353 to = "a:53" proto = "tcp" }` beside
  `forward { host_port = 5353 to = "a:5353" proto = "udp" }` is refused with
  "duplicate forward host_port 5353". `proto = "both"` covers only the case
  of one guest port.
- Proof: unit test `a_tcp_and_a_udp_forward_share_a_host_port` in
  `src/config/validate.rs` with those two blocks. `validate` must return no
  issue. Today it returns the duplicate.
- I did not read `src/labd/forward_plan.rs` to see that the install keys on
  the protocol too. If it does not, the rule is right and the fault is
  there.

## Notes

No proving test, or not checked far enough to be a finding.

- N21. A second `host {}` block in the host config is ignored without a
  word: `HostConfig::parse` reads the first (`src/config/host.rs:265`). Seen
  in the probes. The lab file's schema refuses a second single child
  (`field 'dns' expects a single 'dns' child, found 2`).
- N22. A container's volumes are not compared: two volumes with one
  `target`, and a `target = "/"`, pass. Two `env` blocks with one name pass
  too. Seen in the probes. I did not trace which one wins in the guest.
- N23. A segment `subnet` with host bits set (`10.1.1.5/24`) is kept as
  written, and a `dns { server }` outside the segment's subnet passes. Seen
  in the probes. The gateway and the overlap check use `network()`. I did not
  trace every use of the subnet in `src/labd/network.rs`.
- N24. `memory = 0` on a `vm` passes `vmlab validate`, for the reason in
  N13: `vm_hardware` drops the resolver's error.
- N25. The guest family that the login and path rules use is the profile's
  name prefix (`login_family`, `src/config/validate.rs:1485-1492`). A user
  profile for Windows that is not named `windows…` is `Unknown`, so a login
  with no password and a Unix `workspace_guest` pass. The profile has no
  field for the family.
- N26. `is_macos` tests the profile name `macos` exactly
  (`src/config/hypervisor.rs:320-322`) and `login_family` tests the prefix.
  A user profile `macos-15` is macOS for the login rules and not for the
  hypervisor's CPU model rule, unless it sets `apple_smc`.
- N27. `HostConfig::load_default` returns the defaults when `config.wcl` is
  not a regular file (`src/config/host.rs:239-241`). A dangling symlink is
  the defaults, not an error.
- N28. `trusted_template_repositories` entries are not checked: `""`, `"/"`
  and a bare host load. None of them widens the trust (`repository_trusted`
  needs an exact name or a namespace prefix), so an entry with a mistake
  trusts nothing and says nothing.
- N29. `parse_template_ref` (`src/config/model.rs:1407-1458`) accepts any
  text as a store template's name and version: `x86_64/..` and
  `x86_64/t@../../x`. The lab file is trusted and the install path checks
  its components (`src/template/trust.rs:6-18`), so the effect is a store
  lookup outside the template's directory. I did not trace the lookup.
- N30. A `url` source that is not `https` is quoted whole in the issue
  (`src/config/extract.rs:1153-1158`). A URL with credentials in it would be
  printed. `diagnostic_source` does not cover a `url`.
- N31. `src/config/hypervisor.rs`: no fault found. An aarch64 VM on an
  aarch64 host gets two refusal sentences for one cause (`:226-237` and
  `:251-254`), which is cosmetic.

## What I read and what I did not

Read in full, without the tests: `src/config/mod.rs`, `block.rs`,
`extract.rs`, `validate.rs`, `host.rs`, `hypervisor.rs`, `src/instance.rs`.

Read in part: `src/config/model.rs` (the parsers and the types at `:255-275`,
`:366-470`, `:728-800`, `:985-1110`, `:1330-1560`), `src/profiles/mod.rs`
(`:150-390`), `src/config/schema.wcl` and `host_schema.wcl` (the fields the
findings name), `src/labd/network.rs` (`build`), `src/labd/share_plan.rs`
(`resolve_share_host`), `src/smb/steps.rs` (the Linux mount steps),
`src/template/trust.rs`, `src/supervisor/mod.rs` (`:140-225`),
`src/supervisor/global.rs` (`:652-690`), `src/supervisor/registry.rs`
(`:30-62`), `src/nocow.rs` (`:165-178`), `src/cli/validate.rs` (the context
and `validate_current`), `src/net/switch.rs` (`:855-892`), and the call sites
each finding names.

Not read: `src/config/projection.rs`, the tests of every file,
`src/profiles/shipped.wcl`, the `wcl_lang` crate, `src/qemu/resolve.rs`,
`src/labd/forward_plan.rs`, the workspace syncer beyond the two size
comparisons, the guest agents, and the manual beyond the one share example.
The fixes of F1 to F3 and F9 are outside `src/config` and were not read
again. The notes N1 and N2 of the first pass (the schema import test and
what an `import` can read) are still open and were not taken further, except
that F18 shows an import reaches all three loaders.
