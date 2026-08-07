import { For, Show } from "solid-js";
import { Badge, Card, Icon } from "@forge/ui";
import { CircleAlert, TriangleAlert } from "lucide-solid";
import type { MachineStatus } from "../gen/status";

/**
 * The one sentence PRD §19.6 wants every snapshot surface to say.
 *
 * It restates `bracket::NOT_A_BACKUP` on the Rust side rather than being
 * carried in the status projection — a fixed sentence with no per-machine
 * content has no business on the wire — so the two are kept in step by hand.
 * The wording is the same; only the Rust side's backticks around `destroy`
 * differ, since those are a terminal's markup and would read as punctuation
 * here. It lives beside the workspace rather than beside either snapshot panel
 * because it is the *workspace*'s claim about snapshots, and both panels
 * import it so there is one copy in the console rather than two.
 */
export const NOT_A_BACKUP =
  "Snapshots are not a workspace backup: a dev machine's source lives on the host, which is " +
  "what survives destroy and what a restore re-converges the guest from.";

/**
 * A dev machine's workspace syncer (PRD §19.6).
 *
 * **The console displays the halt and does not act on it.** That is a decision,
 * not a missing feature. Resolution is host-side *necessarily* — ADR-0013's
 * invariant leaves no guest→host control path — and picking a side overwrites
 * one of two copies of a developer's own working file, unrecoverably, next to
 * two directories only they can see. So this shows the halt, names every path,
 * and prints the commands rather than offering a button for them.
 *
 * Everything below comes off the machine's own status projection, which the
 * console already polls — there is no second question asked here.
 */
export default function WorkspaceSync(props: { machine: MachineStatus }) {
  const sync = () => props.machine.dev?.sync ?? undefined;
  const halted = () => sync()?.halt != null;
  // Everything the syncer declined to do, in one list. Each of these is
  // normal and none of them is a halt — but a syncer that stops touching a
  // path without saying so is the silent failure §19.6 exists to rule out.
  const notes = () =>
    [
      sync()?.trouble ? { kind: "trouble", text: sync()!.trouble! } : undefined,
      sync()?.rescan ? { kind: "waiting", text: sync()!.rescan! } : undefined,
      sync()?.reseed ? { kind: "reseed", text: sync()!.reseed! } : undefined,
      sync()?.volume ? { kind: "volume", text: sync()!.volume! } : undefined,
    ].filter(Boolean) as { kind: string; text: string }[];

  return (
    <Show when={sync()}>
      <Card title="Workspace">
        <Show
          when={halted()}
          fallback={
            <Show
              when={sync()!.unsynced.length === 0}
              fallback={
                // Not "in step": these are the paths a snapshot capture
                // refuses on (§19.6), and a console that called this in step
                // would disagree with the refusal the developer is about to
                // read.
                <p class="ws-line">
                  <Badge tone="warning" dot>
                    {sync()!.unsynced.length} behind
                  </Badge>{" "}
                  not yet carried onto the canonical copy:{" "}
                  {sync()!.unsynced.join(", ")}
                </p>
              }
            >
              <p class="ws-line">
                <Badge tone="success" dot>
                  in step
                </Badge>{" "}
                {sync()!.passes} pass{sync()!.passes === 1 ? "" : "es"}
              </p>
            </Show>
          }
        >
          <p class="ws-halt" role="status" aria-live="polite">
            <Icon of={CircleAlert} size={16} /> {sync()!.halt}
          </p>
          <ul class="ws-paths">
            <For each={sync()!.conflicts}>
              {(conflict) => (
                <li>
                  <code>{conflict.path}</code>
                  <span>{conflict.reason}</span>
                </li>
              )}
            </For>
          </ul>
          <Show when={sync()!.conflicts_total > sync()!.conflicts.length}>
            <p class="ws-line">
              … and {sync()!.conflicts_total - sync()!.conflicts.length} more.
            </p>
          </Show>
          {/* Said, not offered: the remedy is a command in the lab
              directory, and this console is deliberately not a route to it. */}
          <p class="ws-line">{sync()!.resolve}</p>
        </Show>

        <For each={notes()}>
          {(note) => (
            <p class="ws-line">
              <Icon of={TriangleAlert} size={14} /> {note.text}
            </p>
          )}
        </For>

        <Show when={sync()!.deferred.length > 0}>
          <p class="ws-line">
            waiting on a git lock — timing rather than a conflict, and it clears itself:{" "}
            {sync()!.deferred.join(", ")}
          </p>
        </Show>

        <Show when={sync()!.skipped.length > 0}>
          <ul class="ws-paths">
            <For each={sync()!.skipped}>
              {(skip) => (
                <li>
                  <code>{skip.path}</code>
                  <span>{skip.reason}</span>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </Card>
    </Show>
  );
}
