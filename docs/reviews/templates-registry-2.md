# Second review: templates, registry and OCI pull and push

Card #914. A second bug-hunting pass over template store, build install,
import and export, the OCI registry client (pull, push, login, auth), the
container image pull and its layer flatten. It lists faults and makes no fix.
Reviewed at `next` commit `ab017e1b`. All paths are from the repository root,
and all line numbers are from that commit.

The first pass is `docs/reviews/templates-registry.md` (card #440, findings F1
to F19, notes N1 to N16). This document continues its numbering, so a code
names one finding across both: F20 to F25 and N17 to N24 are new here. A
finding of the first pass is named again only where its fix is incomplete or
brought a new fault in, and the text says which. The "Not fixed here" items
the first pass already lists under its findings (an archive's `pulled_from`,
variants merged into a local version, the container image config read under
the 1 GiB limit, the container layer count) are known and not repeated.

Nothing here was run. Every finding is from reading the code, and each one
names the test that would prove it. A finding whose test fails to reproduce
is wrong and should be dropped. No test below uses a wall-clock limit: where
an order of events matters, the test makes the events in that order itself.

Severity words are those of the first pass: **data loss**, **crash**,
**security**, **wrong result**, **cosmetic**. The threat model is the first
pass's: a registry is hostile once a lab or a command names it, an archive
given to `vmlab template import` is crafted by a stranger, and a lab file is
trusted as much as a Makefile is. A **local user** is another account on the
same host.

## Findings, worst first

| Code | Severity | Fault in one line |
|------|----------|-------------------|
| F20 | data loss | The replace guard of an overwriting pull or import reads the clones once, before the operation, so a clone made during a long pull is replaced under (F2 fix incomplete) |
| F21 | security | A pulled template keeps the `registry` its config blob states: `template list` shows it as the template's name, `--remote` asks it, and `template push` without a target uploads there |
| F22 | wrong result | A variant pulled into a held version is turned into overrides against the registry's base metadata, then laid over the held base's, so its effective metadata is neither |
| F23 | security (local user) | `vmlab template login` takes the password only as a command-line argument |
| F24 | wrong result | `template login` replaces a symlinked docker config with a regular file |
| F25 | wrong result | The start-up sweep leaves the artefact cache's `.part` files and the store's `.template.wcl.<n>` temporaries, which a killed process leaves (F14 fix incomplete) |

### F20. The overwrite replace guard reads the clones before the operation (data loss)

- Where: `src/supervisor/store.rs:491-501` (`guarding_replace` runs
  `clones_of(labs)` once and hands the store a closure over that list),
  `:533` (import), `:551` (pull), `src/oci/client.rs:891` (the pull asks
  the guard before the download), `src/template/store.rs:521-523` (the
  install asks it again under the store lock).
- Why it is a fault: the F2 fix says the store asks the guard under its lock
  at the moment it replaces the version. It does ask, but the guard does not
  look again. Its answer is the clone list read when the command started. A
  `template pull --overwrite` of a Windows template downloads and assembles
  tens of GB before the install runs, and an import unpacks and hashes the
  whole archive first. A lab that runs `up` in that window makes a linked
  clone of the version that is still installed. The install then asks the
  old list, finds no clone, and `swap_in` deletes the disk the new clone is
  a delta against. The clone is corrupt at its next start, the failure F2
  describes. A lab registered after the command started is not scanned at
  all. The in-use check of the F1 fix (`in_use_check`, `:464-471`) reads the
  labs when it is asked, so the two guards of one pull behave differently.
- Proof: supervisor test
  `an_overwriting_import_asks_about_the_clones_at_the_install` in
  `src/supervisor/store.rs`, built like
  `an_overwriting_import_is_refused_while_a_clone_is_backed_by_the_version`
  (`:942`): import the first archive, register the lab with no clone, call
  `guarding_replace(store, true, &labs)`, then create the linked clone of the
  installed disk, then call `import_trusting_scripts(second, true, false)` on
  the returned store. Expect a refusal that names `corp/dc01`. Today the
  import succeeds and the installed disk changes.
- Severity: data loss, on a path that needs `--overwrite`. The window is the
  length of the download or the unpack, minutes to hours.

### F21. The `registry` of a config blob names a pulled template (security)

