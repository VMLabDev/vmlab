// The "Add playbook" dialog: folder path + starter play name. Creating
// scaffolds the folder (`<path>/playbook.wcl`) on disk and declares
// `playbook "<path>" { play = "<play>" }` in vmlab.wcl right away, so the
// new playbook is immediately editable in the Files tab. The block targets
// nothing until a machine is cabled to its card, so it is inert on `up`.

import { Show, createEffect, createSignal } from "solid-js";
import { Button, Input, Modal } from "@forge/ui";
import * as api from "../../api";
import { loadPlaybooks, showToast } from "../../store";
import { addPlaybookWithPlay, editor, playbookGroups, saveDraft } from "../../editor/store";

/** Folder segment and play names land inside WCL string literals — the
 *  backend enforces the same shape. */
const NAME_RE = /^[A-Za-z0-9._-]+$/;

/** First `playbooks/playbook-N` not already on the canvas. */
export function nextPlaybookPath(): string {
  const taken = new Set([
    ...(editor.draft?.playbooks ?? []).map((playbook) => playbook.path),
    ...editor.playbookDrafts,
  ]);
  for (let number = 1; number < 10_000; number++) {
    const path = `playbooks/playbook-${number}`;
    if (!taken.has(path)) return path;
  }
  return "playbooks/playbook";
}

export default function NewPlaybookModal(props: { open: boolean; onClose: () => void }) {
  const [path, setPath] = createSignal("");
  const [play, setPlay] = createSignal("main");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  createEffect(() => {
    if (!props.open) return;
    setPath(nextPlaybookPath());
    setPlay("main");
    setError(null);
  });

  const pathIssue = () => {
    const value = path().trim();
    if (!value) return "A folder path is required";
    if (value.startsWith("/")) return "The path must be relative to the lab root";
    const parts = value.split("/");
    if (parts.some((part) => part === "" || part === "." || part === ".."))
      return "The path cannot be empty or leave the lab root";
    if (!NAME_RE.test(parts[parts.length - 1]))
      return "The folder name may use letters, digits, dot, dash and underscore";
    if (playbookGroups().some((group) => group.path === value))
      return "A playbook node with that folder already exists";
    return null;
  };
  const playIssue = () =>
    NAME_RE.test(play().trim())
      ? null
      : "The play name may use letters, digits, dot, dash and underscore";
  const ready = () => !pathIssue() && !playIssue() && !busy();

  function close() {
    setBusy(false);
    props.onClose();
  }

  async function create() {
    const lab = editor.lab;
    if (!ready() || !lab) return;
    const folder = path().trim();
    const name = play().trim();
    setBusy(true);
    setError(null);
    try {
      // Files first: validation rejects a `playbook {}` block whose folder
      // has no playbook.wcl, so the block can only be saved afterwards.
      await api.scaffoldPlaybook(lab, folder, name);
      addPlaybookWithPlay(folder, name);
      close();
      // Silent: this modal's own toast reports the result.
      if (await saveDraft({ auto: true })) {
        // The declaration list gates the Files tab's package buttons — pick
        // the new folder up without waiting for a lab reload.
        void loadPlaybooks();
        showToast(`Created ${folder}`);
      }
    } catch (e) {
      setBusy(false);
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <Modal
      open={props.open}
      onClose={close}
      title="New playbook"
      footer={
        <>
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          <Button variant="primary" onClick={() => void create()} disabled={!ready()}>
            {busy() ? "Creating…" : "Create playbook"}
          </Button>
        </>
      }
    >
      <div class="stack">
        <Input
          label="Folder"
          help={pathIssue() ?? "config-weave playbook folder, relative to the lab root"}
          placeholder="playbooks/baseline"
          value={path()}
          error={pathIssue() !== null}
          onInput={(e) => setPath(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void create();
          }}
        />
        <Input
          label="Play"
          help={playIssue() ?? "The starter play written into playbook.wcl"}
          placeholder="main"
          value={play()}
          error={playIssue() !== null}
          onInput={(e) => setPlay(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") void create();
          }}
        />
        <div class="inspector-note">
          The folder, its playbook.wcl and an empty pkgs/ (ready for the Files tab's Add
          package) are created now, and the playbook is declared in vmlab.wcl (saving any
          other pending changes with it). It runs nothing until you drag its play onto a
          machine.
        </div>
        <Show when={error()}>
          <div class="newlab-error">{error()}</div>
        </Show>
      </div>
    </Modal>
  );
}
