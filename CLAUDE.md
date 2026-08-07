# CLAUDE.md

Project context for Claude Code.

## Project Purpose

**vmlab** is a VM lab management tool written in Rust. This is a fresh
rewrite; the product requirements live in `docs/vmlab-prd.md` — read that first;
it is the source of truth for design and scope.

Many earlier attempts are archived under
github.com/wiltaylor/.graveyard-private — notably `vmlab_qemu` (QMP/QGA
driver crate), `vmlab_oci` (OCI registry client for VM disk images), and
`vmlab_floppy` (pure-Rust FAT for floppy images), all buried 2026-06-12.
Consult them for prior art only — the PRD overrides anything they did.

## Status

PRD implemented (M1–M6). **§19 (Dev machines) is partly built** — the
workspace syncer and the managed `~/.ssh/config` block are spec only;
implementation is tracked by #78–#98. The `@dev` declaration (#80) and all
three agent vocabularies (§19.5) are built: `tunnel` (#85), `watch` (#86) and
`fileops` (#84) — the handle-based, offset-addressed, pipelined file RPC
session that **replaced** the whole-file push/pull pair outright, carrying
`vmlab cp`, the console's transfer, wscript push/pull and tree pushes. So is
machine-level `login {}` (#81) on both guest families: the **Windows logon it
mints** (#82) — the wire's per-open `logon`,
`LogonUser`/`LoadUserProfileW`/linked-token minting, the (account, secret,
machine) cache and `exec`/`shell`'s `--user`/`--password` — and the **Linux
session** (#83): `su -l` where the guest has PAM, `setuid` where it does not,
plus the container floor. **The SSH facade serves a shell** (#87,
§19.3/ADR-0012): `machine.ssh_open` + `vmlab ssh-proxy`, `none` auth over a
label selector, and the `session` vocabulary onto agent terminals and execs.
**`direct-tcpip` rides the agent tunnel** (#89), so `ssh -D`/`-W` reach a
guest port with no guest network involved and a failed dial answers
`SSH_OPEN_CONNECT_FAILED` rather than the prohibited code. **`subsystem sftp`
is answered host-side** (#88, `ssh/sftp.rs`): version 3, transcoded
packet-for-request onto `fileops` under the connection's own logon, so `scp`
and an editor's file explorer land on the same cached logon as the shell.
**`attachable` and the failure ladder** (#90, §19.4): `tunnel && fileops`,
reported by `vmlab machine capabilities` and carried in the status projection;
silent at `validate`, a warning at `up`, hard at attach, and the facade
degrading per channel — a stale agent still serves a shell, while the two
channels that need what it lacks refuse by name. With it, `vmlab machine
repair-agent`, which pushes the host's shipped agent into a running machine and
marks it **diverged**; never automatic, and meaningless on a container, which
it says.
Module map under `src/`:

- `config/` — WCL schema, typed model, §5.1 validation, host config, profiles;
  `projection.rs` reflects `schema.wcl` into the Schema projection (ADR-0005)
  and `designer.rs` renders the console's inspector forms from it.
- `dev.rs` — dev machines (§19.1): who carries `@dev`, which one is the lab's
  default, and the `@dev` > profile > floor resolution of its arguments —
  deliberately separate from the hardware resolver ADR-0008 owns.
- `attach.rs` — `attachable` (§19.4) and the words every rung of its failure
  ladder says: the one derivation over probed agent features, the refusal that
  names both remedies, and the warning `up` prints. Nothing here is called by
  `validate`, deliberately.
- `profiles/` — guest OS profiles (WCL data, user-overridable).
- `qemu/` — hardware resolution (VM>template>profile), cmdline builder,
  firmware lookup, process management; `container.rs` builds the micro-VM
  argv for lab containers (§18).
- `qmp/` — the QMP client.
- `template/` — store, qemu-img, builds, artefact cache, registry catalog
  search; `cli.rs` is the `vmlab template` surface, a pure protocol client
  since the supervisor took ownership of the store (ADR-0010).
- `oci/image/` — standard container-image pull: docker/OCI manifests,
  layer flatten (whiteouts → squashfs via sqfstar), digest-addressed cache.
- `guest_asset.rs` + `guest/` — the container micro-VM kernel/initramfs:
  `vmlab-cinit` (guest PID 1), `cinit-proto` (host↔init contract, shared
  crate), `build-asset.sh` (pinned Alpine, rootless build).
- `agent_asset.rs` + `guest/agent`, `guest/agent-proto` — `vmlab-agent`, the
  in-guest agent on the `vmlab.agent.0` virtio-serial port: interactive
  terminals (PTY/ConPTY), streaming exec, tail, metrics, clipboard, the
  handle-based file RPC session every transfer runs over (`fileops.rs`,
  §19.5), guest-side TCP tunnels (`tunnel.rs`, §19.5) and the recursive
  tree `watch` backing the workspace syncer (`watch/`, §19.5) — only a
  tunnel's payload touches the guest network. `spawn.rs` is the one seam
  every guest process is created through, and that a file session borrows
  its identity from (ADR-0015); each platform half mints §19.2's declared
  logins behind it:
  `windows/logon.rs` a token and a loaded profile, `linux/login.rs` a real
  login (PAM via `su -l`, else `setuid`) and the container floor. Baked into
  templates by
  `template/agent_install.rs`; spawned by cinit inside container micro-VMs;
  `labd/vm_agent.rs` is the host-side client; `build-agent.sh` builds the
  per-target binaries (musl + windows-gnu).
- `media/` — folder → ISO/floppy with content-addressed cache.
- `vision/` — screenshot, template matching, OCR.
- `net/` — userspace fabric: frame codecs, L2 switch, DHCP, DNS, gateway,
  NAT engine, L3 rules.
- `proto/` — JSON-lines daemon wire protocol (client + server). `vocab.rs`
  holds the typed request vocabulary every surface constructs through
  (ADR-0007) and `error.rs` the error codes replies carry; `report.rs`
  generates `docs/protocol.md` and `web-ui/src/protocol.ts` from them
  (`just proto-generate`).
- `supervisor/` — `vmlabd`: lab registry, global segments, watchdogs;
  `templates.rs` runs lab-scoped builds/pushes for the console and `store.rs`
  the store- and registry-scoped operations behind every `vmlab template` verb
  (ADR-0010).
- `labd/` — per-lab daemon: lifecycle, snapshots, network assembly, events,
  SMB integration, the lab runtime the wscript host binds to;
  `container.rs`/`container_ctl.rs` run OCI containers as micro-VMs (§18);
  `ssh/` is the SSH facade vmlab terminates on the host (§19.3, ADR-0012) —
  no guest runs an sshd, and its refusals follow ADR-0013's invariant;
  `agent_repair.rs` is `machine.repair_agent` (§19.4), the plan for replacing
  a guest's agent binary over its own channel and the divergence it records.
  Decisions the runtime used to make mid-flight are values computed before
  execution (ADR-0003): `plan.rs` (wave ordering), `share_plan.rs` (share
  transports, gateway rules, the smbd port), `forward_plan.rs` (every port
  forward), `pull_ledger.rs` (deferred download lifecycle).
- `scripting/` — wscript host module (lab/VM/segment API), provisions, handlers.
- `smb/` — bundled-smbd shared folders; `steps.rs` owns the guest-side mount
  plan (virtiofs + SMB, per guest OS) the lab runtime only executes.
- `oci/` — OCI registry push/pull (chunked, multi-arch).
- `cli/` — the `vmlab` verb surface.
- `web/` — the `vmlab-web` binary (Actix-web): REST + WebSocket API over the
  proto client, an embedded SolidJS console UI (`web-ui/`, rust-embed), live
  noVNC over a `vnc.sock` WebSocket bridge, and username/password auth. Behind
  the optional `web` feature; the crate also exposes a `[lib]` so this binary
  reuses `proto`/`paths`/`cli`.

`docs/vmlab-prd.md` remains the binding contract; section refs (`§N`) appear
throughout the code and commit messages.

## Conventions

- Changes land on a branch and merge to `main` via a pull request.
- **just** as command runner: `just build` / `just test`, and `just ci::check`
  for the merge bar — the `ci` module (`.just/ci/mod.just`) is the single
  definition of what a change must pass, and `ci.yml` calls its recipes rather
  than inlining equivalents. Justfile follows the norms in the justfile skill
  (groups, doc comments, `[private]`, noun-verb naming, the norm-14 gate).
- Standard Rust toolchain: `cargo build`, `cargo test`, `cargo clippy`,
  `cargo fmt`.

## Agent skills

### Issue tracker

Issues live in GitHub Issues on `VMLabDev/vmlab`, driven via the `gh` CLI.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name
(`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`,
`wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root.
See `docs/agents/domain.md`.
