// A container's page: state/health badges, start/stop/restart, its facts
// (image, digest, address, restarts), and three tabs — Console (the raw
// text the container writes, i.e. the `console` log stream), Recovery
// terminal (an on-demand shell into the micro-VM) and Log (every log line
// for this container, timestamped and stream-tagged).

import { Show, createSignal } from "solid-js";
import { Badge, Button, Card, Empty, PageHead, Tabs } from "@forge/ui";
import { RotateCcw } from "lucide-solid";
import PowerButton from "./PowerButton";
import {
  containerLook,
  containerRestart,
  containerStart,
  containerStop,
  state,
} from "../store";
import LogPanel from "./LogPanel";
import TerminalPanel from "./TerminalPanel";

export default function ContainerView() {
  const [tab, setTab] = createSignal<"console" | "terminal" | "log">("console");
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
            <PowerButton
              name={ctr()!.name}
              state={ctr()!.state}
              startLabel="Start"
              stopLabel="Stop"
              onStart={() => containerStart(ctr()!.name)}
              onStop={() => containerStop(ctr()!.name)}
            />
            <Button icon={RotateCcw} disabled={!on()} onClick={() => containerRestart(ctr()!.name)}>
              Restart
            </Button>
          </>
        }
      />

      <Tabs
        tabs={[
          { id: "console", label: "Console" },
          { id: "terminal", label: "Recovery terminal" },
          { id: "log", label: "Log" },
        ]}
        active={tab()}
        onChange={(id) => setTab(id as "console" | "terminal" | "log")}
      />

      <Show when={tab() === "log"}>
        <LogPanel lab={state.currentLab!} source={ctr()!.name} />
      </Show>

      {/* display:none rather than unmount, so a started terminal session
          survives switching tabs; stopping the container tears it down. */}
      <div style={{ display: tab() === "terminal" ? undefined : "none" }}>
        <Show
          when={on()}
          fallback={<Empty title="Container is stopped">Start it to open a terminal.</Empty>}
        >
          <TerminalPanel lab={state.currentLab!} container={ctr()!.name} />
        </Show>
      </div>

      <div class="vm-layout" style={{ display: tab() === "console" ? undefined : "none" }}>
        <div class="ctr-main">
          <LogPanel lab={state.currentLab!} source={ctr()!.name} stream="console" plain />
        </div>
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
