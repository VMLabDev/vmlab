// The playbook designer's right pane: the properties of whatever the tree has
// selected. A step's `properties` are generated from its resource's declared
// params (name, description, type, required, default) — that is the whole
// point of the panel.

import { For, Show, createMemo } from "solid-js";
import { Alert, Badge, Button, Input, ListBox, Select, Textarea, Toggle } from "@forge/ui";
import { Trash2 } from "lucide-solid";
import type { CatalogInfo, ContainerDoc, GatherDoc, Kv, PlayDoc, PlaybookDoc, StepDoc, Val } from "../../api";
import type { Sel } from "../../playbook/doc";
import {
  findGatherer,
  findResource,
  gathererRefs,
  isContainer,
  itemAt,
  kvGet,
  playSteps,
  resourceRefs,
  scopeNames,
  selId,
} from "../../playbook/doc";
import PropertyField, { ListPropertyField, RefField } from "./PropertyField";

const CONCURRENCY = ["parallel", "exclusive", "global"];

export interface PlaybookInspectorProps {
  doc: PlaybookDoc;
  selection: Sel | null;
  catalog: CatalogInfo | null;
  warnings: Map<string, string[]>;
  disabled?: boolean;
  /** Mutate the document in place; the designer wraps this in `produce`. */
  mutate: (fn: (doc: PlaybookDoc) => void) => void;
  onDelete: (sel: Sel) => void;
  onAddPackages: () => void;
}

export default function PlaybookInspector(props: PlaybookInspectorProps) {
  const warnings = () => (props.selection ? (props.warnings.get(selId(props.selection)) ?? []) : []);

  return (
    <div class="pb-inspector">
      <Show when={props.selection} fallback={<div class="inspector-note">Select something on the left.</div>}>
        {(sel) => (
          <>
            <Show when={warnings().length}>
              <Alert tone="warning">
                <For each={warnings()}>{(w) => <div>{w}</div>}</For>
              </Alert>
            </Show>
            <fieldset class="inspector-fields" disabled={props.disabled}>
              <Show when={sel().kind === "playbook"}>
                <PlaybookFields {...props} />
              </Show>
              <Show when={sel().kind === "gather"}>
                <GatherFields {...props} index={(sel() as { index: number }).index} />
              </Show>
              <Show when={sel().kind === "var"}>
                <VarFields {...props} index={(sel() as { index: number }).index} />
              </Show>
              <Show when={sel().kind === "play"}>
                <PlayFields {...props} play={(sel() as { play: number }).play} />
              </Show>
              <Show when={sel().kind === "item"}>
                <ItemFields {...props} sel={sel() as { kind: "item"; play: number; path: number[] }} />
              </Show>
            </fieldset>
          </>
        )}
      </Show>
    </div>
  );
}

function Head(props: { kind: string; name: string; onDelete?: () => void; disabled?: boolean }) {
  return (
    <div class="inspector-head">
      <span class="inspector-kind">{props.kind}</span>
      <span class="inspector-name">{props.name || "(unnamed)"}</span>
      <Show when={props.onDelete}>
        <Button
          variant="danger"
          size="sm"
          icon={Trash2}
          disabled={props.disabled}
          onClick={() => props.onDelete?.()}
        >
          Delete
        </Button>
      </Show>
    </div>
  );
}

function PlaybookFields(props: PlaybookInspectorProps) {
  return (
    <>
      <Head kind="playbook" name={props.doc.name} />
      <Input
        label="Name"
        value={props.doc.name}
        help="Shown in run output and docs"
        onInput={(e: InputEvent) =>
          props.mutate((d) => (d.name = (e.currentTarget as HTMLInputElement).value))
        }
      />
      <Textarea
        label="Description"
        value={props.doc.description}
        rows={2}
        onInput={(e: InputEvent) =>
          props.mutate((d) => (d.description = (e.currentTarget as HTMLTextAreaElement).value))
        }
      />
      <Input
        label="Version"
        value={props.doc.version ?? ""}
        help="Defaults to 0.0.0 when empty"
        onInput={(e: InputEvent) =>
          props.mutate((d) => {
            const v = (e.currentTarget as HTMLInputElement).value;
            d.version = v || undefined;
          })
        }
      />
    </>
  );
}

