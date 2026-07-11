// The start/stop control for a machine (VM or container), reflecting power
// transitions: clicking either side swaps the button for a spinner
// ("Starting…" / "Stopping…") until the observed power state settles, then
// it becomes the opposite button. Server-side transitions (a restart policy
// respawn, a CLI-initiated stop) spin too via the reported state.

import { Show, createEffect, createSignal } from "solid-js";
import { Button, Spinner } from "@forge/ui";
import { Power } from "lucide-solid";

export default function PowerButton(p: {
  /** Machine name — a page switch to another machine drops local pending. */
  name: string;
  /** Reported power state: stopped | starting | running | stopping. */
  state: string;
  startLabel: string;
  stopLabel: string;
  onStart: () => Promise<unknown>;
  onStop: () => Promise<unknown>;
}) {
  // Local pending bridges the gaps the reported state can't see: the action
  // request in flight (the daemon flips state only once it gets there) and
  // the ~350ms status-refresh lag after it completes.
  const [pending, setPending] = createSignal<"start" | "stop" | null>(null);

  // The store's actions resolve only once the daemon finished — and they
  // toast failures rather than throwing — so the spinner clears when the
  // observed state reaches its target, with a grace period as the
  // failed-action fallback.
  createEffect(() => {
    const target = pending() === "start" ? "running" : pending() === "stop" ? "stopped" : null;
    if (target && p.state === target) setPending(null);
  });
  let shown = p.name;
  createEffect(() => {
    if (p.name !== shown) {
      shown = p.name;
      setPending(null);
    }
  });

  const act = async (kind: "start" | "stop", fn: () => Promise<unknown>) => {
    setPending(kind);
    await fn();
    setTimeout(() => setPending((cur) => (cur === kind ? null : cur)), 4000);
  };

  const busy = () =>
    pending() === "start" || p.state === "starting"
      ? "Starting…"
      : pending() === "stop" || p.state === "stopping"
        ? "Stopping…"
        : null;

  return (
    <Show
      when={!busy()}
      fallback={
        <Button disabled class="power-busy">
          <Spinner size={14} />
          {busy()}
        </Button>
      }
    >
      <Show
        when={p.state === "running"}
        fallback={
          <Button variant="primary" icon={Power} onClick={() => act("start", p.onStart)}>
            {p.startLabel}
          </Button>
        }
      >
        <Button icon={Power} onClick={() => act("stop", p.onStop)}>
          {p.stopLabel}
        </Button>
      </Show>
    </Show>
  );
}