- Where: `src/oci/config_blob.rs:257` (`into_meta` keeps the blob's
  `registry`), `src/oci/client.rs:712-716` (the pull overrides `origin` and
  sets `pulled_from`, and leaves `registry`),
  `src/template/store_view.rs:166` (the summary carries it),
  `src/template/cli.rs:708-711` (`name_of` prints `registry` in the TEMPLATE
  column in place of the name), `src/supervisor/store.rs:166-168` with
  `:192-203` (`--remote` asks the registry it names, with the credential of
  that host), `:588` (`push` uses it when no target is given).
- Why it is a fault: the F3 fix keeps a hostile repository from filling
  another repository's slot, and its text says that `vmlab template list`
  shows where a version came from. The table shows `registry`, which the
  config blob chooses. A pull of `ghcr.io/evil/x:1` whose blob states
  `registry = "ghcr.io/vmlabdev/vmlab-templates/ubuntu"` is listed under the
  trusted name. `template list --remote` asks that registry and reports
  `published` when it holds the version, which confirms the false name.
  `vmlab template push x86_64/ubuntu@<version>` with no target pushes the
  hostile disk to the repository the blob named, with the user's own
  credential for that host, and moves its `latest` tag. A user who mirrors
  a pulled template "back to its registry" does exactly that. The value also
  reaches the terminal without the filter the F19 fix gave `template search`.
- Proof: client test `a_pull_records_the_repository_it_pulled_from_as_the_registry`
  in `src/oci/client.rs`: with the fake transport, serve at
  `localhost:5000/evil/x` a template whose config blob states
  `registry = "ghcr.io/vmlabdev/vmlab-templates/ubuntu"`, pull it, read the
  installed version. Expect `meta.registry` to be `None` or
  `localhost:5000/evil/x`. Today it is the blob's string. Every reader named
  above takes the value from there.
- Severity: security. The push half needs the user to push without a target.

### F22. A pulled variant's overrides are taken against the wrong base (wrong result)