function PlayFields(props: PlaybookInspectorProps & { play: number }) {
  const play = (): PlayDoc | undefined => props.doc.plays[props.play];
  return (
    <Show when={play()}>
      {(p) => (
        <>
          <Head
            kind="play"
            name={p().name}
            disabled={props.disabled}
            onDelete={() => props.onDelete({ kind: "play", play: props.play })}
          />
          <Input
            label="Name"
            value={p().name}
            help="What `vmlab playbook apply --play` names"
            onInput={(e: InputEvent) =>
              props.mutate((d) => (d.plays[props.play].name = (e.currentTarget as HTMLInputElement).value))
            }
          />
          <Textarea
            label="Description"
            value={p().description}
            rows={2}
            onInput={(e: InputEvent) =>
              props.mutate(
                (d) =>
                  (d.plays[props.play].description = (e.currentTarget as HTMLTextAreaElement).value),
              )
            }
          />
          <Toggle
            checked={p().parallel !== false}
            onChange={(on: boolean) =>
              props.mutate((d) => (d.plays[props.play].parallel = on ? undefined : false))
            }
          >
            Run steps in parallel (off = strict declaration order)
          </Toggle>
        </>
      )}
    </Show>
  );
}

function GatherFields(props: PlaybookInspectorProps & { index: number }) {
  const gather = (): GatherDoc | undefined => props.doc.gathers[props.index];
  const decl = () => findGatherer(props.catalog, gather()?.from ?? "");
  return (
    <Show when={gather()}>
      {(g) => (
        <>
          <Head
            kind="gatherer"
            name={g().name}
            disabled={props.disabled}
            onDelete={() => props.onDelete({ kind: "gather", index: props.index })}
          />
          <Input
            label="Variable"
            value={g().name}
            help="The name the gathered facts bind to (use it in expressions)"
            onInput={(e: InputEvent) =>
              props.mutate(
                (d) => (d.gathers[props.index].name = (e.currentTarget as HTMLInputElement).value),
              )
            }
          />
          <RefField
            label="Gatherer"
            help={decl()?.description}
            value={g().from}
            options={gathererRefs(props.catalog)}
            disabled={props.disabled}
            onChange={(v) => props.mutate((d) => (d.gathers[props.index].from = v))}
          />
          <Input
            label="Description"
            value={g().description ?? ""}
            onInput={(e: InputEvent) =>
              props.mutate((d) => {
                const v = (e.currentTarget as HTMLInputElement).value;
                d.gathers[props.index].description = v || undefined;
              })
            }
          />
          <Show when={decl()?.params.length}>
            <div class="inspector-subhead">Parameters</div>
            <For each={decl()!.params}>
              {(param) => (
                <PropertyField
                  param={param}
                  name={param.name}
                  value={kvGet(g().params, param.name)?.value}
                  disabled={props.disabled}
                  onChange={(value) =>
                    props.mutate((d) => setKv(d.gathers[props.index].params, param.name, value))
                  }
                  onRemove={() =>
                    props.mutate((d) => removeKv(d.gathers[props.index].params, param.name))
                  }
                />
              )}
            </For>
          </Show>
          <div class="inspector-note">
            Gather params are evaluated before variables resolve, so they cannot reference vars.
          </div>
        </>
      )}
    </Show>
  );
}

