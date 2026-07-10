// The "New lab" dialog: name (DNS label) + optional custom location.
// Mounted once in App (like <Dialogs/>); open it from anywhere with
// openNewLabModal().

import { Show, createSignal } from "solid-js";
import { Button, Input, Modal, Toggle } from "@forge/ui";
import { createLabAndOpen } from "../store";

const [open, setOpen] = createSignal(false);

export function openNewLabModal() {
  setOpen(true);
}

const NAME_RE = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

export default function NewLabModal() {
  const [name, setName] = createSignal("");
  const [custom, setCustom] = createSignal(false);
  const [path, setPath] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const nameOk = () => NAME_RE.test(name());
  const pathOk = () => !custom() || path().startsWith("/");
  const ready = () => nameOk() && pathOk() && !busy();

  function close() {
    setOpen(false);
    setName("");
    setPath("");
    setCustom(false);
    setError(null);
  }

  async function create() {
    if (!ready()) return;
    setBusy(true);
    setError(null);
    try {
      await createLabAndOpen(name(), custom() ? path() : undefined);
      close();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={open()}
      onClose={close}
      title="New lab"
      footer={
        <>
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" onClick={create} disabled={!ready()}>
            {busy() ? "Creating…" : "Create lab"}
          </Button>
        </>
      }
    >
      <div class="stack">
        <Input
          label="Name"
          help="A DNS label: lowercase letters, digits, hyphens (≤63 chars)"
          placeholder="my-lab"
          value={name()}
          error={name() !== "" && !nameOk()}
          onInput={(e) => setName(e.currentTarget.value.toLowerCase())}
          onKeyDown={(e) => {
            if (e.key === "Enter") void create();
          }}
        />
        <div class="field-row">
          <div class="field-row-label">Custom location</div>
          <div class="field-row-control">
            <Toggle checked={custom()} onChange={setCustom} />
          </div>
        </div>
        <Show
          when={custom()}
          fallback={
            <div class="inspector-note">
              The lab is stored in the managed labs directory on the server.
            </div>
          }
        >
          <Input
            label="Directory"
            help="Absolute path on the server host; created if missing, must be empty"
            placeholder="/home/you/labs/my-lab"
            value={path()}
            error={path() !== "" && !pathOk()}
            onInput={(e) => setPath(e.currentTarget.value)}
          />
        </Show>
        <Show when={error()}>
          <div class="newlab-error">{error()}</div>
        </Show>
      </div>
    </Modal>
  );
}
