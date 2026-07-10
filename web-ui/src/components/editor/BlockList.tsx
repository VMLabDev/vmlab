// Generic child-collection editor: summary rows that expand into a
// BlockForm, plus add/remove. Drives NICs, disks, shares, media, routes,
// records, forwards, rules, sinkholes, provisions and handlers.

import { For, Show, createSignal } from "solid-js";
import { Button, IconButton } from "@forge/ui";
import { ChevronDown, ChevronRight, Plus, Trash2 } from "lucide-solid";
// lucide-solid components satisfy forge's IconComponent shape.
import type { FieldDesc } from "../../editor/fields";
import BlockForm from "./BlockForm";

export interface BlockListProps {
  title: string;
  items: Record<string, unknown>[];
  fields: FieldDesc[];
  summary: (item: any, index: number) => string;
  onAdd: () => void;
  onRemove: (index: number) => void;
  onSet: (index: number, key: string, value: unknown) => void;
  addLabel?: string;
}

export default function BlockList(props: BlockListProps) {
  const [open, setOpen] = createSignal<number | null>(null);

  return (
    <div class="block-list">
      <div class="block-list-head">
        <span class="block-list-title">{props.title}</span>
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() => {
            props.onAdd();
            setOpen(props.items.length); // the row just appended
          }}
        >
          {props.addLabel ?? "Add"}
        </Button>
      </div>
      <Show when={props.items.length} fallback={<div class="block-list-empty">none</div>}>
        <For each={props.items}>
          {(item, i) => (
            <div class="block-list-item">
              <div class="block-list-row" onClick={() => setOpen(open() === i() ? null : i())}>
                {open() === i() ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                <span class="block-list-summary">{props.summary(item, i())}</span>
                <IconButton
                  icon={Trash2}
                  label="Remove"
                  onClick={(e: MouseEvent) => {
                    e.stopPropagation();
                    if (open() === i()) setOpen(null);
                    props.onRemove(i());
                  }}
                />
              </div>
              <Show when={open() === i()}>
                <div class="block-list-body">
                  <BlockForm
                    fields={props.fields}
                    value={item}
                    onSet={(key, v) => props.onSet(i(), key, v)}
                  />
                </div>
              </Show>
            </div>
          )}
        </For>
      </Show>
    </div>
  );
}
