import { For, Show, createEffect, createResource, createSignal } from "solid-js";
import {
  state,
  buildTemplate,
  publishTemplate,
  dismissTemplateOp,
  type TemplateOp,
} from "../store";
import { templateRemote } from "../api";
import type { TemplateInfo, RemoteStatus } from "../api";
import * as I from "./icons";

export default function TemplatesView() {
  return (
    <Show
      when={state.currentLab && state.templates.length}
      fallback={
        <div class="body">
          <div class="csub">No template blocks in this lab's vmlab.wcl.</div>
        </div>
      }
    >
      <header class="chead">
        <div>
          <div class="eyebrow">// templates</div>
          <h1 class="ctitle">templates</h1>
          <div class="csub">
            {state.templates.length} defined · built on this host, published to a
            registry
          </div>
        </div>
      </header>
      <div class="body">
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
    <div class="tpl">
      <div class="tplhd">
        <span class="tplname">{p.t.name}</span>
        <span class="niarch">{p.t.arch}</span>
        <span class="tplver">prefix {p.t.version_prefix}</span>
        <Show when={p.t.registry}>
          <span class="tplreg">{p.t.registry}</span>
        </Show>
        <div class="tplact">
          <button
            class="btn btn-primary"
            classList={{ dis: running() }}
            onClick={() => buildTemplate(p.t.name)}
          >
            <I.Play />
            Build
          </button>
          <button
            class="btn"
            classList={{ dis: running() || !selected() }}
            onClick={publish}
            title={selected() ? `Push ${selected()} to the registry` : "No local build to publish"}
          >
            <I.Upload />
            Publish
          </button>
        </div>
      </div>

      <div class="tplcols">
        <div>
          <h3 class="sectitle">Local builds</h3>
          <Show
            when={p.t.local_versions.length}
            fallback={<div class="tplempty">Nothing built yet.</div>}
          >
            <div class="tpllist">
              <For each={p.t.local_versions}>
                {(v) => (
                  <button
                    class="tplverrow"
                    classList={{ on: v === selected() }}
                    onClick={() => setVersion(v)}
                    title="Select for publishing"
                  >
                    <span class="tplvname">{v}</span>
                    <Show when={v === p.t.local_versions[0]}>
                      <span class="tpltag">newest</span>
                    </Show>
                    <span class="tplcheck">
                      <I.Check />
                    </span>
                  </button>
                )}
              </For>
            </div>
          </Show>
        </div>
        <RemotePanel t={p.t} />
      </div>

      <Show when={op()}>
        <OpPanel op={op()!} opKey={key()} />
      </Show>
    </div>
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
      <h3 class="sectitle">
        Registry
        <Show when={p.t.registry}>
          <button
            class="tplrefresh"
            onClick={() => setTick(tick() + 1)}
            title="Re-query the registry"
          >
            refresh
          </button>
        </Show>
      </h3>
      <Show
        when={p.t.registry}
        fallback={<div class="tplempty">No registry configured.</div>}
      >
        <Show when={!remote.loading} fallback={<div class="tplempty">Checking registry…</div>}>
          <Show
            when={!remote.error}
            fallback={<div class="tplempty c-dan">{String(remote.error)}</div>}
          >
            <Show
              when={remote()?.tags.length}
              fallback={<div class="tplempty">Nothing published yet.</div>}
            >
              <div class="tpllist">
                <For each={remote()!.tags}>
                  {(tag) => (
                    <div class="tplverrow static">
                      <span class="tplvname">{tag.tag}</span>
                      <span class="tplarches">{tag.arches.join(", ") || "—"}</span>
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
    <div class="tplop">
      <div class="tplophd">
        <span
          class="sdot"
          style={`background:${
            p.op.status === "running"
              ? "var(--warning-fg)"
              : p.op.status === "done"
                ? "var(--success-fg)"
                : "var(--danger-fg)"
          }`}
        />
        <span class="tplopstate" classList={{ "c-dan": p.op.status === "error" }}>
          {label()}
        </span>
        <Show when={p.op.status !== "running"}>
          <button class="tplrefresh" onClick={() => dismissTemplateOp(p.opKey)}>
            dismiss
          </button>
        </Show>
      </div>
      <Show when={p.op.error}>
        <div class="tploperr">{p.op.error}</div>
      </Show>
      <Show when={p.op.log.length}>
        <div class="tpllog" ref={pane}>
          <For each={p.op.log}>{(line) => <div>{line}</div>}</For>
        </div>
      </Show>
    </div>
  );
}
