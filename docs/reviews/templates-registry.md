# Review: templates, registry and OCI pull and push

Card #440. A bug-hunting review of template build, seal and store, the OCI
registry client (pull, push, login, pagination, limits), manifest and config
parsing, the container image pull, and the layout of the template store on
disk. It lists faults and makes no fix. Reviewed at `next` commit `723bd882`.
All paths are from the repository root, and all line numbers are from that
commit.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped.

Severity words: **data loss**, **crash** (of the VM or a host process),
**security** (against a hostile registry, a crafted template, or a local
peer), **wrong result**, **cosmetic**.

The threat model used: a registry is hostile once a lab or a command names it.
It controls every manifest, config blob, tag list and header it serves. TLS
protects the path to it, not its content. A template archive given to
`vmlab template import` is crafted by a stranger. A lab file is trusted as
much as a Makefile is.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F1 | data loss | A pull that brings a changed variant deletes the old overlay under the clones that are backed by it (fixed, #461) |
| F2 | data loss | `--overwrite` on pull and import replaces a version's disks with no in-use check (fixed in card #464) |
| F3 | security | The store slot comes from the config blob, the cache lookup from the repository name, so one repository can fill another reference's slot (fixed, #480) |
| F4 | security, crash | The config blob's `secure_boot_vars` is decompressed with no limit, on every resolve of a registry reference (fixed in card #481) |
| F5 | security | A template pull bounds each chunk but not the number of chunks or the total size (fixed, #482) |
| F6 | security | The layer flatten is quadratic in the entry count, has no entry or size limit, and runs on a thread that a cancel cannot stop (fixed, #503) |
| F7 | security | The store accepts a qcow2 with an external data file (reachable only with `VMLAB_IMAGE_TOOL=qemu-img`) |
| F8 | data loss | `template login` replaces a docker config it cannot parse with an empty one, and panics on a non-object `auths` (fixed, #465) |
| F9 | wrong result | A hard link in a container layer loses its target when a higher layer deletes or replaces that target |
| F10 | wrong result | The registry HTTP client has no timeout (fixed, #505) |
| F11 | wrong result | A pull does not compare the config blob's `sha256` with the manifest's whole-image digest, and a push does not compare the disk with the recorded digest (fixed, #504) |
| F12 | wrong result | Two pulls of one template at the same time: the second fails with "already exists" |
| F13 | wrong result | A cancelled pull removes the work directory under the install that is still running (fixed, #521) |
| F14 | wrong result | Staging directories of the store, of pulls, of pushes and of the image cache are never swept |
| F15 | wrong result | The artefact cache shares one `.part` file between concurrent downloads, and uses the declared `sha256` as a file name unchecked |
| F16 | wrong result | A push onto a tag that holds a plain manifest drops that manifest's architecture |
| F17 | wrong result | The flatten drops PAX extended attributes, so file capabilities are lost |
| F18 | wrong result | A 404 on a later page of a tag list returns an empty list |
| F19 | cosmetic | Strings from a registry reach the terminal unfiltered |

### F1. A pull that brings a changed variant deletes the old overlay under its clones (data loss)

- Where: `src/oci/client.rs:787-791` (a held variant is skipped only when its
  digest is equal), `:873-880` (the call), `src/template/store.rs:741-782`
  (`add_variants`), `:758-766` (remove, then rename).
- Why it is a fault: a pull into a version that the store already holds
  fetches the default variant and every variant asked for
  (`VariantWant::choose`, `src/oci/client.rs:1041-1047`). A variant the store
  holds under the same name with another digest is not skipped. It goes to
  `add_variants`, which runs `fs::remove_dir_all(variants/<name>)` and renames
  the new overlay to the same path. Nothing asks whether a clone is backed by
  the old overlay. `store.remove` (`src/supervisor/store.rs:211-236`) and a
  variant rebuild (`refuse_in_use`, `src/template/build.rs:664-680`) both
  refuse in this case. A clone is a delta against the old overlay. After the
  replacement its backing path names another image, so the guest disk is
  corrupt at the next start. A running VM keeps the old inode only until it
  stops.
  The path needs no flag. Lab B asks for a variant the store lacks, the pull
  runs from `ensure_registry_template` (`src/oci/client.rs:1157-1209`) during
  B's `up`, and the registry's default variant of the same version was pushed
  again after a rebuild (`template build --variant` followed by a push to the
  same version tag). Lab A's clones of the default variant are then damaged.
- Proof:
  1. Store test `adding_a_variant_that_replaces_one_asks_whether_it_is_in_use`
     in `src/template/store.rs`: install a version with variant `v` (digest
     A), call `add_variants` with a `v` of digest B. Today it succeeds, the
     old `variants/v/disk.qcow2` is gone, and `add_variants` has no parameter
     through which a caller could refuse.
  2. Client test `a_pull_does_not_replace_a_variant_a_clone_is_backed_by` in
     `src/oci/client.rs` with the fake transport: push a base and variant `v`,
     pull, create a linked clone on `variants/v/disk.qcow2`, push a rebuilt
     `v` and a new variant `w`, pull `#w`. Assert the clone's backing file
     still hashes to digest A. Today it hashes to B.
- Fixed in card #461. `add_variants` takes an in-use check, asks it under the
  store lock about every overlay it would replace, and refuses with nothing
  moved. A pull asks the same check (`Registry::guarding`, every registered
  lab's clones) before it downloads a variant the store holds with another
  digest. While a clone is backed by the held overlay, a pull that only
  brought the variant along as the default keeps it and fetches the rest, and
  a pull that asked for it is refused, naming the machines. A client with no
  check replaces no held overlay. The second proof test failed before the fix
  (the held overlay hashed to B) and passes after it. The first could not
  compile before the fix, as the finding says. Not fixed here: `--overwrite`
  (F2).

### F2. `--overwrite` on pull and import replaces a version with no in-use check (data loss)

- Where: `src/supervisor/store.rs:453-467` (`import`), `:472-493` (`pull`),
  `src/template/store.rs:392-408` and `:424-453` (`swap_in`, which removes
  `.old-<version>` at `:451`).
- Why it is a fault: both commands pass `overwrite` straight to the store.
  `swap_in` moves the old version aside and deletes it. Neither command calls
  `clones_by_backing`, which `remove` and `prune` call for the same hazard and
  which `remove` overrides only with `--force`. Every clone of the replaced
  version, in any lab, is then a delta against a different base disk. The
  flag is explicit, but its name says nothing about clones and there is no
  refusal to force past.
- Proof: supervisor test `an_overwriting_import_is_refused_while_a_clone_is_backed_by_the_version`
  in `src/supervisor/store.rs`: import an archive, register a lab whose disk
  is a linked clone of the installed `disk.qcow2`, import an archive of the
  same version with another disk and `overwrite = true`. Expect a refusal
  that names the machine, as `backed_clones_reason` gives for `remove`. Today
  the import succeeds. The same test with `pull`.
- Fixed in card #464. `store.import` and `store.pull` with `overwrite` scan
  the registered labs with `clones_by_backing` and hand the store a replace
  guard (`TemplateStore::refusing_replace`). An install that would replace a
  version asks the guard about the base and every variant overlay in the
  version directory before `swap_in`, and a pull asks before it downloads
  anything. The refusal names the machines and leaves the version as it
  was. There is no force flag: the way past is to remove the clones or to
  install under another version. Tests:
  `an_overwriting_import_is_refused_while_a_clone_is_backed_by_the_version`
  (`src/supervisor/store.rs`),
  `an_overwriting_pull_is_refused_while_a_clone_is_backed_by_the_version`
  (`src/oci/client.rs`) and
  `an_overwrite_asks_the_replace_guard_and_a_refusal_keeps_the_version`
  (`src/template/store.rs`). All three fail with the guard not consulted
  (the install succeeds) and pass with it.
  - Not fixed here: F1 (`add_variants`) is a separate path.

### F3. One repository can fill the store slot another reference resolves to (security)

- Where: `src/oci/client.rs:636-645` (the store name, arch and version come
  from the config blob, checked only for path safety by
  `src/template/trust.rs:6-28`), `:730` and `:899` (installed under them),
  `:1127-1144` (`cached_registry_template` looks up the last component of the
  repository path and the tag), `:1166-1170` (a hit ends the resolve offline),
  `src/template/store.rs:239-256` (`resolve_version` reads no `origin`).
- Why it is a fault: a pull of `ghcr.io/evil/anything:1` installs under
  whatever `name` and `version` its config blob states, for example
  `ubuntu` and `24.04.99`. Nothing compares them with the repository path or
  the tag. A later reference `ghcr.io/vmlabdev/ubuntu:24.04` is looked up as
  store name `ubuntu`, version pin `24.04`. `resolve_version_pin`
  (`src/template/store.rs:1088-1101`) takes the highest `24.04.<N>`, which is
  the hostile entry, and `ensure_registry_template` returns it without a
  request to the named registry. A local reference `x86_64/ubuntu` resolves
  to it too. The recorded `origin` says where the entry came from, and no
  reader checks it. The user then boots a disk from one publisher in the
  belief that it is another's, and the lab's provisioning hands it the lab's
  secrets. `trust::scripts` keeps a first-boot script out, since the hostile
  repository is not in `trusted_template_repositories`, but the disk is the
  guest.
  The reverse also holds: once the slot is taken, a pull of the real
  reference is refused with "already in the store with a different disk"
  (`src/oci/client.rs:753-758`).
- Proof: client test `a_cached_registry_reference_is_not_served_from_another_repository`
  in `src/oci/client.rs`: with the fake transport, serve at repository
  `evil/x` a template whose config blob says `name = "ubuntu"`,
  `version = "24.04.9"`. Pull `localhost:5000/evil/x:1`. Then call
  `cached_registry_template("localhost:5000/good/ubuntu:24.04", "x86_64", &store)`.
  Expect `None`. Today it returns the entry whose `origin` is
  `localhost:5000/evil/x:1`.
- Fixed in card #480. `origin` could not carry the check: a build writes its
  source there, and a source may be a URL or a reference. A pull now records
  the repository it was given, `host/owner/name`, as `pulled_from` in the
  version's `template.wcl`, from the reference and never from the config
  blob. `cached_registry_template` and `ensure_registry_template` take a
  version for a registry reference only when it was pulled from that
  repository or was not pulled at all (a build or an import is the user's
  own). Where another repository's entry is the newest match of a pin, the
  newest entry of the named repository is taken, so the hostile entry cannot
  hide it. A pull that finds its slot holding a version pulled from another
  repository is refused and names that repository, with the same disk too,
  since the held metadata and variants are the other publisher's.
  `ensure_registry_template` returns exactly the version its pull installed,
  where it used a pin resolution that preferred a `<version>.<N>` sub-build.
  Tests: `a_cached_registry_reference_is_not_served_from_another_repository`
  and `a_pull_does_not_use_a_version_another_repository_installed`
  (`src/oci/client.rs`), `round_trip_pulled_from` (`src/template/meta.rs`).
  The two client tests failed before the fix (the lookup returned the entry
  from `evil/x`, the second pull succeeded) and pass after it.
  - Not fixed here: a local reference (`x86_64/ubuntu`) still resolves to the
    newest version of that name whoever published it. It names the store
    slot by design, and `vmlab template list` shows the origin.
  - Not fixed here: a version pulled before this change has no `pulled_from`
    and is treated as the user's own. Pull it again with `--overwrite` to
    record its repository.
  - Not fixed here: a pull still merges variants into a version that was
    built or imported locally when the base disk is the same. An archive
    given to `template import` can state any `pulled_from`.
  - Changed behaviour: the same artefact published under two repositories
    can no longer share one store version. The second pull is refused.

### F4. The config blob's VARS file is decompressed with no limit (security, crash)

- Where: `src/oci/config_blob.rs:276-287` (`zstd::decode_all`),
  `src/oci/client.rs:631-637` (called from `resolve_manifest_meta`),
  `:1554-1564` with `:1231` (the config blob is fetched under the 1 GiB blob
  limit, not the 8 MiB JSON limit).
- Why it is a fault: `resolve_manifest_meta` runs for `resolve_version`, so
  for every `up` of a lab with a registry reference that is not in the store
  yet, and for every `template pull`. It calls `config.vars()` before any
  chunk is fetched and before the result is needed. `vars()` base64-decodes
  `secure_boot_vars` and hands it to `zstd::decode_all`, which grows a `Vec`
  until the frame ends. A zstd frame of zeros expands by more than four
  orders of magnitude, so a config blob of a few MiB asks for hundreds of
  GiB. The process is the lab daemon (`src/labd/lab.rs:808`) or the
  supervisor (`src/supervisor/store.rs:479`). It is killed by the OOM killer,
  and with the lab daemon go the lab's network fabric and control
  connections. A real VARS file is 528 KiB or 64 MiB at most.
- Proof: unit test `an_oversized_vars_payload_is_refused` in
  `src/oci/config_blob.rs`: build a `TemplateConfig` whose
  `secure_boot_vars` is base64 of `zstd::encode_all` over 256 MiB of zeros
  (about 8 KiB of text). Expect `vars()` to return an error. Today it returns
  256 MiB. The size in the test is the only thing that keeps it from taking
  the test process down.
- The same code path reads the config blob with `get_blob`, so a 1 GiB JSON
  document is held in memory, then parsed. A test with the fake transport
  that serves a config blob of 16 MiB should see a refusal.
- Fixed in card #481. `TemplateConfig::vars` reads the zstd frame through a
  reader that stops one byte past `MAX_VARS_BYTES` (64 MiB, the largest real
  VARS file) and refuses a payload that reaches it. `with_vars` refuses a
  larger file on push. Every read of a template's config blob (`index_arches`,
  `tag_version`, `resolve_manifest_meta`, the variant configs of a pull) goes
  through `Registry::config_blob`, which refuses a descriptor that declares
  more than `MAX_CONFIG_BLOB` (8 MiB, the JSON limit) before any request, and
  fetches with `Transport::get_blob_bounded`, so the HTTP transport stops
  reading at that limit whatever the descriptor says. `put_config` refuses
  to publish a config blob over the same limit. Tests:
  `an_oversized_vars_payload_is_refused` (`src/oci/config_blob.rs`) and
  `an_oversized_config_blob_is_refused` (`src/oci/client.rs`). Both fail
  before the fix (`vars()` returns 256 MiB, the pull of a 16 MiB config blob
  succeeds) and pass with it.
  - Not fixed here: the container image config (`src/oci/image/pull.rs:180`,
    `index_platform_arches`) is still read with `get_blob` under the 1 GiB
    limit.

### F5. A template pull does not bound the chunk count or the total size (security)

- Where: `src/oci/client.rs:822-844` (every layer of the manifest is
  downloaded), `:936-966` (`download_layers`), `src/oci/chunking.rs:167-200`
  (`assemble`: 1 GiB per chunk, any number of chunks),
  `src/oci/manifest.rs:152-162` (the chunk-count and total-size annotations
  are never read on pull).
- Why it is a fault: the per-chunk limit is 1 GiB after decompression. The
  manifest may be 8 MiB, which is room for tens of thousands of layer
  descriptors, and they may all name the same small blob. A chunk of zeros
  under `window_log_max(27)` is a few tens of KiB. So a registry that serves
  a few MiB makes the pull write terabytes to `<store>/.oci-pull/op-*/staging`
  on the store's filesystem. The digest check comes after the last byte is
  written (`:888`). A full store filesystem stops every running VM whose
  overlay is on it. `TOTAL_SIZE` and `CHUNK_COUNT` are in the manifest and
  the config blob states the virtual disk size, and none of them is compared
  with what is written or with the free space.
  The container image pull has the same shape: each layer is bounded at
  8 GiB compressed (`src/oci/client.rs:1232`, `:1592`), the layer count is
  not, and the decompressed stream that goes to `sqfstar`
  (`src/oci/image/flatten.rs:156-246`) has no limit at all.
- Proof: client test `a_pull_refuses_more_data_than_its_manifest_declares` in
  `src/oci/client.rs`: with the fake transport, a manifest whose
  `TOTAL_SIZE` says 1 MiB and whose layers are 64 references to one chunk of
  1 MiB of zeros. Expect the pull to fail before it has fetched or written
  more than the declared size (the fake transport counts `get_blob` calls).
  Today all 64 chunks are fetched and 64 MiB is assembled before
  `verify_whole` fails.
- Fixed in card #482. A pull now takes what each part assembles to from the
  manifest and holds the registry to it. Before a chunk is fetched
  (`declared_chunk_sizes` in `src/oci/manifest.rs`): the chunk count
  annotation must equal the layer count, every layer must state its
  uncompressed size, no chunk may state more than 1 GiB, none may be empty
  unless it is the only one, and the sizes must sum to the declared total
  (`TOTAL_SIZE` for the base, `VARIANT_SIZE` and `VARIANT_CHUNK_COUNT` for an
  overlay). The declared bytes, compressed plus assembled, are then compared
  with the free space of the filesystem of the work directory, and the
  assembled bytes with that of the store (`ensure_room` in
  `src/oci/client.rs`). A fetched chunk must have the compressed size its
  descriptor states. `chunking::assemble` takes each chunk's stated size and
  refuses a chunk that decompresses to more or fewer bytes, so the staged
  disk never grows past the declared total.
  Tests: `a_pull_refuses_more_data_than_its_manifest_declares` and
  `a_pull_is_refused_where_the_filesystem_has_no_room` (`src/oci/client.rs`),
  `the_declared_chunk_sizes_are_held_to_the_manifests_totals`
  (`src/oci/manifest.rs`), `decompressed_chunk_limit_is_enforced`
  (`src/oci/chunking.rs`). The first failed before the fix (64 chunks
  fetched, the pull ended at the whole-image digest) and passes after it.
  - Not fixed here: the container image pull. Its layer count and the
    decompressed stream that goes to `sqfstar` are still unbounded (F6 covers
    the flatten).
  - Not fixed here: the declared total is the registry's word. A pull may
    still take all the free space that it finds at its start, as an honest
    large template does. No reserve is kept, and the virtual disk size in the
    config blob is not compared.
  - Changed behaviour: a manifest without the chunk count, the total size or
    a layer's uncompressed size is refused. Every vmlab push has written all
    three.

### F6. The layer flatten is quadratic, unbounded, and cannot be cancelled (security)

- Where: `src/oci/image/flatten.rs:110-139` (`apply_entry`), the three
  `winners.retain` calls at `:121`, `:131`, `:137`, `:67-104`
  (`survey_layers`), `src/oci/image/pull.rs:260-268` (`spawn_blocking`).
- Why it is a fault: every tar entry that is not a directory runs
  `winners.retain` over the whole survivor map (`:137`), to drop a lower
  directory tree at the same path. The survey of a layer with `n` files is
  `n²/2` map visits. An ordinary large image has a few hundred thousand
  files, which is tens of billions of visits. A hostile layer needs no size:
  a tar of several million empty files compresses to a few MiB and keeps one
  blocking thread busy for days. The map itself has no entry limit, so the
  same layer with distinct long names takes the memory of the lab daemon.
  The work runs under `spawn_blocking`, and aborting the pull task
  (`src/labd/lab.rs:712-716`) does not stop a blocking thread. Each retry of
  the `up` adds another.
- Proof: unit test `the_survey_of_a_large_layer_is_linear` in
  `src/oci/image/flatten.rs`: build one uncompressed layer of 200,000 empty
  files in one directory and call `survey_layers`. Expect it to finish in
  under ten seconds. Today it performs about 2 × 10^10 closure calls. A
  second test, `a_layer_with_too_many_entries_is_refused`, states the limit
  that the fix picks.
- Fixed in card #503. The survey keeps the surviving paths as a tree of
  path components (`Survivors` in `src/oci/image/flatten.rs`). A whiteout,
  an opaque marker and a file over a directory write one number on the root
  of the subtree they erase, and visit nothing beneath it. Folding one entry
  in, and asking which layer won a path, costs the length of that path.
  One image is held to `Limits::IMAGE`: 4,000,000 tar entries over all
  layers and as many distinct paths, 256 MiB of path names, and 64 GiB of
  decompressed layer bytes. Past any of them the pull is refused. The pull
  holds a `CancelOnDrop` across the `spawn_blocking` await
  (`src/oci/image/pull.rs`). Aborting the pull task drops it, which sets the
  flag that both passes test at each entry and each read, and `sqfstar` is
  killed when the stream to it fails.
  Tests in `src/oci/image/flatten.rs`:
  `the_survey_of_a_large_layer_is_linear` failed before the fix (the survey
  of 200,000 files took 135 s on helios) and passes after it.
  `a_layer_with_too_many_entries_is_refused`,
  `a_layer_whose_names_take_too_much_memory_is_refused`,
  `layers_that_unpack_to_too_much_are_refused` and
  `a_cancelled_flatten_stops` call what the fix added.
  `the_survivor_tree_agrees_with_a_scan_of_every_path` compares the tree
  with the old map on 2,000 generated layer stacks.
  - Not fixed here: the number of layers in a manifest has no limit of its
    own. The 8 MiB manifest limit bounds it, the three limits above are over
    the whole image, and every layer is still downloaded before the flatten
    refuses anything.
  - Not fixed here: the 64 GiB are not compared with the free space of the
    cache's filesystem.
  - Changed behaviour: an image past one of the limits, which flattened
    before (slowly), is now refused.

### F7. The store accepts a qcow2 with an external data file (security)

- Where: `src/template/store.rs:894-907` (`verify_base`), `:837-883`
  (`qcow2_backing_file`), `:986-1029` (`verify_variants`),
  `src/oci/client.rs:866-870`.
- Why it is a fault: the checks on a pulled or imported disk read one thing
  from the qcow2 header, the backing file name. A base must have none and a
  variant must have `../../disk.qcow2`. The header's incompatible-feature
  bit 2 with the data-file header extension names a second host file that
  holds the guest's clusters. QEMU and `qemu-img` open that file when they
  open the image. A variant overlay that is backed by the real base and maps
  one unused guest cluster range to `/home/<user>/.ssh/id_ed25519` passes
  every check here. The guest, which the same publisher wrote, reads the key
  from its disk and sends it out through NAT.
  The default image tool stops this by accident: `vmlab_disk` refuses the
  bit (`hv/crates/vmlab-disk/src/qcow2/mod.rs:329-332`), and a clone opens
  its backing image first (`hv/crates/vmlab-disk/src/ops.rs:86-102`), so the
  clone fails. With `VMLAB_IMAGE_TOOL=qemu-img` (`src/template/qimg.rs:147-162`)
  and a machine on QEMU, nothing refuses it. I did not run QEMU against such
  an image. The store should refuse it at the boundary where it refuses a
  backing file, whichever tool is selected.
- Proof: store test `a_base_with_an_external_data_file_is_refused` in
  `src/template/store.rs`: write a version-3 qcow2 header with incompatible
  feature bit 2 set and a data-file extension (type `0x44415441`) naming
  `/etc/hostname`, and call `verify_base`. Expect an error. Today it returns
  `Ok`. The same for `verify_variants` with a correct backing name.
- Fixed in card #462. `refuse_qcow2_data_file` in `src/template/store.rs`
  refuses a qcow2 whose header has incompatible-feature bit 2 or a data-file
  header extension, with either image tool. `verify_base`, `verify_variants`
  and the variant pull in `src/oci/client.rs` call it. The variant case has
  its own test, `a_variant_with_an_external_data_file_is_refused`.

### F8. `template login` destroys a docker config it cannot parse (data loss)

- Where: `src/oci/auth.rs:301-340` (`store_login_at`), `:311-318`, `:322`.
- Why it is a fault: an existing `~/.docker/config.json` that does not parse
  as JSON becomes `{}` (`unwrap_or_else(|_| json!({}))`), and so does one
  whose top level is not an object. The new credential is added and the file
  is renamed over the original. Every other registry's credential, every
  `credHelpers` entry and every proxy setting in the file is gone, with no
  message. Docker's own config is the user's, not vmlab's. A file with a
  trailing comma or a comment is enough.
  A file that parses and whose `auths` is not an object (a string, an
  array, a number) reaches `root["auths"][registry] = ...`, and `serde_json`
  panics on indexing a non-object. The supervisor's handler task for
  `registry.login` dies and the CLI gets a closed connection.
  Smaller, same function: the temporary file has a fixed name
  (`config.json.tmp`), so two logins at once write one file. It is created
  by `fs::write` with the default mode and narrowed afterwards, so the
  password is readable by other local users for a moment under a umask of
  022, and stays readable when the existing file was 0644.
- Proof: unit tests in `src/oci/auth.rs`:
  `a_malformed_config_is_not_replaced`: write `{"auths":{"a.example":{"auth":"eDp5"}},}`
  and call `store_login_at`. Expect an error and the file unchanged. Today it
  succeeds and `a.example` is gone.
  `a_non_object_auths_is_an_error_not_a_panic`: write `{"auths":"x"}`. Today
  the call panics.
- Fixed in card #465. `store_login_at` refuses a config that does not parse
  as JSON, one whose top level is not an object, and one whose `auths` is not
  an object. The error names the file and the file is left unchanged. A
  missing file, an empty file and a null `auths` still start from an empty
  object, because nothing is lost there. The temporary file has a name of its
  own and mode 0600 from its creation, so two logins do not share it and the
  written config is 0600 whatever mode the old file had. Both proof tests
  failed before the fix (the first returned `Ok` with the file replaced, the
  second panicked in `serde_json`) and pass after it. A third test,
  `a_login_keeps_the_other_keys_and_writes_a_private_file`, covers the
  smaller half. It failed before on the mode (0644).

### F9. A hard link in a layer loses its target (wrong result)

- Where: `src/oci/image/flatten.rs:194-240` (`stream_survivors`), the link
  branch at `:222-232`, and `:110-139`.
- Why it is a fault: the survey tracks paths. A hard-link entry survives or
  dies by its own path, and its target by the target's path. A hard link
  carries no data. When layer 1 holds file `b` and the link `a -> b`, and
  layer 2 deletes `b` (a whiteout) or replaces it, the merged stream holds
  `a -> b` with no `b` from layer 1. In the whiteout case `sqfstar` gets a
  link to a path that is not in the archive, and the pull fails or `a` is
  missing. In the replace case `b` from layer 2 comes later in the stream
  than `a`, with the same outcome, or `a` gets layer 2's content. In a real
  image `a` keeps the old content. Package managers produce this: two names
  of one binary in one layer, and one name removed in the next.
- Proof: unit test `a_hard_link_survives_the_deletion_of_its_target` in
  `src/oci/image/flatten.rs` (beside the existing tests that run `sqfstar`):
  layer 1 has `b` with content `one` and the hard link `a -> b`. Layer 2 has
  `.wh.b`. Flatten, read the squashfs back, and expect `a` to exist with the
  content `one` and `b` to be absent. A second test replaces `b` in layer 2
  with content `two` and expects `a` = `one`, `b` = `two`.

### F10. The registry HTTP client has no timeout (wrong result)

- Where: `src/oci/client.rs:1287-1291` (`HttpTransport::new`),
  `src/template/artefact.rs:138-141` (the artefact download client). The
  provider client has one (`src/oci/catalog.rs:22-29`).
- Why it is a fault: neither client sets a connect, read or total timeout.
  A registry, a token realm or a mirror that accepts the connection and then
  sends nothing holds the request for ever. `bounded_body` limits bytes, not
  time. For a lab this is an `up` that stays in "pulling" until someone
  cancels it. For `vmlab template pull`, `search` and `login` through the
  supervisor there is no cancel. `next_version`
  (`src/template/build.rs:752-760`) reads the registry's tags before a build,
  so a silent registry also stops a local build from starting. The retry
  logic (`is_transient`, `:1852-1854`) handles a timeout that can never
  occur.
- Proof: client test `a_registry_that_never_answers_times_out` in
  `src/oci/client.rs`: bind a `TcpListener` on loopback that accepts and
  never writes, build `HttpTransport::new("127.0.0.1:<port>", Anonymous)`,
  and call `get_manifest` under `tokio::time::timeout` of the documented
  limit plus a margin. Expect an `Err` from the transport. Today the outer
  timeout fires.
- Fixed in card #505. `HttpTransport` runs under `Limits`: 30 s to connect,
  60 s from a request without a body to its response head, 60 s between two
  reads of a body, and 15 minutes for a request that carries a body. The
  transport keeps the waits itself, because reqwest's `read_timeout` also
  runs through the upload and would cut a slow chunk PUT. A silence is the
  error `NoAnswer`, retried like a network error except after an upload. No
  limit bounds a whole transfer. The artefact download client sets
  `connect_timeout` (30 s) and `read_timeout` (60 s).
  - Proof: `a_registry_that_never_answers_times_out`, with
    `a_body_that_stops_arriving_times_out` and
    `an_upload_outlasts_the_answer_limit` in `src/oci/client.rs`, and
    `a_mirror_that_never_answers_times_out` in `src/template/artefact.rs`.
    The tests inject short limits. `the_default_limits_bound_every_wait`
    holds the limits that `new` uses.
  - Not fixed here: the supervisor's `template pull`, `search` and `login`
    still have no cancel. They now end by themselves on a silent registry.

### F11. Recorded digests are not compared with the content (wrong result)

- Where: pull: `src/oci/client.rs:726-729`, `:888-897` (the config blob's
  `sha256` is kept when present, the manifest's digest is used only when it
  is absent). Push: `:388-393`, `:446-455` (the disk is chunked and its
  digest goes into the manifest, the config blob carries `meta.sha256`).
  Readers of the recorded value: `:1105-1115` (`same_disk`),
  `src/template/store.rs:720` and `:755` (`verify_variants` takes it as the
  base digest).
- Why it is a fault: the manifest's whole-image digest is what the pull
  verifies. The config blob's `sha256` is what the store records. Nothing
  requires the two to be equal. When they differ, the store holds a version
  whose `template.wcl` names a digest that its disk does not have. The next
  pull of the same reference then fails in `same_disk` with "already in the
  store with a different disk", and a variant of that version cannot be
  added, since `verify_variants` compares against the recorded value.
  A push produces exactly this when the store's disk no longer matches its
  metadata (bit rot, a truncated file after a crash, see the note on
  `fsync`): it publishes the damaged bytes under a correct manifest digest
  and the old `sha256` in the config, and every consumer verifies it as
  good.
- Proof: client test `a_config_digest_that_disagrees_with_the_manifest_is_refused`
  in `src/oci/client.rs`: with the fake transport, serve a correct manifest
  and a config blob whose `sha256` is 64 zeros. Expect the pull to fail.
  Today it installs, and a second `pull` of the same reference fails with
  "different disk". Push test `a_push_refuses_a_disk_that_is_not_the_recorded_one`:
  flip one byte of a stored `disk.qcow2`, push, expect an error.
- Fixed in card #504: a pull refuses a config blob whose `sha256` is not
  the manifest's whole-image digest, before it fetches a chunk, and records
  the verified digest. A push refuses a base disk or a variant overlay
  that does not hash to the digest its metadata records, before it uploads
  that disk.

### F12. Two pulls of one template at the same time (wrong result)

- Where: `src/oci/client.rs:742-746` (the store is asked once, at the
  start), `:899`, `src/template/store.rs:392-399`.
- Why it is a fault: two lab daemons that start a lab with the same registry
  reference both find the version absent, both download it, and both call
  `install_with_variants` with `overwrite = false`. The store lock orders
  them. The second finds the directory present and fails with "already
  exists in the store (pass overwrite to replace)". That lab's `up` fails
  although the template it needs was installed a second earlier with the
  same digest. A retry works. The staging directories are separate for this
  case (`:1199-1203`), the install is not.
- Proof: client test `two_pulls_of_one_reference_both_succeed` in
  `src/oci/client.rs`: a fake transport whose `get_blob` waits on a barrier
  for two callers, two `ensure_registry_template` calls joined. Expect both
  `Ok` with the same version. Today one is `Err`.

### F13. A cancelled pull removes the work directory under a running install (wrong result)

- Where: `src/labd/lab.rs:702-716` (`cancel_pull` aborts the task),
  `src/oci/client.rs:1201-1207` (the `TempDir` lives in the aborted future),
  `:855-905` (the install runs under `spawn_blocking` inside that
  directory), `src/template/store.rs:367-409` (`install_with`).
- Why it is a fault: aborting the task drops the future, and the future owns
  the work directory, so `remove_dir_all` runs on it at once. The blocking
  thread that assembles, hashes and installs from `staging/` inside that
  directory keeps running. The comment at `src/labd/lab.rs:708-711` says a
  cancel during the install "still leaves a fully verified template behind".
  In most interleavings the install fails on a missing file, which is
  harmless. In one it does not: `install_with` checks that `disk.qcow2` is a
  file (`:375-379`), writes `template.wcl`, and renames the directory. If the
  removal deletes `disk.qcow2` or a variant overlay between the check and
  the rename, the store gets a version directory with metadata and no disk.
  `installed_as_is` then answers "is corrupt: missing disk.qcow2"
  (`:296-300`) for every later resolve and pull of that version, until
  someone removes it with `--force`.
  The comment at `:705-706`, "cleaned up by the next successful pull", is
  also untrue. See F14.
- Proof: store test `a_staged_disk_removed_before_the_rename_installs_nothing`
  in `src/template/store.rs`, with the `InstallOps` seam that the swap tests
  use: a `write_meta` that deletes `staging/disk.qcow2` and then writes the
  metadata. Expect `install_with` to fail and the version to be absent.
  Today the rename succeeds and `installed()` returns the "corrupt" error.
- Fixed in card #521: the blocking thread that installs holds the owner of
  the work directory (`Registry::holding`), so a cancelled pull removes it
  only when the install has ended (client test
  `a_cancelled_pull_keeps_its_work_directory_until_the_install_ends`).
  `install_with` also checks the staged disks again after it writes the
  metadata and after the rename, and takes out a version that arrived
  without one. The untrue comment on `cancel_pull` is corrected. The sweep
  of work directories that a killed process leaves is F14.

### F14. Staging directories are never swept (wrong result)

- Where: `src/template/store.rs:1301-1324` (`.staging-<rand>`),
  `src/template/oci_bridge.rs:36-39` (`<store>/.oci-pull/op-*` and
  `cache/oci-push/op-*`), `src/oci/image/cache.rs:274-297`
  (`<oci cache>/.staging-<rand>`).
- Why it is a fault: each of these is removed by a `Drop`. A process that is
  killed, that exits through `process::exit`, or that loses power runs no
  `Drop`. A pull that was half way holds the compressed chunks and part of
  the assembled disk, tens of GiB for a Windows template, and nothing ever
  removes them: `subdirs` skips dot names so the store does not even list
  them. Build work directories have a lock and a sweep at supervisor start
  for exactly this reason (`src/template/build.rs:985-1008`). The store, the
  pull and push work directories and the image cache have neither.
- Proof: test `a_leftover_pull_work_dir_is_swept` beside
  `sweep_build_workdirs`' tests: create `<store>/.staging-deadbeef/disk.qcow2`
  and `<store>/.oci-pull/op-dead/chunks/base/chunk-0000.zst`, run the
  supervisor's start-up sweep, expect both gone. Today no code path removes
  them. The sweep must not take a directory that a live pull holds, which
  the test also states.

### F15. The artefact cache: a shared `.part` file and an unchecked file name (wrong result)

- Where: `src/template/artefact.rs:31` (the cache path is
  `<sha256>.artefact`), `:38-46`, `:130-159` (`download` writes
  `<sha256>.part` and renames it), `src/config/extract.rs:1144-1152` (the
  `sha256` of a URL source is any string).
- Why it is a fault, three parts:
  1. Two builds that use the same URL at the same time (two templates from
     one ISO) both open `<sha256>.part` with `File::create`. The second open
     truncates the file the first is writing. Each then writes at its own
     offset into one inode. The first to finish renames it and verifies a
     file with a hole, and fails with "hash mismatch". The second fails at
     its rename.
  2. `download` renames the file to its final, digest-named path before
     `verify_file` runs. A file that fails the check stays in the cache
     under the name of a digest it does not have. The uncompressed path
     re-verifies on a hit, so the cost is a wasted download each time. It is
     still a cache whose names do not mean what they say.
  3. The `sha256` attribute is used as a file name with no check that it is
     64 hex digits. `sha256 = "../../../x"` in a template definition makes
     the download write the URL's content to `<data>/x.artefact` and leave it
     there after the hash check fails.
- Proof: unit tests in `src/template/artefact.rs`:
  `two_resolves_of_one_url_both_succeed` (a loopback server that serves the
  body slowly, two `resolve` calls joined, expect both `Ok`).
  `a_download_that_fails_its_hash_leaves_no_cache_entry`.
  `a_url_source_needs_a_64_digit_hex_sha256` in the config tests: a
  definition with `sha256 = "../x"` is a validation error.

### F16. A push onto a tag that holds a plain manifest drops its architecture (wrong result)

- Where: `src/oci/client.rs:551-557` (`put_arch_manifest`).
- Why it is a fault: the module text (`:12-17`) and the function's own
  comment (`:526-529`) say a plain manifest under the tag is promoted into
  an index. The code starts an empty index in that case
  (`ManifestOrIndex::Manifest(_) => ImageIndex::new()`). The architecture
  that the plain manifest served is gone from the tag after the push of the
  second architecture. vmlab itself always writes an index, so this needs a
  tag written by another tool or by a client that predates the index.
  The merge is also a read, change and write with no precondition, so two
  architectures pushed to one tag at the same time can each write an index
  without the other. I list that under the notes, since no test without a
  real registry shows it.
- Proof: client test `a_push_onto_a_plain_manifest_tag_keeps_its_architecture`
  in `src/oci/client.rs`: put a plain aarch64 template manifest at tag `1.0`
  in the fake transport, push x86_64 to `1.0`, expect `index_arches("1.0")`
  to hold both. Today it holds `x86_64` only.

### F17. The flatten drops PAX extended attributes (wrong result)

- Where: `src/oci/image/flatten.rs:216-238`.
- Why it is a fault: the merged stream is rebuilt entry by entry from
  `entry.header()` with `append_data` or `append_link`. A layer stores file
  capabilities and other extended attributes in the PAX records before the
  entry (`SCHILY.xattr.security.capability`). The `tar` reader folds those
  records into the entry and the builder does not write them again. The
  squashfs then has no extended attributes. A binary that relies on a file
  capability instead of setuid (`ping` in several distributions, some web
  servers binding port 80) fails for a non-root user in the container.
  The same rebuild turns a non-UTF-8 name into replacement characters
  (`to_string_lossy`, `:275`), so two such names can become one.
- Proof: unit test `a_file_capability_survives_the_flatten` in
  `src/oci/image/flatten.rs`: a layer with one file and a PAX record
  `SCHILY.xattr.security.capability`, flattened, read back with
  `unsquashfs -lls -xattrs` or an equivalent reader. Expect the attribute.
  Today it is absent. Drop this finding if `sqfstar` on the gate host does
  not read PAX attributes at all. Then the fault is in the choice of tool.

### F18. A 404 on a later page of a tag list returns an empty list (wrong result)

- Where: `src/oci/client.rs:1728-1731` (`list_tags`).
- Why it is a fault: the 404 branch is for a repository that does not exist
  yet. It is inside the page loop and returns `Vec::new()`, which also
  discards the tags of the pages before it. A registry whose `Link` cursor
  has expired answers 404 on the next page. The caller then sees a
  repository with no tags: `next_version` restarts the build counter at the
  local maximum (`src/template/build.rs:752-781`), and `pull_tag`
  (`:303-312`) resolves a version pin as if nothing were published.
- Proof: client test `a_404_on_a_later_tag_page_is_an_error` in
  `src/oci/client.rs`, with the loopback HTTP server the transport tests
  use: page one returns two tags and a `Link` header, page two returns 404.
  Expect an error, or the two tags. Today the result is an empty list.

### F19. Strings from a registry reach the terminal unfiltered (cosmetic)

- Where: `src/oci/client.rs:1746-1750`, `:1798-1802` (tags and repository
  names as the registry sends them), `src/template/catalog.rs:105-169`
  (they become the rows of `template search`, and the `version` of a row is
  the config blob's own, `src/oci/client.rs:283-289`).
- Why it is a fault: a tag, a repository name or a version may hold any
  bytes that JSON can carry, among them ESC. `vmlab template search` prints
  them. A hostile registry can move the cursor, recolour or overwrite lines
  of the table, for example to show its own entry under another namespace's
  name. Template names that enter the store are checked
  (`src/template/trust.rs:6-18`). Names that are only listed are not.
- Proof: catalog test `a_tag_with_a_control_character_is_not_listed` in
  `src/template/catalog.rs`: a fake registry whose tag list holds
  `"1.0\u001b[2J"`. Expect the row to be left out or the character to be
  escaped in `CatalogSearchRow`. Today it is passed through.

## Notes

These have no test that proves them on the gate host, or are design choices
that the lead should know about.

- N1. No `fsync` anywhere on the install path. `chunking::assemble`
  (`src/oci/chunking.rs:167-188`), the tar unpack, the sealed image and
  `template.wcl` are written, and the directory is renamed
  (`src/template/store.rs:460-483`), with no `sync_all` on a file or on the
  parent directory. After a power loss the rename can be durable while the
  disk's data is not. The store then serves a version whose disk is short or
  zeroed. Nothing verifies a disk at use, and `same_disk` trusts the
  recorded digest, so a later pull does not repair it. The same holds for
  `write_meta_atomic` (`:812-819`) and the docker config.
- N2. The template pull does not check the digest of the manifest that an
  index entry names, nor of the config blob (`src/oci/client.rs:607-611`,
  `:631-635`). The variant path does (`fetch_manifest`, `:917-932`), and so
  does the container image pull (`src/oci/image/pull.rs:158-161`,
  `:181-184`). The registry controls the tag anyway. The gap matters when a
  blob GET is redirected to another host, which may be plain HTTP
  (`:1422-1425`).
- N3. Sums of sizes that the registry states can overflow:
  `Download::new` (`src/oci/client.rs:1067`), `done` (`:1081`), the layer sum
  (`src/oci/image/pull.rs:218`), and `next_subbuild`'s `n + 1`
  (`src/template/store.rs:1075`) on a tag `1.0.18446744073709551615`. In a
  debug build these panic. In a release build they wrap, and the build
  counter restarts at 0.
- N4. `quote` (`src/template/meta.rs:830-845`) escapes five characters. The
  free-text fields of a config blob (`profile`, `firmware`, `display`,
  `cpu_model`, `origin`, `registry`, `agent_version`, a login's `user` and
  `password`, a variant's `description`) may hold any other control
  character. If the WCL lexer refuses one of them inside a string, the pull
  installs a `template.wcl` that cannot be read. `list` skips the version,
  and `installed` fails for it. I did not find out what the lexer accepts. A
  pull should read its own metadata back before the rename.
- N5. An imported archive's `template.wcl` is evaluated as a full WCL
  document with a loader that reads the disk
  (`src/template/meta.rs:424-447`, `disk_loader` at `:435`), before any check. I did not read what
  the loader and the WCL built-ins permit.
- N6. `import` has no size limits: the zstd stream, the entry sizes and
  `template.wcl` (read whole into memory) are unbounded
  (`src/template/store.rs:660-705`, `:909-966`). It also holds the store
  lock for the whole unpack and both hashes, so every other store change
  waits, and a read that finds a leftover swap waits too (`:333-344`).
- N7. `remove` and `prune` scan for clones and take the store lock
  afterwards (`src/supervisor/store.rs:213-236`). A clone made between the
  two is not seen.
- N8. Concurrent pushes of two architectures to one tag read, merge and
  write the index with no precondition (`src/oci/client.rs:551-582`). One
  can overwrite the other's entry.
- N9. `put_blob_file` reads a whole chunk into memory and `put_blob` clones
  it for each attempt (`src/oci/client.rs:1614-1654`), so a push holds twice
  the chunk size, 1 GiB at the default.
- N10. `Reference::parse` (`src/oci/reference.rs:30-76`) accepts any
  characters in host, repository and tag. They go into request paths as
  written. `image/reference.rs` validates all three. A repository name from
  a registry's catalog reaches `Registry::new` this way
  (`src/template/catalog.rs:110`).
- N11. `first_boot` and `secure_boot_vars` of a template definition are
  joined to the template root with no check (`src/template/build.rs:1387-1392`,
  `:1413-1416`), so a definition can embed any readable file in metadata
  that a push then publishes.
- N12. A template's metadata sets the default `cpus` and `memory` of its
  clones. A pulled template can state any value. I did not check what
  validation caps them.
- N13. `registries.json` is read, changed and written with a fixed
  temporary name and no lock (`src/template/registries.rs:119-129`).
- N14. A manifest's variant list may name one variant twice
  (`src/oci/manifest.rs:342-369`). The pull then fetches it twice into one
  directory.
- N15. `send_with_auth` gives up after three attempts of any kind
  (`src/oci/client.rs:1440-1482`). Two 5xx answers followed by a 401 fetch a
  token and return the 401 without using it.
- N16. A push that fails with an error text that contains `401` anywhere,
  for example inside a digest, gets the "run template login" hint
  (`src/supervisor/store.rs:568-571`).

## What I read and what I did not

Read in full, without their test modules:

- `src/oci/client.rs`, `chunking.rs`, `manifest.rs` (from `layers_in_order`
  on), `config_blob.rs`, `reference.rs`, `auth.rs`, `catalog.rs`, `mod.rs`.
- `src/oci/image/pull.rs`, `cache.rs`, `flatten.rs`, and the parser of
  `reference.rs`.
- `src/template/store.rs`, `trust.rs`, `oci_bridge.rs`, `artefact.rs`, and
  the WCL writer of `meta.rs`.
- `src/supervisor/store.rs` (remove, prune, export, import, pull, push,
  search, login).

Read in part:

- `src/template/build.rs`: the entry, version numbering, work directories
  and their sweep, the variant install, sealing and the install into the
  store. Not the boot and provisioning of the build VM (about 1,300 lines).
- `src/template/qimg.rs`: the tool selection and the argument lists.
- `src/template/catalog.rs`: `fetch_search_row`.
- `src/supervisor/backing.rs`: `backing_chain` only.
- `src/labd/lab.rs`: `pull_template`, `bind_template`, `cancel_pull`.
- `hv/crates/vmlab-disk`: only the feature-bit check and
  `create_linked_clone`, for F7.

Not read:

- `src/template/cli.rs`, `store_view.rs`, `bootstrap.rs`, `agent_install.rs`,
  `registries.rs` beyond its file write, `src/template/meta.rs`'s parser and
  variant selection, `src/oci/image/model.rs`, `src/oci/media_types.rs`,
  `src/supervisor/templates.rs`.
- The qcow2 parser of `vmlab-disk`, which reads every pulled disk. A crafted
  template reaches it. It belongs to a review of its own.
- Every test module. A finding here may already have a test that I did not
  see. The named tests are new names.
- The `fuzz/` targets for registry documents.
