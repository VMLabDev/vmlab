// A vertical form for one block: its FieldDesc table rendered as FieldRows
// bound to the draft object via a (key, value) setter.

import { For } from "solid-js";
import type { FieldDesc } from "../../editor/fields";
import FieldRow from "./FieldRow";

export interface BlockFormProps {
  fields: FieldDesc[];
  value: Record<string, unknown>;
  onSet: (key: string, value: unknown) => void;
}

export default function BlockForm(props: BlockFormProps) {
  return (
    <div class="block-form">
      <For each={props.fields}>
        {(desc) => (
          <FieldRow
            desc={desc}
            value={props.value[desc.key]}
            onChange={(v) => props.onSet(desc.key, v)}
          />
        )}
      </For>
    </div>
  );
}
