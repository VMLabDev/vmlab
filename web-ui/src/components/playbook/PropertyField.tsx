// One property/param row of the playbook designer.
//
// Deliberately not the lab designer's FieldRow: every value here is a
// config-weave `Val`, i.e. either a literal (typed widget) or raw WCL
// expression source — the "fx" mode a property needs to reference a var or a
// gather result (`path = marker_a`). FieldRow's contract is a plain value, and
// its segref/vmref kinds reach into the lab editor store.

import { Show, createMemo, createSignal } from "solid-js";
import { IconButton, Input, Select, Textarea, Toggle } from "@forge/ui";
import { FunctionSquare, Trash2 } from "lucide-solid";
import type { ParamDoc, Val } from "../../api";
import {
  isExpr,
  isSymbolLiteral,
  litFor,
  symbolChoices,
  symbolToken,
  symbolVal,
  valText,
} from "../../playbook/doc";

export interface PropertyFieldProps {
  /** The declared parameter, when there is one — its description, type,
   *  requiredness and default drive the widget. */
  param?: ParamDoc;
  /** A step property the chosen resource doesn't declare: kept and editable,
   *  but called out. (A var or a free-form value simply has no param.) */
  unknown?: boolean;
  name: string;
  value: Val | undefined;
  /** Names an expression may reference (vars + gather labels). */
  scope?: string[];
  disabled?: boolean;
  onChange: (value: Val) => void;
  /** Present for properties that may be cleared (undeclared or optional). */
  onRemove?: () => void;
}

export default function PropertyField(props: PropertyFieldProps) {
  const type = () => props.param?.type ?? "string";
  const required = () => props.param?.required === true && props.param?.default === undefined;

  /** Symbols ride as expression source (`{expr: ":present"}`) even when the
   *  user is picking from a list, so "is this expression mode?" can't be read
   *  off the value alone — the fx button says so explicitly. */
  const [forced, setForced] = createSignal(false);
  const symbol = () => type() === "symbol";
  const choices = createMemo(() => symbolChoices(props.param));
  const picking = () => symbol() && choices().length > 0 && isSymbolLiteral(props.value);
  const expr = () => forced() || (isExpr(props.value) && !picking());

  const help = createMemo(() => {
    const parts: string[] = [];
    if (props.param?.description) parts.push(props.param.description);
    const fallback = props.param?.default;
    if (fallback && "lit" in fallback && fallback.lit !== null && fallback.lit !== "") {
      parts.push(`default ${String(fallback.lit)}`);
    }
    if (props.unknown) parts.push("not a parameter of this resource");
    if (expr()) {
      const scope = props.scope?.length ? ` — in scope: ${props.scope.join(", ")}` : "";
      parts.push(`WCL expression${scope}`);
    }
    return parts.join(" · ");
  });

  const label = () => `${props.name}${required() ? " *" : ""}`;

  /** The declared symbols, plus whatever the file already says — a value the
   *  package's prose doesn't name is still the file's, so it stays pickable. */
  const symbolOptions = createMemo(() => {
    const current = symbolToken(props.value);
    const values = choices().includes(current) || !current ? choices() : [...choices(), current];
    return [
      { value: "", label: required() ? "— pick one —" : "— unset —" },
      ...values.map((v) => ({ value: v, label: `:${v}` })),
    ];
  });

  /** Switching modes keeps the text, so a literal can be promoted to an
   *  expression (and back) without retyping it. */
  function toggleExpr() {
    const text = valText(props.value);
    if (expr()) {
      setForced(false);
      props.onChange(litFor(type(), text));
    } else {
      setForced(true);
      props.onChange({ expr: text });
    }
  }

  return (
    <div class="pb-prop">
      <div class="pb-prop-field">
        <Show
          when={!expr()}
          fallback={
            <Textarea
              label={label()}
              help={help()}
              value={valText(props.value)}
              rows={2}
              disabled={props.disabled}
              placeholder="marker_a"
              onInput={(e: InputEvent) =>
                props.onChange({ expr: (e.currentTarget as HTMLTextAreaElement).value })
              }
            />
          }
        >
          <Show
            when={type() !== "bool"}
            fallback={
              <div class="pb-prop-bool">
                <div class="pb-prop-bool-label">{label()}</div>
                <Toggle
                  checked={valText(props.value) === "true"}
                  disabled={props.disabled}
                  onChange={(on: boolean) => props.onChange({ lit: on })}
                >
                  {help()}
                </Toggle>
              </div>
            }
          >
            <Show
              when={type() !== "symbol"}
              fallback={
                <Show
                  when={picking()}
                  fallback={
                    <Input
                      label={label()}
                      help={help() || "A WCL symbol, written :name"}
                      value={valText(props.value)}
                      disabled={props.disabled}
                      placeholder=":present"
                      onInput={(e: InputEvent) =>
                        props.onChange(symbolVal((e.currentTarget as HTMLInputElement).value))
                      }
                    />
                  }
                >
                  <Select
                    label={label()}
                    help={help()}
                    value={symbolToken(props.value)}
                    disabled={props.disabled}
                    options={symbolOptions()}
                    onChange={(v: string) => props.onChange(symbolVal(v))}
                  />
                </Show>
              }
            >
              <Input
                label={label()}
                help={help()}
                value={valText(props.value)}
                type={type() === "int" || type() === "float" ? "number" : "text"}
                disabled={props.disabled}
                onInput={(e: InputEvent) =>
                  props.onChange(litFor(type(), (e.currentTarget as HTMLInputElement).value))
                }
              />
            </Show>
          </Show>
        </Show>
      </div>
      <div class="pb-prop-actions">
        <IconButton
          icon={FunctionSquare}
          label={expr() ? `Enter ${props.name} as a value` : `Enter ${props.name} as an expression`}
          disabled={props.disabled}
          onClick={toggleExpr}
        />
        <Show when={props.onRemove}>
          <IconButton
            icon={Trash2}
            label={`Clear ${props.name}`}
            disabled={props.disabled}
            onClick={() => props.onRemove?.()}
          />
        </Show>
      </div>
    </div>
  );
}