function VarFields(props: PlaybookInspectorProps & { index: number }) {
  const entry = (): Kv | undefined => props.doc.vars[props.index];
  return (
    <Show when={entry()}>
      {(v) => (
        <>
          <Head
            kind="variable"
            name={v().key}
            disabled={props.disabled}
            onDelete={() => props.onDelete({ kind: "var", index: props.index })}
          />
          <Input
            label="Name"
            value={v().key}
            onInput={(e: InputEvent) =>
              props.mutate((d) => (d.vars[props.index].key = (e.currentTarget as HTMLInputElement).value))
            }
          />
          <PropertyField
            name="value"
            value={v().value}
            scope={scopeNames(props.doc)}
            disabled={props.disabled}
            onChange={(value) => props.mutate((d) => (d.vars[props.index].value = value))}
          />
        </>
      )}
    </Show>
  );
}

function ItemFields(props: PlaybookInspectorProps & { sel: { play: number; path: number[] } }) {
  const item = () => itemAt(props.doc, props.sel.play, props.sel.path);
  return (
    <Show when={item()}>
      {(it) => (
        <Show
          when={!isContainer(it())}
          fallback={<ContainerFields {...props} container={(it() as { container: ContainerDoc }).container} />}
        >
          <StepFields {...props} step={(it() as { step: StepDoc }).step} />
        </Show>
      )}
    </Show>
  );
}

/** In-place edit of the addressed item, so every form writes through one path. */
function withItem(
  props: PlaybookInspectorProps & { sel: { play: number; path: number[] } },
  fn: (item: { step?: StepDoc; container?: ContainerDoc }) => void,
) {
  props.mutate((d) => {
    const target = itemAt(d, props.sel.play, props.sel.path);
    if (target) fn(target as { step?: StepDoc; container?: ContainerDoc });
  });
}

function ContainerFields(
  props: PlaybookInspectorProps & { sel: { play: number; path: number[] }; container: ContainerDoc },
) {
  return (
    <>
      <Head
        kind="step folder"
        name={props.container.name}
        disabled={props.disabled}
        onDelete={() => props.onDelete({ kind: "item", ...props.sel })}
      />
      <Input
        label="Name"
        value={props.container.name}
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (i.container) i.container.name = (e.currentTarget as HTMLInputElement).value;
          })
        }
      />
      <Textarea
        label="Description"
        value={props.container.description}
        rows={2}
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (i.container) i.container.description = (e.currentTarget as HTMLTextAreaElement).value;
          })
        }
      />
      <Textarea
        label="Condition"
        value={props.container.condition ?? ""}
        rows={2}
        help="WCL expression; when false every step inside is skipped"
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (!i.container) return;
            const v = (e.currentTarget as HTMLTextAreaElement).value;
            i.container.condition = v.trim() ? v : undefined;
          })
        }
      />
      <div class="inspector-note">
        Folders group steps for readability and share a condition — they don't affect ordering, and
        step names stay unique across the whole play.
      </div>
    </>
  );
}

