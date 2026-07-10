import { For, Show, createEffect, createResource, createSignal } from "solid-js";
import {
  Alert,
  Badge,
  Button,
  Card,
  Empty,
  ListBox,
  PageHead,
  Spinner,
  StatusDot,
} from "@forge/ui";
import { Play, RefreshCw, Upload } from "lucide-solid";
import {
  state,
  buildTemplate,
  publishTemplate,
  dismissTemplateOp,
  type TemplateOp,
} from "../store";
import { templateRemote } from "../api";
import type { TemplateInfo, RemoteStatus } from "../api";

export default function TemplatesView() {
  return (
    <Show
      when={state.currentLab && state.templates.length}
      fallback={<Empty title="No templates">No template blocks in this lab's vmlab.wcl.</Empty>}
    >
      <PageHead
        title="templates"
        sub={`${state.templates.length} defined · built on this host, published to a registry`}
      />
      <div class="stack">
        <For each={state.templates}>{(t) => <TemplateCard t={t} />}</For>
      </div>
    </Show>
  );
}

function TemplateCard(p: { t: TemplateInfo }) {
  const key = () => `${state.currentLab}/${p.t.name}`;
  const op = (): TemplateOp | undefined => state.templateOps[key()];
  const running = () => op()?.status === "running" || p.t.op !== null;
  const [version, setVersion] = createSignal<string | null>(null);
  // Selected publish version, defaulting to the newest local build.
  const selected = () => version() ?? p.t.local_versions[0] ?? null;

  const publish = () => {
    const v = selected();
    if (v) publishTemplate(p.t.name, v);
  };

  return (
    <Card
      title={
        <span class="tpl-title">
          {p.t.name}
          <Badge>{p.t.arch}</Badge>
          <span class="tpl-meta">prefix {p.t.version_prefix}</span>
          <Show when={p.t.registry}>
            <span class="tpl-meta">{p.t.registry}</span>
          </Show>
        </span>
      }
      action={
        <span style={{ display: "inline-flex", gap: "8px" }}>
          <Button
            size="sm"
            variant="primary"
            icon={Play}
            disabled={running()}
            onClick={() => buildTemplate(p.t.name)}
          >
            Build
          </Button>
          <Button
            size="sm"
            icon={Upload}
            disabled={running() || !selected()}
            onClick={publish}
            title={
              selected() ? `Push ${selected()} to the registry` : "No local build to publish"
            }
          >
            Publish
          </Button>
        </span>
      }
    >
      <div class="tpl-cols">
        <div>
          <div class="tpl-sec">Local builds</div>
          <Show
            when={p.t.local_versions.length}
            fallback={<div class="tpl-meta">Nothing built yet.</div>}
          >
            <ListBox
              options={p.t.local_versions.map((v, i) => ({
                value: v,
                label: (
                  <span class="tpl-title">
                    {v}
                    <Show when={i === 0}>
                      <Badge tone="accent">newest</Badge>
                    </Show>
                  </span>
                ),
              }))}
              value={selected() ?? undefined}
              onChange={(v) => setVersion(v)}
            />
          </Show>
        </div>
        <RemotePanel t={p.t} />
      </div>

      <Show when={op()}>
        <OpPanel op={op()!} opKey={key()} />
      </Show>
    </Card>
  );
}

function RemotePanel(p: { t: TemplateInfo }) {
  const [tick, setTick] = createSignal(0);
  const [remote] = createResource(
    () => (p.t.registry ? `${p.t.name}:${tick()}` : null),
    (): Promise<RemoteStatus> => templateRemote(state.currentLab!, p.t.name),
  );
  return (
    <div>
      <div class="tpl-sec">
        Registry
        <Show when={p.t.registry}>
          <Button
            size="sm"
            variant="ghost"
            icon={RefreshCw}
            onClick={() => setTick(tick() + 1)}
            title="Re-query the registry"
          >
            refresh
          </Button>
        </Show>
      </div>
      <Show when={p.t.registry} fallback={<div class="tpl-meta">No registry configured.</div>}>
        <Show
          when={!remote.loading}
          fallback={
            <div class="tpl-meta">
              <Spinner size={12} /> Checking registry…
            </div>
          }
        >
          <Show
            when={!remote.error}
            fallback={<Alert tone="danger">{String(remote.error)}</Alert>}
          >
            <Show
              when={remote()?.tags.length}
              fallback={<div class="tpl-meta">Nothing published yet.</div>}
            >
              <div>
                <For each={remote()!.tags}>
                  {(tag) => (
                    <div class="tag-row">
                      <span>{tag.tag}</span>
                      <span class="tag-row-arches">{tag.arches.join(", ") || "—"}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </Show>
      </Show>
    </div>
  );
}

function OpPanel(p: { op: TemplateOp; opKey: string }) {
  let pane: HTMLDivElement | undefined;
  // Follow the tail as log lines arrive.
  createEffect(() => {
    p.op.log.length;
    if (pane) queueMicrotask(() => (pane!.scrollTop = pane!.scrollHeight));
  });
  const label = () => {
    const verb = p.op.kind === "push" ? "Publish" : "Build";
    switch (p.op.status) {
      case "running":
        return `${verb} running…`;
      case "done":
        return `${verb} finished${p.op.version ? ` — ${p.op.version}` : ""}`;
      default:
        return `${verb} failed`;
    }
  };
  return (
    <div class="tpl-op">
      <div class="tpl-op-head">
        <StatusDot
          tone={
            p.op.status === "running" ? "warning" : p.op.status === "done" ? "success" : "danger"
          }
        />
        <span>{label()}</span>
        <Show when={p.op.status !== "running"}>
          <Button size="sm" variant="ghost" onClick={() => dismissTemplateOp(p.opKey)}>
            dismiss
          </Button>
        </Show>
      </div>
      <Show when={p.op.error}>
        <Alert tone="danger">{p.op.error}</Alert>
      </Show>
      <Show when={p.op.log.length}>
        <div class="tpl-log" ref={pane}>
          <For each={p.op.log}>{(line) => <div>{line}</div>}</For>
        </div>
      </Show>
    </div>
  );
}
