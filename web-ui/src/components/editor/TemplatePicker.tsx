import { Show } from "solid-js";
import { Select } from "@forge/ui";
import type { Option } from "@forge/ui";
import { editor } from "../../editor/store";
import ArtifactPicker from "./ArtifactPicker";

const UNSET = " unset";

export interface TemplatePickerProps {
  value: string;
  onChange: (v: string) => void;
  profile: string | null;
  onMeta: (key: "arch" | "profile", v: string | null) => void;
}

export default function TemplatePicker(props: TemplatePickerProps) {
  const optsWithUnset = (values: string[]): Option[] => [
    { value: UNSET, label: "(pick one)" },
    ...values.map((v) => ({ value: v, label: v })),
  ];
  return (
    <div class="template-picker">
      <ArtifactPicker
        kind="vm"
        value={props.value}
        onSelect={(reference, arch, source) => {
          props.onChange(reference);
          if (source === "local" && reference !== "scratch") {
            props.onMeta("arch", null);
            props.onMeta("profile", null);
          } else {
            props.onMeta("arch", arch);
            if (reference !== "scratch") props.onMeta("profile", null);
          }
        }}
      />
      <Show when={props.value === "scratch"}>
        <Select
          label="Profile"
          help="Guest OS profile (hardware defaults); required for scratch"
          options={optsWithUnset(editor.catalog.profiles)}
          value={props.profile ?? UNSET}
          onChange={(v) => props.onMeta("profile", v === UNSET ? null : v)}
        />
      </Show>
    </div>
  );
}
