import { For, Show, createResource, createSignal } from "solid-js";
import { Badge, Button, Card, Empty, Grid, Icon, Modal, PageHead, Spinner, Stat } from "@forge/ui";
import { Camera, Globe, History, Play, Square, Trash2 } from "lucide-solid";
import {
  state,
  startAll,
  stopAll,
  destroyLab,
  takeSnapshot,
  restoreSnapshot,
  deleteLabSnapshot,
  labSnapshotList,
  fmtMem,
} from "../store";
import { vms } from "../status";
import { confirmDialog, promptDialog } from "./dialogs";
import ActionButton from "./ActionButton";
import LabEditorView from "./editor/LabEditorView";
import PullPanel from "./PullPanel";
import { openWebPage } from "./WebView";
import { NOT_A_BACKUP } from "./WorkspaceSync";

function fmtTime(t: string): string {
  if (!t) return "";
  const d = new Date(t);
  return isNaN(d.getTime()) ? t : d.toLocaleString();
}

export default function LabView() {
  const s = () => state.status;
  // "Machines" spans VMs and containers (they share one lab namespace).
  const machines = () => s()?.machines ?? [];
  const running = () => machines().filter((m) => m.state === "running").length;
  const total = () => machines().length;
  const vcpu = () => vms(s()).reduce((a, v) => a + (v.cpus ?? 0), 0);
  const mem = () => vms(s()).reduce((a, v) => a + (v.memory ?? 0), 0);
  // Every declared `web {}` page across the lab, one launch card each.
  const webPages = () =>
    machines()
      .map((m) => ({
        kind: m.kind === "vm" ? ("vms" as const) : ("containers" as const),
        machine: m.name,
        up: m.state === "running" && !!m.ip,
        pages: m.web,
      }))
      .flatMap((m) => m.pages.map((page) => ({ ...m, page })));

  const snapshotLab = async () => {
    const name = await promptDialog({
      title: "Snapshot lab",
      label: "Snapshot name for the whole lab",
      confirmLabel: "Take snapshot",
    });
    if (name) takeSnapshot(name);
  };

  // Restore-lab picker: a modal listing every snapshot across the lab.
  const [restoreOpen, setRestoreOpen] = createSignal(false);
  const [labSnaps, { refetch: refetchLabSnaps }] = createResource(restoreOpen, (open) =>
    open ? labSnapshotList() : Promise.resolve([]),
  );
  const pickRestore = (name: string) => {
    setRestoreOpen(false);
    restoreSnapshot(name);
  };
  const delLabSnapshot = async (name: string) => {
    const ok = await confirmDialog({
      title: "Delete snapshot",
      body: (
        <>
          Delete snapshot <b>{name}</b> from every VM in the lab?
        </>
      ),
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    await deleteLabSnapshot(name);
    refetchLabSnaps();
  };
  const destroy = async () => {
    const ok = await confirmDialog({
      title: "Destroy lab",
      body: <>Destroy this lab? Clones and lab-local state are deleted.</>,
      confirmLabel: "Destroy",
      danger: true,
    });
    if (ok) destroyLab();
  };

  return (
    <Show when={state.currentLab} fallback={<Empty title="No lab selected" />}>
      <PageHead
        title={s()?.lab ?? state.currentLab!}
        sub={
          s()
            ? `${total()} machines · ${s()!.segments.length} segments`
            : (state.error ?? "lab daemon not running")
        }
        actions={
          <>
            <ActionButton
              label="Start all"
              busyLabel="Starting…"
              icon={Play}
              variant="primary"
              onClick={startAll}
            />
            <ActionButton
              label="Stop all"
              busyLabel="Stopping…"
              icon={Square}
              onClick={() => stopAll()}
              menu={[
                { label: "Force stop all", danger: true, onClick: () => stopAll(true) },
              ]}
            />
            <Button icon={Camera} onClick={snapshotLab}>
              Snapshot
            </Button>
            <Button icon={History} onClick={() => setRestoreOpen(true)}>
              Restore
            </Button>
            <Button
              variant="danger"
              icon={Trash2}
              onClick={destroy}
              disabled={!s()?.provisioned}
              title={
                s()?.provisioned
                  ? undefined
                  : "Nothing to destroy — this lab has no clones or lab-local state yet"
              }
            >
              Destroy
            </Button>
          </>
        }
      />

      <div class="stack">
        <PullPanel />

        <Grid>
          <Card>
            <Stat
              label="Machines up"
              value={s() ? String(running()) : "—"}
              delta={s() ? `/ ${total()}` : undefined}
            />
          </Card>
          <Card>
            <Stat
              label="Allocated vCPU"
              value={vcpu() ? String(vcpu()) : "—"}
              delta={vcpu() ? "cores" : undefined}
            />
          </Card>
          <Card>
            <Stat label="Memory" value={fmtMem(mem() || null)} />
          </Card>
          <Card>
            <Stat label="Segments" value={s() ? String(s()!.segments.length) : "—"} />
          </Card>
        </Grid>

        {/* Per-segment switch counters. A non-zero drop count is the only
            warning that the fabric is shedding frames under load — the thing
            that makes a guest transfer mysteriously slow. The daemon has
            always measured it; until the projection carried it, only the CLI
            could show it. */}
        <Show when={(s()?.segments.length ?? 0) > 0}>
          <Card title="Segments">
            <For each={s()!.segments}>
              {(seg) => (
                <div class="kv">
                  <span class="kv-k">{seg.name}</span>
                  <span class="kv-v">
                    {seg.subnet} · nat {seg.nat ? "on" : "off"} · dhcp{" "}
                    {seg.dhcp ? "on" : "off"}
                    <Show when={seg.frames.dropped > 0}>
                      {" "}
                      <Badge tone="warning" dot>
                        {seg.frames.dropped} frames dropped
                      </Badge>
                    </Show>
                  </span>
                </div>
              )}
            </For>
          </Card>
        </Show>

        <Show when={webPages().length > 0}>
          <Grid>
            <For each={webPages()}>
              {(w) => (
                <div
                  class="webpage-launch"
                  classList={{ off: !w.up }}
                  role="button"
                  tabindex="0"
                  title={
                    w.up
                      ? `Open ${w.page.name}`
                      : "Start the machine to open its web pages"
                  }
                  onClick={() =>
                    w.up && openWebPage(state.currentLab!, w.kind, w.machine, w.page)
                  }
                  onKeyDown={(e) => {
                    if (w.up && (e.key === "Enter" || e.key === " ")) {
                      e.preventDefault();
                      openWebPage(state.currentLab!, w.kind, w.machine, w.page);
                    }
                  }}
                >
                  <Card title={w.page.name} action={<Icon of={Globe} size={14} />}>
                    <div class="webpage-sub">
                      {w.machine}
                      <span class="webpage-addr">
                        :{w.page.port}
                        {w.page.path !== "/" ? w.page.path : ""}
                      </span>
                    </div>
                  </Card>
                </div>
              )}
            </For>
          </Grid>
        </Show>

        <LabEditorView />
      </div>

      <Modal
        open={restoreOpen()}
        onClose={() => setRestoreOpen(false)}
        title="Restore lab"
        footer={
          <Button variant="ghost" onClick={() => setRestoreOpen(false)}>
            Cancel
          </Button>
        }
      >
        <Show
          when={!labSnaps.loading}
          fallback={
            <div style={{ padding: "8px 0" }}>
              <Spinner label="Loading snapshots" /> Loading snapshots…
            </div>
          }
        >
          {/* §19.6, said where a developer would otherwise assume the
              opposite. A lab-wide restore reaches every dev machine in it. */}
          <p class="snap-row-time">{NOT_A_BACKUP}</p>
          <Show
            when={(labSnaps()?.length ?? 0) > 0}
            fallback={<Empty title="No snapshots found in this lab" />}
          >
            <div class="snap-list">
              <For each={labSnaps()}>
                {(snap) => (
                  <div class="snap-row">
                    <Icon of={Camera} size={14} />
                    <div class="snap-row-meta">
                      <div class="snap-row-name">{snap.name}</div>
                      <div class="snap-row-time">{fmtTime(snap.taken_at)}</div>
                    </div>
                    <Button size="sm" onClick={() => pickRestore(snap.name)}>
                      Restore
                    </Button>
                    <Button size="sm" variant="danger" onClick={() => delLabSnapshot(snap.name)}>
                      Delete
                    </Button>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </Modal>
    </Show>
  );
}