function StepFields(
  props: PlaybookInspectorProps & { sel: { play: number; path: number[] }; step: StepDoc },
) {
  const decl = () => findResource(props.catalog, props.step.resource);
  const siblings = createMemo(() =>
    playSteps(props.doc.plays[props.sel.play])
      .map((s) => s.name)
      .filter((n) => n && n !== props.step.name),
  );
  const extras = createMemo(() => {
    const declared = new Set((decl()?.params ?? []).map((p) => p.name));
    return props.step.properties.filter((kv) => !declared.has(kv.key));
  });
  const refs = () => resourceRefs(props.catalog);

  return (
    <>
      <Head
        kind="step"
        name={props.step.name}
        disabled={props.disabled}
        onDelete={() => props.onDelete({ kind: "item", ...props.sel })}
      />
      <Input
        label="Name"
        value={props.step.name}
        help="Unique across the play; what other steps name in Requires"
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (i.step) i.step.name = (e.currentTarget as HTMLInputElement).value;
          })
        }
      />
      <Textarea
        label="Description"
        value={props.step.description}
        rows={2}
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (i.step) i.step.description = (e.currentTarget as HTMLTextAreaElement).value;
          })
        }
      />
      <RefField
        label="Resource"
        help={decl()?.description}
        value={props.step.resource}
        options={refs()}
        error={!props.step.resource}
        disabled={props.disabled}
        onChange={(v) =>
          withItem(props, (i) => {
            if (i.step) i.step.resource = v;
          })
        }
      />
      <Show when={!refs().length}>
        <div class="inspector-note">
          No packages installed in this playbook yet.{" "}
          <Button size="sm" onClick={() => props.onAddPackages()}>
            Add a package
          </Button>
        </div>
      </Show>
      <Show when={siblings().length}>
        <ListBox
          label="Requires"
          multiple
          values={props.step.requires}
          options={siblings().map((n) => ({ value: n, label: n }))}
          onChange={(v) =>
            withItem(props, (i) => {
              // `multiple` reports the whole selection.
              if (i.step) i.step.requires = v as unknown as string[];
            })
          }
        />
      </Show>
      <Select
        label="Concurrency"
        value={props.step.concurrency ?? ""}
        help={
          decl()?.concurrency
            ? `${props.step.resource} declares ${decl()!.concurrency}; a step may only tighten it`
            : "How this step may overlap with others"
        }
        options={[
          { value: "", label: `— resource default —` },
          ...CONCURRENCY.map((c) => ({ value: c, label: c })),
        ]}
        onChange={(v: string) =>
          withItem(props, (i) => {
            if (i.step) i.step.concurrency = v || undefined;
          })
        }
      />
      <Textarea
        label="Condition"
        value={props.step.condition ?? ""}
        rows={2}
        help="WCL expression; false = the step is skipped, not failed"
        onInput={(e: InputEvent) =>
          withItem(props, (i) => {
            if (!i.step) return;
            const v = (e.currentTarget as HTMLTextAreaElement).value;
            i.step.condition = v.trim() ? v : undefined;
          })
        }
      />

      <div class="inspector-subhead">
        Properties
        <Show when={decl()}>
          <Badge tone="neutral">{props.step.resource}</Badge>
        </Show>
      </div>
      <Show
        when={decl()}
        fallback={
          <div class="inspector-note">
            {props.step.resource
              ? "That resource isn't installed here, so its parameters are unknown — the properties below are shown as written."
              : "Pick a resource to see its parameters."}
          </div>
        }
      >
        <For each={decl()!.params}>
          {(param) => {
            const current = () => kvGet(props.step.properties, param.name)?.value;
            return (
              <PropertyField
                param={param}
                name={param.name}
                value={current()}
                scope={scopeNames(props.doc)}
                disabled={props.disabled}
                onChange={(value) =>
                  withItem(props, (i) => {
                    if (i.step) setKv(i.step.properties, param.name, value);
                  })
                }
                onRemove={
                  current()
                    ? () =>
                        withItem(props, (i) => {
                          if (i.step) removeKv(i.step.properties, param.name);
                        })
                    : undefined
                }
              />
            );
          }}
        </For>
      </Show>
      <For each={extras()}>
        {(kv) => (
          <PropertyField
            name={kv.key}
            unknown
            value={kv.value}
            scope={scopeNames(props.doc)}
            disabled={props.disabled}
            onChange={(value) =>
              withItem(props, (i) => {
                if (i.step) setKv(i.step.properties, kv.key, value);
              })
            }
            onRemove={() =>
              withItem(props, (i) => {
                if (i.step) removeKv(i.step.properties, kv.key);
              })
            }
          />
        )}
      </For>
    </>
  );
}

// --- kv helpers (mutating; called inside `produce`) --------------------------

function setKv(kvs: Kv[], key: string, value: Val) {
  const existing = kvs.find((kv) => kv.key === key);
  if (existing) existing.value = value;
  else kvs.push({ key, value });
}

function removeKv(kvs: Kv[], key: string) {
  const index = kvs.findIndex((kv) => kv.key === key);
  if (index >= 0) kvs.splice(index, 1);
}

/** Unused re-export guard: keeps the list widget importable from one place. */
export { ListPropertyField };
