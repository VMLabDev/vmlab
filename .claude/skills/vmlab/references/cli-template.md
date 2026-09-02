# vmlab template

`vmlab template` manages the template store and registry distribution (see
templates.md): building templates a file declares, listing and pruning what the
store holds, moving templates through archives, and pushing and pulling them
over OCI registries. Every subcommand is a request to the supervisor
(`vmlabd`), which owns the store and serialises writes to it. The CLI starts
the supervisor when it is not running, parses the flags, asks, and renders the
answer. A build or push started here is the same operation the supervisor runs
for anyone else, so it can be stopped from another terminal.

```sh
vmlab template <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `build` | Build templates defined in a lab or template file. |
| `stop` | Stop a build or push running for a template, whoever started it. |
| `list` | List templates in the store. |
| `search` | Search a registry for published templates. |
| `rm` | Remove one template version. |
| `clean` | Prune superseded builds, keeping the latest per template. |
| `export` | Export a template to a portable archive. |
| `import` | Import a template from an archive. |
| `push` | Push a template to an OCI registry. |
| `pull` | Pull a template from an OCI registry. |
| `login` | Log in to an OCI registry. |
| `registry` | Manage the OCI namespaces `search` looks in. |
| `-h`, `--help` | Print help. |

Store references are spelled `<arch>/<name>[@<version>]`. Without a version the
reference resolves to the newest version in the store. Registry references are
spelled `ghcr.io/owner/name[:version]`.

Exit status is 0 on success. Failures from the supervisor carry the protocol
error codes: `conflict` (5) when an operation is already running for the
template, `not_found` (4) when a template or a running operation does not
exist, `invalid_argument` (2) when a name is ambiguous, and `failed` (1) for a
store or registry operation that was attempted and did not succeed. A failure
before any request is sent, such as a missing file or a supervisor that does
not start, exits 1. Each subcommand below names the codes it can produce.

## vmlab template build

```sh
vmlab template build [OPTIONS] [NAME]
```

| Option | Meaning |
| --- | --- |
| `[NAME]` | Build only the named template. Default: every template in the file. |
| `-f`, `--file <FILE>` | File containing `template {}` blocks. Default: the `vmlab.wcl` of the lab the shell is in. |
| `--version <VERSION>` | Pin an explicit version instead of auto-incrementing. Requires a single target template. |
| `-h`, `--help` | Print help. |

`build` asks the supervisor which templates the file declares, filters them by
`NAME` when one is given, and starts one build per target, in file order. A
relative `--file` is made absolute against the shell's directory before it is
sent, because the supervisor does not stand where you do. The build is filed
under the lab the file belongs to, so a console watching that lab sees it. A
bare template file with no `lab` block is filed under the store itself.

The command subscribes to the supervisor's event stream before asking, then
prints every `template.op.log` line as it arrives until the operation reports
done, cancelled, or an error. One Ctrl-C sends a stop request and keeps
following until the supervisor has cleaned up. A second Ctrl-C exits
immediately and warns that the build may still be running.

It refuses locally, with exit status 1, when the file declares no `template {}`
block, when `NAME` matches nothing in the file, and when `--version` is given
with more than one target. The supervisor answers `conflict` when a build or
push is already running for that template, `not_found` when the named template
is not in the file it loaded, and `invalid_argument` when the name is declared
for more than one architecture and none was chosen. A build step that fails
ends the stream with an error and exit status 1.

```sh
cd examples/templates
vmlab template build win2022
vmlab template build -f templates.wcl --version 1.4.0 alpine
```

Exit status is 0 on success, `conflict` (5), `not_found` (4),
`invalid_argument` (2), or 1.

## vmlab template stop

```sh
vmlab template stop [OPTIONS] <NAME>
```

| Option | Meaning |
| --- | --- |
| `<NAME>` | Template name, as the file declares it. |
| `--arch <ARCH>` | Architecture, when the name is declared for more than one. Default: `x86_64`. |
| `-h`, `--help` | Print help. |

`stop` cancels the operation running for the template in the lab the shell is
in, whether it was started from this terminal, another one, or the console. A
build and a push are stopped by different requests, so the command first asks
to stop a build. If the supervisor answers `conflict`, the running operation is
a push, and the command sends the push stop instead. On success it prints
`stopping <arch>/<name>`; the terminal that started the operation sees it end
as cancelled.

```sh
vmlab template stop win2022
```

Exit status is 0 on success. `not_found` (4) means nothing is running for that
template in this lab. Any other failure is `failed` (1).

## vmlab template list

```sh
vmlab template list [OPTIONS]
```

| Option | Meaning |
| --- | --- |
| `--json` | Emit a JSON array of template metadata instead of a table. |
| `--remote` | Also ask each template's registry whether this exact version is published. Adds a `REMOTE` column. Requires network access. |
| `-h`, `--help` | Print help. |

The table has columns `ARCH`, `TEMPLATE`, `VERSION`, `SIZE`, and `CREATED`. The
`TEMPLATE` column shows the template's full registry path when its metadata
names one, and the bare store name otherwise. An empty store prints `no
templates in the store`. With `--remote` the extra column reads `yes` when the
version is published, `no` when the registry is reachable and does not have it,
`local` when the template names no registry, and `?` when the registry
reference could not be asked. Under `--json` the remote answer is folded into
each entry as a `remote` field.

```sh
$ vmlab template list
ARCH     TEMPLATE                          VERSION          SIZE     CREATED
x86_64   ghcr.io/vmlabdev/win2022          1.2.0            9.8G     2026-08-30
x86_64   alpine                            0.3.1            180.0M   2026-08-12
```

Exit status is 0 on success and `failed` (1) when the store cannot be read.

## vmlab template search

```sh
vmlab template search [OPTIONS] [QUERY]
```

| Option | Meaning |
| --- | --- |
| `[QUERY]` | Case-insensitive substring to match the template name. Default: everything. |
| `--registry <REGISTRY>` | Registry namespace to search. Default: every configured namespace, which starts as the vmlab registry. |
| `--arch <ARCH>` | Only show templates that have this architecture. |
| `--kind <KIND>` | Search VM templates or container images: `vm` or `container`. Default: `vm`. |
| `--json` | Emit a JSON array instead of a table. |
| `-h`, `--help` | Print help. |

`search` reads the catalog of one namespace, or of every namespace `template
registry list` shows, and prints `TEMPLATE`, `ARCH`, and `VERSION` per match. A
namespace that fails while others answer is reported as a warning on stderr
beside the results. When every namespace fails the search itself fails. No
match prints how many namespaces were searched.

```sh
vmlab template search win --arch x86_64
vmlab template search --kind container nginx --json
```

Exit status is 0 on success, including no match, and `failed` (1) when no
namespace answered.

## vmlab template rm

```sh
vmlab template rm [OPTIONS] <REFERENCE>
```

| Option | Meaning |
| --- | --- |
| `<REFERENCE>` | The version to remove, `<arch>/<name>[@<version>]`. |
| `--force` | Remove even if it backs existing clones. |
| `-h`, `--help` | Print help. |

`rm` removes one exact store version and prints `removed
<arch>/<name>@<version>`. A version that backs a machine's clone is refused
unless `--force` is given, because the clone reads its base image from the
store. Removing such a version with `--force` breaks every machine cloned from
it.

```sh
vmlab template rm x86_64/win2022@1.1.0
```

Exit status is 0 on success and `failed` (1) when the reference does not
resolve or the version backs a clone.

## vmlab template clean

```sh
vmlab template clean [OPTIONS] [FILTER]
```

| Option | Meaning |
| --- | --- |
| `[FILTER]` | Limit to a family: `<arch>/<name>`, `<arch>/` for every name in an architecture, or `<name>` for that name in any architecture. Default: every template. |
| `--keep <KEEP>` | Most-recent builds to keep per template, by version order. Default: 1. |
| `-y`, `--yes` | Actually delete. Without it the command only prints what it would remove. |
| `--force` | Also remove builds that still back existing clones. |
| `-h`, `--help` | Print help. |

`clean` groups the store by `<arch>/<name>`, keeps the newest `--keep` versions
of each, and lists the rest. Without `--yes` it is a dry run: each line reads
`would remove ...` and the summary says to re-run with `--yes`. With `--yes`
each line reads `removing ...` and the summary reports the count and the space
freed. A build that backs a clone is listed as `skipping ... backs a clone (use
--force)` and left alone unless `--force` is given. A store with nothing past
the keep count prints `nothing to clean`.

`--keep 0` is refused locally: use `rm` to remove a specific version.

```sh
vmlab template clean                 # dry run, every family, keep 1
vmlab template clean x86_64/ --keep 2 --yes
```

Exit status is 0 on success and `failed` (1) when the store cannot be read or a
removal fails.

## vmlab template export

```sh
vmlab template export <REFERENCE> <OUT>
```

| Option | Meaning |
| --- | --- |
| `<REFERENCE>` | The version to export, `<arch>/<name>[@<version>]`. |
| `<OUT>` | Output archive path, a `.tar.zst` file. |
| `-h`, `--help` | Print help. |

`export` writes one store version, disk and metadata, to a portable archive the
supervisor creates at `OUT`. A relative `OUT` is resolved against the shell's
directory. It prints `exported to <OUT>` when done.

```sh
vmlab template export x86_64/alpine@0.3.1 alpine-0.3.1.tar.zst
```

Exit status is 0 on success and `failed` (1) when the reference does not
resolve or the archive cannot be written.

## vmlab template import

```sh
vmlab template import [OPTIONS] <ARCHIVE>
```

| Option | Meaning |
| --- | --- |
| `<ARCHIVE>` | An archive written by `template export`. |
| `--overwrite` | Overwrite the version if the store already has it. |
| `-h`, `--help` | Print help. |

`import` reads a template back out of an archive into the store and prints
`imported <arch>/<name>@<version>`. The version comes from the archive's
metadata. A version the store already holds is refused unless `--overwrite` is
given.

```sh
vmlab template import alpine-0.3.1.tar.zst
```

Exit status is 0 on success and `failed` (1) when the archive cannot be read or
the version already exists.

## vmlab template push

```sh
vmlab template push [OPTIONS] <REFERENCE> [TARGET]
```

| Option | Meaning |
| --- | --- |
| `<REFERENCE>` | Local template `<arch>/<name>[@<version>]`. |
| `[TARGET]` | Registry repository, for example `ghcr.io/owner/name`. Default: the template's own `registry` field. |
| `--source <SOURCE>` | Source repository URL to link the package to. Default: the git `origin` remote of the current directory, when it normalises to a web URL. |
| `--prerelease` | Publish as a pre-release: move the `latest-prerelease` tag instead of `latest`. |
| `-h`, `--help` | Print help. |

`push` uploads one store version to a registry as an OCI artefact, chunked, and
moves a floating tag onto it: `latest` by default and `latest-prerelease` with
`--prerelease`. The version tag is always written. The command follows the
push's log the way `build` does and prints `pushed <arch>/<name>@<version> to
<target> (moved <tag>)` at the end, with the source link when one was sent.
Ctrl-C stops the push.

The registry needs credentials from `template login` first. A template with no
`registry` field and no `TARGET` is refused with `no push target`. A push
already running for the template is refused with `conflict`.

```sh
vmlab template push x86_64/win2022 ghcr.io/vmlabdev/win2022
```

Exit status is 0 on success, `conflict` (5) when a push or build is already
running, and `failed` (1) for a missing target, a refused upload, or a
cancelled push.

## vmlab template pull

```sh
vmlab template pull [OPTIONS] <TARGET>
```

| Option | Meaning |
| --- | --- |
| `<TARGET>` | Registry reference, for example `ghcr.io/owner/name:version`. |
| `--arch <ARCH>` | Architecture to pull. Required when the reference is a multi-architecture index. |
| `--overwrite` | Overwrite the version if the store already has it. |
| `-h`, `--help` | Print help. |

`pull` downloads a published template into the store and prints `pulled
<arch>/<name>@<version> into the store`. A reference without a tag pulls
`latest`. A multi-architecture index with no `--arch` is refused, because the
store keys templates by architecture. `vmlab up` and `vmlab pull` pull a
machine's template on demand, so this verb is for fetching ahead of time or for
a template no lab file names yet.

```sh
vmlab template pull ghcr.io/vmlabdev/alpine:0.3.1 --arch x86_64
```

Exit status is 0 on success and `failed` (1) when the registry refuses, the
architecture is missing, or the version already exists without `--overwrite`.

## vmlab template login

```sh
vmlab template login --username <USERNAME> --password <PASSWORD> <REGISTRY>
```

| Option | Meaning |
| --- | --- |
| `<REGISTRY>` | The registry host, for example `ghcr.io`. |
| `-u`, `--username <USERNAME>` | The account name. |
| `-p`, `--password <PASSWORD>` | The password or token. |
| `-h`, `--help` | Print help. |

`login` stores credentials for a registry host with the supervisor, which uses
them for every later push and pull against that host. It prints `logged in to
<REGISTRY>`. The password is taken from the command line, so on a shared
machine pass a token with limited scope rather than an account password.

```sh
vmlab template login ghcr.io -u wil -p "$GHCR_TOKEN"
```

Exit status is 0 on success and `failed` (1) when the credentials cannot be
stored.

## vmlab template registry

```sh
vmlab template registry <COMMAND>
```

| Subcommand | Meaning |
| --- | --- |
| `list [--json]` | List the configured namespaces and what each is used for. |
| `add <NAMESPACE> [--use-for <USE_FOR>]` | Add or update a searchable namespace. `--use-for` is `vms`, `containers`, or `both`. Default: `both`. |
| `remove <NAMESPACE>` | Remove a searchable namespace. |
| `-h`, `--help` | Print help. |

The namespaces are what `template search` looks in when no `--registry` is
given, and each is marked for VM templates, container images, or both. `list`
prints `NAMESPACE` and `USE` columns. `add` prints `added <namespace>` and
replaces the entry when the namespace is already configured. `remove` prints
`removed <namespace>`.

```sh
vmlab template registry add ghcr.io/vmlabdev --use-for vms
vmlab template registry list
```

Exit status is 0 on success and `failed` (1) when the namespace file cannot be
read or written.
