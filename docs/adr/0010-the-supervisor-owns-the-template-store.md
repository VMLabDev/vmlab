# ADR-0010: The supervisor owns the template store, and every surface asks it

- **Status**: Accepted
- **Date**: 2026-08-01
- **Related**: [ADR-0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md)

## Context

PRD §3 gives the supervisor "serialised writes to the template store (pulls,
builds, imports — so concurrent labs can't corrupt it; reads are lock-free)".
The code did not match. `src/template/cli.rs` held no reference to the protocol
client: all eleven `vmlab template` verbs opened the store, dialled registries
and ran builds in the CLI process, while the console reached templates through
seven lab-scoped `template.*` supervisor commands.

This was never a correctness problem. The store takes an exclusive `flock` on
every mutation, so the two paths cannot corrupt each other however many
processes run at once. The cost was duplication and divergent capability: a
build started from a terminal was invisible to the console and could not be
stopped from anywhere else, got no structured progress, and reproduced logic
the supervisor already had.

The genuine question was whether `vmlab template build` should acquire a
dependency on a running supervisor. It works today with no supervisor at all, which
is a real property. Against it: two implementations of build and push, no
cancellation, no shared visibility, and an ownership claim in the PRD that the
code contradicts.

## Decision

**The supervisor is the only thing that opens the template store or dials a
registry. Every `vmlab template` verb is a protocol client.**

Concretely:

- Store-scoped and registry-scoped operations get their own namespaces,
  `store.*` and `registry.*`. The existing `template.*` commands are
  lab-scoped — each takes `lab` and `root`, and `template.list` means "the
  templates *this lab declares*" — and keep that meaning. Reusing them for a
  store-wide list beside a lab-declared one would make the vocabulary lie about
  what it addresses.
- **Reads route too, and stay lock-free.** "Reads are lock-free" is about
  locking, not routing. A read command takes no lock; it routes so that there
  is one path to the store, not because it needs serialising.
- A CLI build goes through the existing `template.build`, with the file's
  directory as the root and the lab that file declares as the lab. It is the
  same operation the console starts, claimed in the same registry, so the two
  surfaces see and can stop each other's work. `template.list` and
  `template.build` gained optional arguments to make that possible — a `file`,
  because a shell may point at any template file, and a `version` pin. The
  brief asked that `template.*` "stay exactly as they are"; this is the one
  place that was not kept literally, because the alternative was a second build
  command, which is the duplication the record exists to remove. Both arguments
  default to what the console already sends, so its payloads are untouched, and
  a test pins that.
- **Stopping is a verb, not only an interrupt.** `vmlab template stop` cancels
  an operation this terminal did not start — the half of "either surface can
  stop the other's" that an interrupt handler cannot reach.
- A CLI push is `store.push`, not `template.push`. It addresses a store
  reference rather than a lab's declaration, and carries a target and the git
  origin of the directory it was run in — none of which a lab declaration has.
- **Long operations report through `template.op.*` events**, the mechanism the
  console already renders. A terminal reads that stream as text; there is no
  second progress mechanism.
- **Interrupting the CLI cancels the operation** rather than detaching it, and
  the CLI follows the operation to its end afterwards so the supervisor has
  finished clearing up before the process exits. A build that should outlive a
  terminal belongs to the console. A second interrupt gives up waiting and
  says so: tokio's handler has replaced SIGINT's default, so if this did not
  honour it the user would have no way out of an upload that will not die.
- **A supervisor restart fails in-flight builds and pushes.** There is no
  resumption. This is the in-process failure model reproduced: the process
  dies, the workdir guard removes everything. The guard is a `Drop`, which a
  killed process does not run, so the supervisor also sweeps the build
  directory at startup — the one moment it is certain to own no build.
- **The answers are typed values, not hand-built JSON** (`template/store_view.rs`,
  the shape ADR-0004 settled on for lab status). The supervisor builds them and
  the CLI decodes them, so a renamed field stops the far side compiling instead
  of rendering a confident zero.
- Failures of the new commands answer with `ErrorCode::Failed`. These replaced
  in-process work whose every failure exited 1, and `Failed` is the code that
  still exits 1; classifying them more finely would silently change the exit
  code of every existing script. A wrongly *shaped* request is still an
  `invalid_argument`, answered by the wire decoder before a handler runs.

## Consequences

**Gained**

- One implementation of build, push and pull. `src/supervisor/store.rs` is
  where the store is opened; `src/template/cli.rs` is presentation.
- A build or push started from a terminal appears in the console's operation
  status, and either surface can stop the other's.
- The CLI gets cancellation and structured progress for nothing — it reads the
  event stream the console already had.
- Build working directories stop surviving a killed supervisor, which on a
  multi-gigabyte template was the sharpest edge of the old failure model.
- The `template.*` group stops being an unexplained one-way block in the
  coverage report: four commands now have two callers, and the three that do
  not carry their reason.

**Given up**

- `vmlab template build` now needs a supervisor. It is auto-started like every
  other verb, so a user sees no difference, but the property "this command
  needs no daemon" is gone.
- A relative path in `export`, `import` or `-f` has to be absolutised before it
  is sent: the supervisor is not standing where the caller is, and would
  resolve the same string against its own working directory.
- Adding a store verb now touches the vocabulary, the supervisor and the CLI.

**Watch for**

- A verb that needs something only the caller has — its cwd, its git remote,
  its terminal — being tempted back into the CLI wholesale. The rule is that
  the CLI reads its own surroundings and sends them; it does not act on the
  store with them.
- `store.*` growing lab-shaped arguments. If a store command starts needing to
  know which lab is asking, it is a `template.*` command wearing the wrong
  namespace. `store.push` carries a `lab` deliberately and only so a console
  watching that lab sees the operation — it does not change what is pushed.

## Outcome

Six of the eleven CLI verbs mapped onto new store-scoped commands, three onto
registry-scoped ones, and build onto the console's own `template.build`. The
one genuinely unclear mapping the brief flagged — build — resolved toward reuse
because the shared operation registry is the point: a store-scoped build would
have been a second implementation of the thing this record exists to remove.

Push went the other way for the same reason: it addresses a store reference and
carries provenance a lab declaration cannot supply, so sharing the lab-scoped
command would have meant bending both.

Three `template.*` commands remain reachable only from the console —
`template.remote`, `template.op_status` and `template.console_path` — and each
now carries a recorded reason rather than sitting bare in the report. The
`store.*` and `registry.*` commands are reachable only from the CLI, because
the console has no store-wide view to hang them on; giving it one is a separate
decision from putting the operations on the protocol.
