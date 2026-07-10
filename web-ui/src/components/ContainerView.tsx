// A container's page: state/health badges, start/stop/restart, its facts
// (image, digest, address, restarts), and the live console log. Containers
// have no display, so there is no console/VNC pane — the log takes its place.

import { Show } from "solid-js";
import { Badge, Button, Card, Empty, PageHead } from "@forge/ui";
import { Power, RotateCcw } from "lucide-solid";
import {
  containerLook,
  containerRestart,
  containerStart,
  containerStop,
  state,
} from "../store";
import LogPanel from "./LogPanel";

export default function ContainerView() {
  // Accessors so the view tracks the selected container reactively.
  const ctr = () => state.status?.containers?.find((c) => c.name === state.view.vm);
  const on = () => ctr()?.state === "running";
  const lk = () => {
    const c = ctr();
    return c ? containerLook(c) : { label: "", tone: "neutral" as const };
  };
  const segments = () =>
    ctr()
      ?.nics.map((n) => n.segment)
      .filter(Boolean)
      .join(", ") || "—";
  const digest = () => {
    const d = ctr()?.digest;
    if (!d) return "—";
    // "sha256:abcd…" → the first 12 hex chars are plenty for the eye.
    const hex = d.split(":").pop() ?? d;
    return hex.slice(0, 12);
  };

  return (
    <Show when={ctr()} fallback={<Empty title="Container not found" />}>
      <PageHead
        title={
          <span style={{ display: "inline-flex", "align-items": "center", gap: "10px" }}>
            {ctr()!.name}
            <Badge tone={lk().tone} dot>
              {lk().label}
            </Badge>
          </span>
        }
        sub={`container · ${ctr()!.image}`}
        actions={
          <>
            <Show
              when={on()}
              fallback={
                <Button
                  variant="primary"
                  icon={Power}
                  onClick={() => containerStart(ctr()!.name)}
                >
                  Start
                </Button>
              }
            >
              <Button icon={Power} onClick={() => containerStop(ctr()!.name)}>
                Stop
              </Button>
            </Show>
            <Button icon={RotateCcw} disabled={!on()} onClick={() => containerRestart(ctr()!.name)}>
              Restart
            </Button>
          </>
        }
      />

      <div class="vm-layout">
        <LogPanel lab={state.currentLab!} source={ctr()!.name} />
        <div class="vm-side">
          <Card title="Container">
            <KV k="Image" v={ctr()!.image} />
            <KV k="Digest" v={digest()} />
            <KV k="Address" v={ctr()!.ip ?? "—"} />
            <KV k="MAC" v={ctr()!.nics[0]?.mac ?? "—"} />
            <KV k="Segment" v={segments()} />
            <KV k="Restarts" v={String(ctr()!.restarts)} />
            <KV k="Last exit" v={ctr()!.exit_code != null ? String(ctr()!.exit_code) : "—"} />
            <KV
              k="Health"
              v={ctr()!.health == null ? "no probe" : ctr()!.health ? "healthy" : "unhealthy"}
            />
          </Card>
        </div>
      </div>
    </Show>
  );
}

function KV(p: { k: string; v: string }) {
  return (
    <div class="kv">
      <span class="kv-k">{p.k}</span>
      <span class="kv-v">{p.v}</span>
    </div>
  );
}
