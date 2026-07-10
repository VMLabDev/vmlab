// A host-capped slider for an optional (inherited) hardware attr: unset
// shows the template's default marked "inherited"; dragging sets the attr;
// the reset button clears it back to inherited. Clicking the readout swaps
// it for a text box so an exact value can be typed (committed on
// Enter/blur, clamped to the host range).

import { Show, createSignal } from "solid-js";
import { IconButton, Slider } from "@forge/ui";
import { RotateCcw } from "lucide-solid";

export interface SliderRowProps {
  label: string;
  doc: string;
  /** The VM's own value; null = inherited from template→profile. */
  value: number | null;
  /** The template's default, shown while `value` is unset. */
  fallback: number | null;
  min: number;
  max: number;
  step: number;
  fmt: (v: number) => string;
  /** Text → value for the click-to-type readout; null rejects the input. */
  parse: (text: string) => number | null;
  /** What the edit box is pre-filled with (default: `fmt`) — lets a "4 vCPU"
   *  readout edit as a bare "4". */
  editText?: (v: number) => string;
  /** Suffix shown while unset (default "inherited"). */
  unsetLabel?: string;
  onChange: (v: number | null) => void;
}

export default function SliderRow(props: SliderRowProps) {
  const shown = () => props.value ?? props.fallback ?? props.min;
  // Non-null while the readout is being edited as text.
  const [editText, setEditText] = createSignal<string | null>(null);

  const commit = () => {
    const text = editText();
    setEditText(null);
    if (text == null) return;
    const parsed = props.parse(text);
    if (parsed == null) return;
    props.onChange(Math.min(props.max, Math.max(props.min, parsed)));
  };

  return (
    <div class="field-row">
      <div class="field-row-label" title={props.doc}>
        {props.label}
      </div>
      <div class="field-row-control slider-row">
        <Slider
          min={props.min}
          max={props.max}
          step={props.step}
          value={Math.min(props.max, Math.max(props.min, shown()))}
          onChange={(v) => props.onChange(v)}
        />
        <Show
          when={editText() == null}
          fallback={
            <input
              class="slider-value-edit"
              value={editText()!}
              ref={(el) =>
                setTimeout(() => {
                  el.focus();
                  el.select();
                })
              }
              onInput={(e) => setEditText(e.currentTarget.value)}
              onBlur={commit}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                else if (e.key === "Escape") setEditText(null);
              }}
            />
          }
        >
          <span
            class="slider-value"
            classList={{ inherited: props.value == null }}
            title="Click to type an exact value"
            onClick={() => setEditText((props.editText ?? props.fmt)(shown()))}
          >
            {props.fmt(shown())}
            {props.value == null ? ` · ${props.unsetLabel ?? "inherited"}` : ""}
          </span>
        </Show>
        <Show when={props.value != null}>
          <IconButton
            icon={RotateCcw}
            label="Reset to the template default"
            onClick={() => props.onChange(null)}
          />
        </Show>
      </div>
    </div>
  );
}
