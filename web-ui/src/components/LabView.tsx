import { For, Show, createResource, createSignal } from "solid-js";
import {
  Button,
  Card,
  Empty,
  Grid,
  Icon,
  Modal,
  PageHead,
  Progress,
  Spinner,
  Stat,
} from "@forge/ui";
import {
  Camera,
  Download,
  History,
  Play,
  Square,
  Trash2,
} from "lucide-solid";
import {
  state,
  startAll,
  stopAll,
  destroyLab,
  pullLab,
  needsPull,
  takeSnapshot,
  restoreSnapshot,
  deleteLabSnapshot,
  labSnapshotList,
  fmtMem,
  fmtBytes,
  currentPulls,
  type Pull,
} from "../store";
import { confirmDialog, promptDialog } from "./dialogs";
import ActionButton from "./ActionButton";
import LabEditorView from "./editor/LabEditorView";

function fmtTime(t: string): string {
  if (!t) return "";
  const d = new Date(t);
  return isNaN(d.getTime()) ? t : d.toLocaleString();
}

export default function LabView() {
  const s = () => state.status;
  const pulls = () => currentPulls();
  // "Machines" spans VMs and containers (they share one lab namespace).
  const containers = () => s()?.containers ?? [];
  const running = () =>
    (s()?.vms.filter((v) => v.state === "running").length ?? 0) +
    containers().filter((c) => c.state === "running").length;
  const total = () => (s()?.vms.length ?? 0) + containers().length;
  const vcpu = () => s()?.vms.reduce((a, v) => a + (v.cpus ?? 0), 0) ?? 0;
  const mem = () => s()?.vms.reduce((a, v) => a + (v.memory ?? 0), 0) ?? 0;

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
            : "lab daemon not running"
        }
        actions={
          <>
            <Show when={needsPull()}>
              <ActionButton
                label="Download templates"
                busyLabel="Downloading…"
                icon={Download}
                onClick={pullLab}
              />
            </Show>
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
            <Button variant="danger" icon={Trash2} onClick={destroy}>
              Destroy
            </Button>
          </>
        }
      />

      <div class="stack">
        <Show when={pulls().length}>
          <PullPanel pulls={pulls()} />
        </Show>

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

function PullPanel(p: { pulls: Pull[] }) {
  return (
    <Card title="Downloading templates">
      <div class="pull-list">
        <For each={p.pulls}>
          {(pl) => (
            <div>
              <Progress
                label={`${pl.vm} — ${pl.reference}`}
                value={pl.status === "error" ? 100 : pl.percent}
                indeterminate={pl.status === "checking"}
                tone={pl.status === "error" ? "danger" : "accent"}
                showValue={pl.status === "pulling"}
              />
              <div class="pull-sub" classList={{ "is-error": pl.status === "error" }}>
                {pl.status === "error"
                  ? pl.error
                  : pl.status === "checking"
                    ? "Resolving template…"
                    : `${fmtBytes(pl.bytesDone)} / ${fmtBytes(pl.bytesTotal)}`}
              </div>
            </div>
          )}
        </For>
      </div>
    </Card>
  );
}
