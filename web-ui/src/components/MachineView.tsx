import { For, Show, createResource, createSignal } from "solid-js";
import { Badge, Button, Card, Empty, Icon, PageHead, Tabs } from "@forge/ui";
import { Camera, RotateCcw } from "lucide-solid";
import ActionButton from "./ActionButton";
import PowerButton from "./PowerButton";
import {
  state,
  vmStart,
  vmStop,
  vmRestart,
  takeSnapshot,
  restoreSnapshot,
  deleteSnapshot,
  look,
  osOf,
  archOf,
  fmtMem,
} from "../store";
import { vmSnapshots } from "../api";
import { confirmDialog, promptDialog } from "./dialogs";
import ConsoleScreen from "./ConsoleScreen";
import LogPanel from "./LogPanel";

export default function MachineView() {
  const [tab, setTab] = createSignal<"console" | "log">("console");
  // All of these are accessors so the view tracks the selected VM reactively —
  // switching machines re-runs them rather than pinning to the first one.
  const vm = () => state.status?.vms.find((v) => v.name === state.view.vm);
  const on = () => vm()?.state === "running";
  const lk = () => {
    const v = vm();
    return v ? look(v) : { label: "", tone: "neutral" as const };
  };
  const segments = () =>
    vm()
      ?.nics.map((n) => n.segment)
      .filter(Boolean)
      .join(", ") || "—";

  // Re-fetched whenever the selected VM (or its power state) changes, and
  // explicitly after taking a new snapshot.
  const [snaps, { refetch }] = createResource(
    () => (state.view.vm ? `${state.currentLab}/${state.view.vm}/${vm()?.state}` : false),
    () => vmSnapshots(state.currentLab!, state.view.vm!).catch(() => []),
  );

  const takeVmSnapshot = async () => {
    const name = await promptDialog({
      title: "Take snapshot",
      label: `Snapshot name for ${vm()!.name}`,
      confirmLabel: "Take snapshot",
    });
    if (!name) return;
    await takeSnapshot(name, vm()!.name);
    refetch();
  };

  const delVmSnapshot = async (name: string) => {
    const ok = await confirmDialog({
      title: "Delete snapshot",
      body: (
        <>
          Delete snapshot <b>{name}</b> of {vm()!.name}?
        </>
      ),
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    await deleteSnapshot(vm()!.name, name);
    refetch();
  };

  return (
    <Show when={vm()} fallback={<Empty title="Machine not found" />}>
      <PageHead
        title={
          <span style={{ display: "inline-flex", "align-items": "center", gap: "10px" }}>
            {vm()!.name}
            <Badge tone={lk().tone} dot>
              {lk().label}
            </Badge>
          </span>
        }
        sub={`${osOf(vm()!)} · ${archOf(vm()!)} · ${vm()!.template}`}
        actions={
          <>
            <PowerButton
              name={vm()!.name}
              state={vm()!.state ?? "stopped"}
              startLabel="Power on"
              stopLabel="Power off"
              onStart={() => vmStart(vm()!.name)}
              onStop={() => vmStop(vm()!.name)}
              onForceStop={() => vmStop(vm()!.name, true)}
            />
            <ActionButton
              label="Restart"
              busyLabel="Restarting…"
              icon={RotateCcw}
              disabled={!on()}
              onClick={() => vmRestart(vm()!.name)}
              menu={[
                {
                  label: "Force restart",
                  danger: true,
                  onClick: () => vmRestart(vm()!.name, true),
                },
              ]}
            />
          </>
        }
      />

      <Tabs
        tabs={[
          { id: "console", label: "Console" },
          { id: "log", label: "Log" },
        ]}
        active={tab()}
        onChange={(id) => setTab(id as "console" | "log")}
      />

      <Show when={tab() === "log"}>
        <LogPanel lab={state.currentLab!} source={vm()!.name} />
      </Show>

      <div class="vm-layout" style={{ display: tab() === "console" ? undefined : "none" }}>
        <ConsoleScreen lab={state.currentLab!} vm={vm()!.name} powered={on()} />
        <div class="vm-side">
          <Card title="Machine">
            <KV k="Template" v={vm()!.template} />
            <KV k="vCPU" v={vm()!.cpus ? String(vm()!.cpus) : "default"} />
            <KV k="Memory" v={vm()!.memory ? fmtMem(vm()!.memory) : "default"} />
            <KV k="Arch" v={archOf(vm()!)} />
            <KV k="Address" v={vm()!.ip ?? "—"} />
            <KV k="MAC" v={vm()!.nics[0]?.mac ?? "—"} />
            <KV k="Segment" v={segments()} />
          </Card>
          <Card
            title="Snapshots"
            action={
              <Button size="sm" icon={Camera} onClick={takeVmSnapshot}>
                Take
              </Button>
            }
          >
            <Show
              when={(snaps()?.length ?? 0) > 0}
              fallback={<div class="snap-row-time">No snapshots yet.</div>}
            >
              <div class="snap-list">
                <For each={snaps()}>
                  {(sn) => (
                    <div class="snap-row">
                      <Icon of={Camera} size={14} />
                      <div class="snap-row-meta">
                        <div class="snap-row-name">{sn.name}</div>
                        <div class="snap-row-time">{sn.online ? "online" : "offline"}</div>
                      </div>
                      <Button size="sm" onClick={() => restoreSnapshot(sn.name, vm()!.name)}>
                        Restore
                      </Button>
                      <Button size="sm" variant="danger" onClick={() => delVmSnapshot(sn.name)}>
                        Delete
                      </Button>
                    </div>
                  )}
                </For>
              </div>
            </Show>
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
