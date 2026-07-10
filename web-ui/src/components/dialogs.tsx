// Promise-based confirm/prompt dialogs on forge's Modal, replacing the native
// window.confirm()/prompt() flows. Mount <Dialogs /> once (App does); call
// confirmDialog()/promptDialog() from anywhere, like the forge toaster.

import { Match, Show, Switch, createEffect, createSignal } from "solid-js";
import type { JSX } from "solid-js";
import { Button, Input, Modal } from "@forge/ui";

export interface ConfirmOptions {
  title: string;
  body?: JSX.Element;
  confirmLabel?: string;
  danger?: boolean;
}

export interface PromptOptions {
  title: string;
  label?: JSX.Element;
  placeholder?: string;
  initial?: string;
  confirmLabel?: string;
}

type Pending =
  | { kind: "confirm"; opts: ConfirmOptions; resolve: (ok: boolean) => void }
  | { kind: "prompt"; opts: PromptOptions; resolve: (value: string | null) => void };

const [pending, setPending] = createSignal<Pending | null>(null);

/** Ask a yes/no question; resolves false on cancel/escape. */
export function confirmDialog(opts: ConfirmOptions): Promise<boolean> {
  return new Promise((resolve) => setPending({ kind: "confirm", opts, resolve }));
}

/** Ask for a line of text; resolves null on cancel/escape or empty input. */
export function promptDialog(opts: PromptOptions): Promise<string | null> {
  return new Promise((resolve) => setPending({ kind: "prompt", opts, resolve }));
}

export function Dialogs() {
  const [text, setText] = createSignal("");
  let input: HTMLInputElement | undefined;

  createEffect(() => {
    const p = pending();
    if (p?.kind === "prompt") {
      setText(p.opts.initial ?? "");
      queueMicrotask(() => input?.focus());
    }
  });

  const cancel = () => {
    const p = pending();
    setPending(null);
    if (p?.kind === "confirm") p.resolve(false);
    else if (p?.kind === "prompt") p.resolve(null);
  };
  const accept = () => {
    const p = pending();
    setPending(null);
    if (p?.kind === "confirm") p.resolve(true);
    else if (p?.kind === "prompt") p.resolve(text().trim() || null);
  };

  return (
    <Modal
      open={!!pending()}
      onClose={cancel}
      title={pending()?.opts.title}
      footer={
        <>
          <Button variant="ghost" onClick={cancel}>
            Cancel
          </Button>
          <Button
            variant={(() => {
              const p = pending();
              return p?.kind === "confirm" && p.opts.danger ? "danger" : "primary";
            })()}
            onClick={accept}
          >
            {pending()?.opts.confirmLabel ??
              (pending()?.kind === "prompt" ? "OK" : "Confirm")}
          </Button>
        </>
      }
    >
      <Switch>
        <Match when={pending()?.kind === "confirm"}>
          <Show when={(pending() as Extract<Pending, { kind: "confirm" }>).opts.body}>
            {(body) => body()}
          </Show>
        </Match>
        <Match when={pending()?.kind === "prompt"}>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              accept();
            }}
          >
            <Input
              ref={input}
              label={(pending() as Extract<Pending, { kind: "prompt" }>).opts.label}
              placeholder={(pending() as Extract<Pending, { kind: "prompt" }>).opts.placeholder}
              value={text()}
              onInput={(e) => setText(e.currentTarget.value)}
            />
          </form>
        </Match>
      </Switch>
    </Modal>
  );
}
