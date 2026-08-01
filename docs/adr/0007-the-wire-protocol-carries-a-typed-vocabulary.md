# ADR-0007: The wire protocol carries a typed vocabulary and error codes

- **Status**: Accepted, implemented
- **Date**: 2026-07-31
- **Related**: [ADR-0004](0004-lab-status-is-a-typed-projection.md)
- **Implements**: `src/proto/vocab.rs`, `src/proto/error.rs`;
  [the generated protocol reference and coverage report](../protocol.md)

## Context

The daemon protocol is stringly typed end to end. A request is a command string
plus an untyped argument value, and the client offers a single generic call
taking both. There is no request enumeration and no typed client method, so
nothing links any two hops of an operation at compile time.

"Start a machine" is therefore spelled independently at four layers: twice
verbatim in the CLI (once for each machine noun), once in the daemon's dispatch
match, once as a path-segment-to-command map in the web layer, and once more as
a string union in the console — which then wraps it twice, once per machine
noun. Commit `3117cff` halved the wire vocabulary from twenty-four commands to
twelve and its message names the cost directly: each pair was "restated in the
CLI, in the REST layer, and again in the console's `api.ts`. Adding one
capability meant eight edits."

Because nothing enumerates the protocol, coverage gaps are invisible:

- Eleven commands are reachable only from the CLI; six only from the console.
- One implemented command has no caller anywhere — not in the CLI, not in the
  web layer, not in the console.
- Commit `465494b` added a CLI verb for a command the wire had served "since the
  command surfaces collapsed. It was simply the half nobody wrote."

Errors have the same problem in a sharper form. A response carries an optional
message and no code, so the web layer classifies HTTP status by substring
matching on the daemon's error prose — checking for "already running", "no
such", "invalid lab name" and so on. Rewording any daemon error silently changes
an HTTP status code.

## Decision

**The protocol carries a typed request vocabulary and structured error codes.
Surfaces adapt that vocabulary; they do not re-spell it.**

Concretely:

- Requests become an enumeration in the protocol module, carrying their argument
  shape. The command string remains the serialised form — this is not a wire
  format break — but callers construct requests through the enumeration.
- The client exposes typed constructors rather than only a generic call.
- Responses carry a machine-readable error code alongside the human-readable
  message. The web layer maps code to HTTP status. Message prose becomes free to
  change.
- The CLI and the console are adapters over the one vocabulary. Neither holds a
  private list of command strings.

**Depth condition.** An enumeration that merely restates thirty-five command
strings is shallow — it would add an interface as wide as the implementation
behind it and buy nothing. This decision is conditional on the enumeration
carrying the argument shape *and* the error code. If implementation finds that
the argument shapes are too heterogeneous to model usefully, the honest outcome
is to keep the string and take only the error-code half, and this ADR should be
amended to say so.

## Consequences

**Gained**

- Missing verbs become visible: a surface that does not handle a request variant
  is a compile error or an explicit, greppable omission rather than silence.
- Dead commands are findable.
- HTTP status stops depending on error prose, which removes an entire class of
  invisible coupling between the daemon's wording and the web layer's contract.
- Leverage: one vocabulary, three adapters — CLI, REST, console.

**Given up**

- The protocol module gains a dependency on the argument shapes of every
  command, which couples it more tightly to the lab daemon than a bare string
  does.
- Adding a command touches the enumeration as well as the handler. That is the
  intended cost — it is what makes the omissions visible — but it is a cost.

**Watch for**

- The enumeration becoming a pass-through with one variant per string and an
  untyped argument value on each. That is the shallow outcome the depth
  condition above rules out; if it happens, revert to the string and keep the
  error codes.

## Outcome

The depth condition held: every argument shape modelled, so no variant carries
an untyped value and the string half was not needed as a fallback. Two
vocabularies rather than one — `SupRequest` and `LabRequest` — because there are
two daemons, and one enumeration per daemon is what makes each dispatch an
exhaustive `match`.

The first coverage report found what this ADR predicted: `machine.agent_info`
had no caller on any surface, and was removed rather than wired up — the
features it reported are already in `machine.capabilities`. The surface
asymmetries it lists are left as they are; closing them is separate work, per
gap.

Making the asymmetries visible turned out not to be enough on its own: the
report could say a command was reachable from one surface but not whether that
was a decision, so each pass over the list re-derived the same classification.
A variant may now carry `#[one_way("surface", "why")]` beside its doc comment,
which reaches its `CommandSpec` and is rendered next to the command in the
report. Annotation is optional — an unannotated command means nobody has
decided — but an annotation is checked: it must name a real surface, and the
command must still be reachable from that surface alone.

Its counts are larger than the "eleven CLI-only and six console-only" above,
which came from a hand count of the lab socket. The report covers both sockets
and treats the REST layer as the console's reach, so it lists seventeen and
sixteen. The shape of the finding is the same; the report is the number to
trust, because it is regenerated rather than counted.

Two argument names moved to match the glossary while the contract was being
written down for the first time: `up`/`pull`/`down` take `machines`, and
`snapshot.take`/`snapshot.restore` take `machine`. Both keep the old spellings
as serde aliases, so nothing that spoke `vms`/`vm` breaks.
