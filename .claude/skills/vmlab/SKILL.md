---
name: vmlab
description: "Complete vmlab reference — the lab file, wscript, the CLI, templates, networking, containers, dev machines. Read before writing or editing a `vmlab.wcl` or a `.ws` script, running any `vmlab` command, or answering a question about how vmlab behaves."
allowed-tools:
  - Bash
  - Read
disable-model-invocation: false
---

# vmlab

<overview>
vmlab is a declarative QEMU/KVM VM-lab orchestrator. A lab — machines plus the
virtual networks between them — is declared in WCL in a `vmlab.wcl` file and
brought up with one command. Disk templates are built locally and distributed
over OCI registries. OCI containers join a lab as micro-VM machines. Guest
automation is written in wscript, which drives power, exec, files, keystrokes,
screen matching and OCR. Any machine can be marked `@dev` and serves an editor
over a host-terminated SSH facade. A supervisor daemon plus one daemon per lab
run behind the CLI, which is the only front end.

`references/` holds the whole product reference. It is self-contained: the
answer is in these files, so read the one that covers the question rather than
recalling vmlab behaviour or reading vmlab's source.
</overview>

## Which file answers it

| Question | File |
|----------|------|
| Installing, and the first lab, VM, template or provision end to end | `references/start-here.md` |
| Daemons, processes, guest channels, on-disk paths, the wire protocol and its error codes | `references/architecture.md` |
| The lab file's syntax and evaluation, and the `lab {}` block | `references/lab-file.md` |
| The `vm {}` block and every child block: attributes, types, defaults | `references/vm.md` |
| Containers as lab machines, the micro-VM model, the `container {}` block | `references/containers.md` |
| Segments, DHCP, DNS, NAT, routing, traffic rules, the eBPF fast path | `references/networking.md` |
| Templates, the store, builds, `source {}`, linked clones, scratch VMs, OCI distribution | `references/templates.md` |
| The wscript language: types, control flow, matching, modules, stdlib | `references/wscript-language.md` |
| wscript `Lab`, `Segment` and `Term` methods, and the shared types (`Match`, `ExecResult`, `Login`, `GuestStats`, `Event`) | `references/wscript-lab-api.md` |
| wscript `Machine` methods — lifecycle, exec, files, input, vision, snapshots | `references/wscript-machine-api.md` |
| Provisions, `on "event" {}` handlers, the event catalogue, playbooks | `references/automation.md` |
| `@dev`, `dev attach`, `dev use`, and the workspace syncer | `references/dev-machines.md` |
| `login {}`, minted guest logons, the SSH facade, sftp, `ssh-config` | `references/logins-and-ssh.md` |
| Snapshots, screenshots, keyboard and mouse input, image matching, OCR | `references/snapshots-vision.md` |
| Shared folders, `vmlab cp`, clipboard transfer (the `media {}` block is in `vm.md`) | `references/shares-media.md` |
| The host config file, WSL 2, guest OS profiles | `references/host-profiles.md` |
| Lab-level CLI verbs: `up`, `down`, `status`, `validate`, `destroy`, `pull`, `lab`, `logs`, `eventlog`, `tail`, `dns`, `fastpath`, `playbook`, `script` | `references/cli-lab.md` |
| Per-machine CLI verbs: `vm`, `machine`, `container`, `exec`, `shell`, `console`, `cp`, `clipboard`, `snapshot`, `dev`, `ssh`, `ssh-config` | `references/cli-machine.md` |
| `vmlab template` and its subcommands | `references/cli-template.md` |
| A command that failed, or a message the user is asking about | `references/troubleshooting.md` |
| What a vmlab term means | `references/glossary.md` |
| A worked lab to copy from | `references/examples.md` |

<boundaries>
<always>
- Read the reference file covering the surface before writing WCL or wscript, so
  attribute names, defaults and signatures come from the file rather than memory.
- Run `vmlab validate` after editing a `vmlab.wcl`, before `vmlab up`.
- Write a wscript script (`vmlab script x.ws`) for guest automation of more than
  one step, so the run is one program against the machine handle rather than a
  chain of `vmlab exec` calls.
- Name the reference file an answer came from, so the user can check it.
- Confirm the target lab or template before acting when more than one
  `vmlab.wcl` or store version is plausible.
</always>

<never>
- Run `vmlab destroy` or `vmlab template rm` without the user asking for it in
  this session — both delete state (a lab's clones, a store image).
- State that a WCL attribute, wscript method or CLI flag exists without finding
  it in `references/`.
</never>
</boundaries>

<maintenance>
These files are maintained by hand against vmlab's manual. Inside the vmlab
source tree, `docs/manual/` is where a correction lands first, `docs/vmlab-prd.md`
is the binding contract, `src/config/schema.wcl` is every lab-file attribute
that exists, and `src/scripting/mod.rs` every wscript function.
</maintenance>