/** A `list` param edited as one entry per line — config-weave's list values
 *  are string lists in practice (`requires`-style), and a textarea beats a
 *  bespoke chip editor here. */
export function ListPropertyField(props: {
  label: string;
  help?: string;
  value: string[];
  disabled?: boolean;
  onChange: (value: string[]) => void;
}) {
  return (
    <Textarea
      label={props.label}
      help={props.help ?? "One per line"}
      value={props.value.join("\n")}
      rows={3}
      disabled={props.disabled}
      onInput={(e: InputEvent) =>
        props.onChange(
          (e.currentTarget as HTMLTextAreaElement).value
            .split("\n")
            .map((line) => line.trim())
            .filter(Boolean),
        )
      }
    />
  );
}

/** A picker over what the installed packages declare (`pkg.resource`,
 *  `pkg.gatherer`).
 *
 *  Always a dropdown, never a text box: the point of the panel is to show what
 *  is actually available. A value the catalogue doesn't know — a package that
 *  isn't installed here, or one that has been removed — stays in the list,
 *  marked, so opening a playbook can never quietly drop what the file says. */
export function RefField(props: {
  label: string;
  help?: string;
  value: string;
  options: string[];
  /** Shown when nothing is installed to pick from. */
  emptyHint?: string;
  disabled?: boolean;
  error?: boolean;
  onChange: (value: string) => void;
}) {
  const known = () => props.options.includes(props.value);
  const orphan = () => !!props.value && !known();

  const options = () => [
    {
      value: "",
      label: props.options.length ? "— pick one —" : "— nothing to pick —",
      disabled: true,
    },
    ...props.options.map((o) => ({ value: o, label: o })),
    ...(orphan() ? [{ value: props.value, label: `${props.value} — not installed here` }] : []),
  ];

  const help = () => {
    if (orphan()) return `${props.value} is not among this playbook's packages`;
    if (!props.options.length) return props.emptyHint ?? "Install a package to pick from";
    return props.help;
  };

  return (
    <Select
      label={props.label}
      help={help()}
      value={props.value}
      error={props.error || orphan()}
      disabled={props.disabled}
      options={options()}
      onChange={(v: string) => props.onChange(v)}
    />
  );
}
