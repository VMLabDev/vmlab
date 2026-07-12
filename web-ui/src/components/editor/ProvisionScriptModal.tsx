import { CodeEditor } from "@forge/code";
import { Button, Modal, Spinner } from "@forge/ui";
import { Show, createEffect, createSignal } from "solid-js";
import * as api from "../../api";
import { editor } from "../../editor/store";
import { anyVmRunning, showToast } from "../../store";
import { wscriptLanguage } from "../../wscript-language";
import { confirmDialog } from "../dialogs";

export default function ProvisionScriptModal(props: {
  path: string | null;
  onClose: () => void;
}) {
  const [content, setContent] = createSignal("");
  const [saved, setSaved] = createSignal("");
  const [rev, setRev] = createSignal<string | null>(null);
  const [exists, setExists] = createSignal(false);
  const [loading, setLoading] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const dirty = () => content() !== saved();
  const needsSave = () => !exists() || dirty();
  let loadGeneration = 0;

  createEffect(() => {
    const path = props.path;
    const lab = editor.lab;
    if (!path || !lab) return;
    const generation = ++loadGeneration;
    setContent("");
    setSaved("");
    setRev(null);
    setExists(false);
    setLoading(true);
    setError(null);
    void api
      .getProvisionScript(lab, path)
      .then((doc) => {
        if (generation !== loadGeneration) return;
        const source = doc?.content ?? "";
        setContent(source);
        setSaved(source);
        setRev(doc?.rev ?? null);
        setExists(doc !== null);
      })
      .catch((cause) => {
        if (generation === loadGeneration) {
          setError(cause instanceof Error ? cause.message : String(cause));
        }
      })
      .finally(() => {
        if (generation === loadGeneration) setLoading(false);
      });
  });

  async function close() {
    if (
      dirty() &&
      !(await confirmDialog({
        title: "Discard unsaved script changes?",
        body: "The provision script has edits that have not been saved.",
        confirmLabel: "Discard",
        danger: true,
      }))
    ) {
      return;
    }
    props.onClose();
  }

  async function save() {
    const path = props.path;
    const lab = editor.lab;
    if (!path || !lab) return;
    setSaving(true);
    setError(null);
    try {
      const nextRev = await api.saveProvisionScript(lab, path, content(), rev());
      setRev(nextRev);
      setSaved(content());
      setExists(true);
      showToast(`Saved ${path}`);
    } catch (cause) {
      const message = cause instanceof Error ? cause.message : String(cause);
      setError(message);
      showToast(message, "danger");
    } finally {
      setSaving(false);
    }
  }

  return (
    <Modal
      open={props.path !== null}
      title={props.path ? `Provision · ${props.path}` : "Provision script"}
      onClose={() => void close()}
      footer={
        <>
          <Button variant="ghost" onClick={() => void close()}>
            Cancel
          </Button>
          <Button
            variant="primary"
            disabled={loading() || saving() || anyVmRunning() || !!error() || !needsSave()}
            onClick={() => void save()}
          >
            {saving() ? "Saving…" : exists() ? "Save script" : "Create script"}
          </Button>
        </>
      }
    >
      <div
        class="provision-script-modal"
        onKeyDown={(event) => event.stopPropagation()}
        onKeyUp={(event) => event.stopPropagation()}
        onPointerDown={(event) => event.stopPropagation()}
      >
        <Show when={loading()} fallback={
          <CodeEditor
            value={content()}
            onChange={(value) => {
              setContent(value);
              setError(null);
            }}
            language={wscriptLanguage}
            readOnly={anyVmRunning()}
            placeholder="use vmlab\n\nfn main(lab: Lab) {\n    // provision the lab\n}"
            height="min(62vh, 680px)"
          />
        }>
          <div class="editor-loading"><Spinner /> loading script…</div>
        </Show>
        <Show when={!exists() && !loading()}>
          <div class="provision-script-new">New file — it will be created when you save.</div>
        </Show>
        <Show when={anyVmRunning()}>
          <div class="topo-nic-ip-error">Stop all VMs and containers before editing scripts.</div>
        </Show>
        <Show when={error()}>{(message) => <div class="topo-nic-ip-error">{message()}</div>}</Show>
      </div>
    </Modal>
  );
}
