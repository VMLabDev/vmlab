// Depends-on list editor: rows of already-picked VMs with remove buttons,
// plus a picker that appends from the remaining candidates (never the VM
// itself — a VM can't depend on itself).

import { For, Show, createMemo } from "solid-js";
import { IconButton, Select } from "@forge/ui";
import { Trash2 } from "lucide-solid";

export interface VmRefListProps {
  label: string;
  doc: string;
  values: string[];
  /** Every VM name offered (the owner is excluded by the caller). */
  candidates: string[];
  onChange: (values: string[]) => void;
}

export default function VmRefList(props: VmRefListProps) {
  const remaining = createMemo(() =>
    props.candidates.filter((n) => !props.values.includes(n)),
  );

  return (
    <div class="field-row">
      <div class="field-row-label" title={props.doc}>
        {props.label}
      </div>
      <div class="field-row-control vmref-list">
        <For each={props.values}>
          {(name) => (
            <div class="vmref-row">
              <span class="vmref-name">{name}</span>
              <IconButton
                icon={Trash2}
                label={`Remove ${name}`}
                onClick={() => props.onChange(props.values.filter((v) => v !== name))}
              />
            </div>
          )}
        </For>
        <Show
          when={remaining().length}
          fallback={
            <Show when={!props.values.length}>
              <span class="field-row-none">no other VMs declared</span>
            </Show>
          }
        >
          <Select
            options={remaining().map((v) => ({ value: v, label: v }))}
            value=""
            placeholder="Add VM…"
            onChange={(v) => {
              if (v) props.onChange([...props.values, v]);
            }}
          />
        </Show>
      </div>
    </div>
  );
}
