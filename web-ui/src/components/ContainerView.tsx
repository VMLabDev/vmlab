// A container's page: state/health badges, start/stop/restart, its facts
// (image, digest, address, restarts), and three tabs — Console (the raw
// text the container writes, i.e. the `console` log stream), Recovery
// terminal (an on-demand shell into the micro-VM) and Log (every log line
// for this container, timestamped and stream-tagged).

import { For, Show, createSignal } from "solid-js";
import { Badge, Card, Empty, IconButton, PageHead, Tabs } from "@forge/ui";
import { Globe, RotateCcw, SquarePen } from "lucide-solid";
import ActionButton from "./ActionButton";
import PowerButton from "./PowerButton";
import {
  containerLook,
  containerRestart,
  containerStart,
  containerStop,
  currentPullFor,
  playbooksFor,
  state,
} from "../store";
import { canEditPlaybook, editPlaybook } from "./FilesView";
import { openWebPage } from "./WebView";
import GuestStats from "./GuestStats";
import LogPanel from "./LogPanel";
import MachinePullStatus from "./MachinePullStatus";
import PlaybookPanel from "./PlaybookPanel";
import TerminalPanel from "./TerminalPanel";

export default function ContainerView() {
  const [tab, setTab] = createSignal<"console" | "terminal" | "log" | "playbook">("console");
  // Accessors so the view tracks the selected container reactively.
  const ctr = () => state.status?.containers?.find((c) => c.name === state.view.vm);
  const on = () => ctr()?.state === "running";
  const pull = () => (ctr() ? currentPullFor(ctr()!.name) : undefined);
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
              onForceStop={() => containerStop(ctr()!.name, true)}
            />
            <ActionButton
              label="Restart"
              busyLabel="Restarting…"
              icon={RotateCcw}
              disabled={!on()}
              onClick={() => containerRestart(ctr()!.name)}
              menu={[
                {
                  label: "Force restart",
                  danger: true,
                  onClick: () => containerRestart(ctr()!.name, true),
                },
              ]}
            />
          </>
        }
      />

      <Tabs
        tabs={[
          { id: "console", label: "Console" },
          { id: "terminal", label: "Recovery terminal" },
          { id: "log", label: "Log" },
          ...(playbooksFor(ctr()!.name).length > 0
            ? [{ id: "playbook", label: "Playbook" }]
            : []),
        ]}
        active={tab()}
        onChange={(id) => setTab(id as "console" | "terminal" | "log" | "playbook")}
      />

      <Show when={tab() === "log"}>
        <LogPanel lab={state.currentLab!} source={ctr()!.name} />
      </Show>

      <Show when={tab() === "playbook"}>
        <PlaybookPanel
          lab={state.currentLab!}
          kind="container"
          name={ctr()!.name}
          running={on()}
        />
      </Show>

      {/* display:none rather than unmount, so a started terminal session
          survives switching tabs; stopping the container tears it down. */}
      <div style={{ display: tab() === "terminal" ? undefined : "none" }}>
        <Show
          when={on()}
          fallback={<Empty title="Container is stopped">Start it to open a terminal.</Empty>}
        >
          <TerminalPanel
            lab={state.currentLab!}
            target={{ kind: "container", name: ctr()!.name }}
            title="Recovery terminal"
            hint="Opens a shell inside the container (busybox fallback for distroless images)."
          />
        </Show>
      </div>

      <div class="vm-layout" style={{ display: tab() === "console" ? undefined : "none" }}>
        <div class="ctr-main">
          <Show
            when={pull()}
            fallback={
              <LogPanel lab={state.currentLab!} source={ctr()!.name} stream="console" plain />
            }
          >
            {(activePull) => (
              <MachinePullStatus machine={ctr()!.name} kind="image" pull={activePull()} />
            )}
          </Show>
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
          <Show when={playbooksFor(ctr()!.name).length > 0}>
            <Card title="Playbooks">
              <For each={playbooksFor(ctr()!.name)}>
                {(pb) => (
                  <div class="kv">
                    <span class="kv-k pb-link" role="button" onClick={() => setTab("playbook")}>
                      {pb.path}
                    </span>
                    <span
                      class="kv-v"
                      style={{ display: "inline-flex", gap: "6px", "align-items": "center" }}
                    >
                      {pb.play}
                      <Show when={canEditPlaybook(pb.path)}>
                        <IconButton
                          icon={SquarePen}
                          label={`Edit ${pb.path}`}
                          onClick={() => void editPlaybook(pb.path)}
                        />
                      </Show>
                    </span>
                  </div>
                )}
              </For>
            </Card>
          </Show>
          <Show when={(ctr()!.web ?? []).length > 0}>
            <Card title="Web pages">
              <For each={ctr()!.web ?? []}>
                {(page) => (
                  <div class="kv">
                    <span class="kv-k">
                      {page.name} <span class="muted">:{page.port}</span>
                    </span>
                    <span class="kv-v">
                      <IconButton
                        icon={Globe}
                        label={
                          on()
                            ? `Open ${page.name}`
                            : "Start the container to open its web pages"
                        }
                        disabled={!on() || !ctr()!.ip}
                        onClick={() =>
                          openWebPage(state.currentLab!, "containers", ctr()!.name, page)
                        }
                      />
                    </span>
                  </div>
                )}
              </For>
            </Card>
          </Show>
          <GuestStats lab={state.currentLab!} kind="container" name={ctr()!.name} running={on()} />
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
