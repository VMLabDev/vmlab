// Live guest metrics chips (CPU / memory / disks) polled from the
// vmlab-agent while the machine runs. Quietly renders nothing when the
// guest has no agent (old template, vintage guest) — the poll just fails.

import { For, Show, createEffect, createSignal, onCleanup } from "solid-js";
import { Card } from "@forge/ui";
import * as api from "../api";
import { fmtMem } from "../store";

const POLL_MS = 3000;

export default function GuestStats(p: {
  lab: string;
  kind: "vm" | "container";
  name: string;
  running: boolean;
}) {
  const [stats, setStats] = createSignal<api.GuestStats | undefined>();
  let timer: ReturnType<typeof setInterval> | undefined;
  let inFlight = false;

  const poll = async () => {
    if (inFlight) return;
    inFlight = true;
    try {
      const s =
        p.kind === "vm"
          ? await api.vmStats(p.lab, p.name)
          : await api.containerStats(p.lab, p.name);
      setStats(s);
    } catch {
      setStats(undefined); // no agent / stopped: hide the card
    } finally {
      inFlight = false;
    }
  };

  createEffect(() => {
    clearInterval(timer);
    setStats(undefined);
    // Track the machine + power state reactively; poll only while running.
    if (p.running && p.name) {
      poll();
      timer = setInterval(poll, POLL_MS);
    }
  });
  onCleanup(() => clearInterval(timer));

  const pct = (used: number, total: number) => (total > 0 ? (100 * used) / total : 0);

  return (
    <Show when={stats()}>
      {(s) => (
        <Card title="Guest metrics">
          <Meter label="CPU" pct={s().cpu_pct} detail={`${s().cpu_pct.toFixed(0)}%`} />
          <Meter
            label="Memory"
            pct={pct(s().mem_used, s().mem_total)}
            detail={`${fmtMem(s().mem_used)} / ${fmtMem(s().mem_total)}`}
          />
          <For each={s().disks}>
            {(d) => (
              <Meter
                label={d.mount}
                pct={pct(d.used, d.total)}
                detail={`${fmtMem(d.used)} / ${fmtMem(d.total)}`}
              />
            )}
          </For>
        </Card>
      )}
    </Show>
  );
}

function Meter(p: { label: string; pct: number; detail: string }) {
  return (
    <div class="stat-meter">
      <div class="stat-meter-head">
        <span class="stat-meter-label">{p.label}</span>
        <span class="stat-meter-detail">{p.detail}</span>
      </div>
      <div class="stat-meter-track">
        <div
          class="stat-meter-fill"
          classList={{ "stat-meter-hot": p.pct > 85 }}
          style={{ width: `${Math.min(100, Math.max(0, p.pct)).toFixed(1)}%` }}
        />
      </div>
    </div>
  );
}