- Where: `src/oci/client.rs:982` (`into_variant(&meta)`, where `meta` is the
  base metadata of the registry's config blob), `src/oci/config_blob.rs:178-206`
  (a field is an override only where the variant's effective value differs
  from that base's), `src/oci/client.rs:1094-1104` and
  `src/template/store.rs:950-974` (the block is written into the held
  version's `template.wcl`, whose base fields are the held ones).
- Why it is a fault: a variant's config blob carries its effective metadata,
  and `into_variant` keeps a field only where it differs from the base it is
  given. A pull into a version the store already holds gives it the
  registry's base, then lays the result over the held base. Where the two
  bases differ, the variant's effective metadata in the store is the held
  base's value for every field the registry's variant did not change. The
  held base differs when the same disk was published again with other
  metadata (a re-push after a vmlab upgrade changes `agent_version` or
  `wscript_surface`), and when the held version was built or imported
  locally (the first pass's F3 notes that such a version still takes
  variants). `first_boot_script`, `cpus`, `memory` and `secure_boot` are
  among the fields, so a clone of the variant can run a first-boot script
  the publisher's variant does not have, or boot with other hardware.
- Proof: client test `a_variant_pulled_into_a_held_version_keeps_its_published_metadata`
  in `src/oci/client.rs`: install a version whose base records `cpus = 2`,
  serve the same disk with a base config blob stating `cpus = 4` and a
  variant `v` whose config blob states `cpus = 4`, pull `#v`. Expect
  `store.installed(..).select(Some(v), Lab).meta.cpus == Some(4)`. Today it
  is `Some(2)`.
- Severity: wrong result.

### F23. `template login` takes the password only on the command line (security, local user)

- Where: `src/template/cli.rs:179-185` (`password: String`, a required
  `--password` argument), `docs/manual/pages/commands/template.wcl:548`,
  `:567` (the manual shows `-p "$GHCR_TOKEN"`).
- Why it is a fault: a process's arguments are readable by every local user
  through `/proc/<pid>/cmdline` for as long as the CLI runs, which includes
  the round trip to the registry, and they go into the shell's history. The
  token is a registry credential with push rights. `vmlab app` already reads
  a password from standard input (`src/cli/app.rs:52`, `--password-stdin`),
  and the supervisor protocol carries the password without argv, so only the
  CLI surface is missing.
- Proof: CLI test `template_login_reads_the_password_from_stdin` in
  `src/template/cli.rs`: parse `vmlab template login ghcr.io -u me
  --password-stdin`. Expect it to parse. Today clap refuses it, and refuses
  any form without `--password`.
- Severity: security against a local user. Low.

### F24. `template login` replaces a symlinked docker config (wrong result)

- Where: `src/oci/auth.rs:314` (the config is read through the link),
  `:347-366` (the new content is written to a temporary file beside the
  link and renamed over the link itself).
- Why it is a fault: a `~/.docker/config.json` that is a symlink, as a
  dotfile manager makes it, is read through the link, and the rename then
  replaces the link with a regular file. The target keeps its old content
  without the new credential, and the copy that now stands in the link's
  place no longer follows edits made to the target. The F8 fix made the
  write private and per login. It did not keep the link.
- Proof: unit test `login_writes_through_a_symlinked_config` in
  `src/oci/auth.rs`: write a config to `target.json`, make `config.json` a
  symlink to it, call `store_login_at(config.json, ...)`. Expect
  `config.json` to be a symlink still and `target.json` to hold the new
  entry. Today `config.json` is a regular file and `target.json` is
  unchanged.
- Severity: wrong result. Low.

### F25. The start-up sweep misses two kinds of leftover (wrong result, F14 fix incomplete)

- Where: `src/supervisor/mod.rs:243-263` (`sweep_leftover_staging` sweeps four
  roots), `src/template/artefact.rs:110-124` (each download writes
  `.<sha256>.<pid>-<n>-<nanos>.part` in `cache/artefacts`, up to 32 GiB),
  `src/template/store.rs:1029-1036` (`write_meta_atomic` writes
  `.template.wcl.<n>` in a version directory).
- Why it is a fault: the F15 fix gave each artefact download a file of its
  own and says that a file left by a killed process waits for F14. The F14
  fix sweeps the store's staging directories, the pull and push work areas
  and the image cache, and not the artefact cache. Before F15 a killed
  download left `<sha256>.part`, which the next download of that artefact
  truncated and reused. Now each killed download leaves a new file under a
  name no later download uses, so the leftovers add up. A supervisor stopped
  by a signal during an ISO download is enough. The temporary file of
  `write_meta_atomic` (a variant removal, an `add_variants`) is left the same
  way when the process is killed between the write and the rename. It is
  small, and it stays in a version directory that `remove` deletes.
- Proof: supervisor test `the_startup_sweep_removes_abandoned_artefact_downloads`
  beside the test of `sweep_leftover_staging`: create
  `<data>/cache/artefacts/.<64 hex>.1-0-000000000.part` and a published
  `<64 hex>.artefact`, run the start-up sweep, expect the first gone and the
  second kept. Today both stay.
- Severity: wrong result. The cost is disk space on the filesystem that
  holds the store.

## Notes

These have no test that proves them on the gate host, or are design choices
that the lead should know about.

- N17. `store_login_at` reads, changes and writes the docker config with no
  lock (`src/oci/auth.rs:314-366`). A `docker login` or a second
  `template login` between the read and the rename loses its entry. The F8
  fix closed the shared temporary file, not this.
- N18. The flatten's emit pass reads each layer file a second time and does
  not hash it (`src/oci/image/flatten.rs:417`, `LayerStream::verified` at
  `:577-584`). The survey verified the `diff_id`, and the emit pass trusts
  that the file has not changed since. The files are hard links in the
  pull's own staging directory, so only a process of the same user can
  change them. Hashing the emit stream too and comparing would close it.
- N19. Before it pulls, `ensure_registry_template` resolves the registry's
  real version as a pin (`src/oci/client.rs:1503`, `held_for` at
  `:1435-1458`, `resolve_version_pin` at `src/template/store.rs:1377-1390`).
  A `<real>.<N>` sub-build in the store that was built locally is then
  preferred to the exact version the registry tag names. After the pull the
  exact version is taken (`:1520-1532`), so the two resolutions differ.
- N20. A challenge's `realm` is fetched whatever its host and scheme
  (`src/oci/client.rs:1896-1946`). The Basic credential is kept from a
  foreign realm, but the request is sent, so a hostile registry makes the
  supervisor or a lab daemon send a GET to any URL, a loopback or
  link-local address included. Docker does the same. A host that runs other
  services on loopback may want a limit.
- N21. A token fetched after a 401 from a URL that is not the registry (an
  absolute upload `Location`, a redirect, a `Link` page) is cached under the
  registry's scope (`src/oci/client.rs:1879-1881`). The next request to the
  registry carries the foreign token, gets a 401 and fetches a new one, so
  it costs one attempt of `MAX_RETRIES`.
- N22. A catalog search resolves every repository the catalog lists, up to
  `MAX_LIST_PAGES` pages of 1000 (`src/oci/client.rs:2201-2250`,
  `src/template/catalog.rs:52-60`). Each costs a tag list, manifests and a
  config blob, so a hostile catalog makes one `template search` issue
  hundreds of thousands of requests. Each is bounded, the count is not.
- N23. Each chunk of a template pull is read whole into memory under the
  1 GiB blob limit (`src/oci/client.rs:1172-1176`, `MAX_BLOB_BODY` at
  `:1556`), and its stated compressed size is compared only after the last
  byte. `get_blob_bounded(.., layer.size)` would stop at the declared size.
- N24. The variant config blob's `base_sha256` is not compared with the
  base's whole-image digest on pull (`src/oci/client.rs:992-996` compares the
  name and the overlay digest). The layout's `base_digest` is compared, so a
  blob that disagrees installs a variant that `is_stale` then refuses, with
  a message that says to rebuild it rather than that the publisher's
  metadata is inconsistent.

## First-pass fixes I checked

I read the commit of every first-pass fix against the code at `ab017e1b`.

- F1 (`551bddae`): the held-variant path asks the in-use check before the
  download and again under the store lock in `add_variants`. Correct as far
  as I read.
- F2 (`0183472f`): incomplete, see F20.
- F3 (`c8326757`): correct for pulls. Its remaining gaps are those its own
  text lists, plus F21, where the name it says the list shows is not the one
  shown.
- F4 (`27ab24d7`), F5 (`e4f4f0df`): bounds hold in `config_blob`,
  `declared_chunk_sizes` and `assemble`. N23 is a remaining memory cost, not
  a gap in the bound.
- F6 (`e71febef`), F9 (`6ef7e3a7`), F17 (`2b477a8c`): the survey tree, the
  limits, the cancel flag, the hard-link aliasing and the xattr pass-through
  read correctly. I did not find out whether `sqfstar` decodes the base64
  value of a `LIBARCHIVE.xattr.*` record, which the flatten forwards as it
  is. N18 is the one gap I found.
- F7 (`532f337d`): the data-file bit and extension are checked on the base
  and every variant, on pull and import.
- F8 (`690c5baa`): a config that does not parse is left alone. See F24 and
  N17 for what remains.
- F10 (`8c0238b0`), F11 (`4747897b`), F12 (`7fca84a9`), F13 (`b6b19159`),
  F16 (`0412d49f`), F18 and F19 (`3556f8a6`): correct as far as I read. F19
  covers `template search` only (F21).
- F14 (`0dfc9076`): incomplete, see F25. The directory locks of
  `StagingDir` keep a live operation's directory from the sweep.
- F15 (`ad0a512f`): correct. Its leftovers are F25.

## What I read and what I did not

Read in full, without their test modules:

- `src/oci/client.rs`, `config_blob.rs`, `chunking.rs` (assemble and
  verify), `manifest.rs` (from `whole_digest` to `ImageIndex`), `auth.rs`,
  `reference.rs`, `mod.rs`.
- `src/oci/image/pull.rs`, `flatten.rs`, `cache.rs`, `model.rs`.
- `src/staging.rs`, `src/template/oci_bridge.rs`, `trust.rs`,
  `artefact.rs`, `registries.rs` (its file handling), `catalog.rs`.
- `src/template/store.rs` up to the test module.
- `src/supervisor/store.rs` up to the test module, and the start-up sweep in
  `src/supervisor/mod.rs`.

Read in part:

- `src/supervisor/templates.rs`: the operation registry and the build start.
- `src/template/cli.rs`: login, import, export, pull, search and list.
- `src/template/meta.rs`: variant selection, `effective`, `from_wcl`, the
  display and login checks, `quote`. Not the extractor.
- `src/oci/catalog.rs`: the provider listings and their budgets.
- `src/template/build.rs`: the work directory claim and the build lab name.
- `src/labd/lab.rs`: `cancel_pull`, `pull_template`, `bind_template`.
- `src/qemu/resolve.rs` and `src/qemu/cmdline.rs`: what `firmware` and
  `cpu_model` from a template become.

Not read:

- `src/template/bootstrap.rs`, `agent_install.rs`, `store_view.rs` beyond
  its summary fields, `qimg.rs` beyond `image_info` and the clone argument
  lists, and the rest of `build.rs`.
- The WCL lexer, so whether a control character from a config blob survives
  `quote` and the reader (the first pass's N4) is still open, and with it
  whether F21's value can carry a terminal escape.
- The qcow2 parser of `vmlab-disk`, the `fuzz/` targets, and every test
  module. A finding here may already have a test that I did not see. The
  named tests are new names.
