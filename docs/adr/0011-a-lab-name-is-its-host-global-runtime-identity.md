# ADR-0011: A lab name is its host-global runtime identity

- **Status**: Accepted, amended 2026-10-25 and 2026-10-28
- **Date**: 2026-08-04
- **Related**: [ADR-0003](0003-decisions-are-values-computed-before-execution.md),
  [ADR-0007](0007-the-wire-protocol-carries-a-typed-vocabulary.md)

## Context

The supervisor registry, lab runtime directory, control socket and process
markers all address a lab by its declared name. Two roots declaring the same
name therefore do not merely overwrite registry metadata: a command from the
second root can reach the first root's lab daemon, and orphan reaping for one
can kill processes belonging to the other.

## Decision

**A lab's declared name is its host-global runtime identity. Its root is not
the registry key.**

Before `lab.ensure` returns an existing socket or starts a daemon, the
supervisor compares the requested canonical root with the root registered for
that name. A different root is a `conflict` in every registry state. The same
decision is made before `lab.restart` releases anything. The decision itself
is a pure registry operation; filesystem canonicalisation happens before it.

Re-keying the registry, runtime paths, sockets and process markers by root was
rejected.

## Consequences

**Gained**

- Name-keyed sockets, runtime paths and process markers describe one lab
  without another disambiguator.
- A colliding directory cannot operate on, replace or reap the registered lab.
- The caller receives a structured conflict naming the other root and both
  available remedies: stop that lab, or rename this one.

**Given up**

- Two clones or worktrees that declare the same lab name cannot run
  concurrently on one host without changing one declaration.

**Watch for**

- Any future host-global surface that treats a lab name as a display label
  rather than the identity established here.

## Amendment (2026-10-25): lab instances

The decision stands: the name a lab runs under is its host-global runtime
identity, and the supervisor still refuses a second root for a registered
name. What changed is where that name can come from. A directory may run its
lab as an **instance**, under the effective name `<declared>-<instance>`
(`src/instance.rs`). The CLI picks the instance from `--instance <ID>`, then
`VMLAB_INSTANCE`, then the name of the linked git worktree the lab directory
is in; an empty flag or variable means none, and a main worktree, a submodule
or no git has none. Every name-keyed resource follows the effective name:
registry entry, runtime and state directories, sockets, process markers,
generated MACs, guest DNS names and `[lab/]vm` references. The lab daemon
accepts only the declared name or `<declared>-<something>`.

So the **Given up** point no longer holds for worktrees: two linked worktrees
of one repository resolve to distinct effective names and run at once with no
change to either lab file. Two clones outside git worktrees still need an
instance named by hand. Two directories that resolve to the same effective
name still conflict exactly as above, for example `--instance ""` in a linked
worktree while the main worktree runs the declared name.

The instance renames the lab, not its file content. These still collide
between two instances of one lab file, because they are written in the file:

- a fixed `host_port` in a segment `forward {}` or a container `port {}`: the
  second instance's bind fails;
- an explicit `nic { mac }` on a global segment;
- a GPU `passthrough` device;
- template builds declared in the lab file.

Global segments (`global = true`) are shared by design, and both instances
join the same one. Lab-local segment subnets do not collide, because each
lab's network runs in its own userspace process.

## Amendment (2026-10-28): worktree labs built on a freeze

[ADR-0019](0019-a-worktree-lab-is-a-linked-clone-of-a-frozen-main-lab.md)
changes the global-segment rule for one case. When a lab instance is built on
a freeze of the main lab, its global segments join `<segment>@<worktree>`
instead of `<segment>`, so it never shares a wire with the main lab or with
another worktree. Its `connect {}` peers are refused. Every other instance
keeps the rule above.
