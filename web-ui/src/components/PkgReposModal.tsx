// The playbook's package-repository manager: list registered config-weave
// repos (`pkg repo list` — the backend seeds the stdlib repo when none are
// registered), register new ones (`pkg repo add`, which syncs immediately
// so a bad URL fails fast), and unregister (`pkg repo remove`). Mounted
// once in App; open with openPkgRepos().

import { For, Show, createEffect, createSignal } from "solid-js";
import { Alert, Badge, Button, IconButton, Input, Modal, Spinner } from "@forge/ui";
import { Trash2 } from "lucide-solid";
import * as api from "../api";
import type { PkgRepo } from "../api";
import { showToast, state } from "../store";
import { confirmDialog } from "./dialogs";

const [target, setTarget] = createSignal<{
  playbook: string;
  onChanged: () => void;
} | null>(null);

/** Open the repos manager for a declared playbook folder; `onChanged`
 *  runs after add/remove (repo.wcl changed on disk). */
export function openPkgRepos(playbook: string, onChanged: () => void) {
  setTarget({ playbook, onChanged });
}

const NAME_RE = /^[A-Za-z0-9_-][A-Za-z0-9_.-]*$/;

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default function PkgReposModal() {
  const [repos, setRepos] = createSignal<PkgRepo[] | null>(null);
  const [seeded, setSeeded] = createSignal(false);
  const [warning, setWarning] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [name, setName] = createSignal("");
  const [url, setUrl] = createSignal("");
  const [branch, setBranch] = createSignal("");
  const [subdir, setSubdir] = createSignal("");

  function close() {
    setTarget(null);
    setRepos(null);
    setSeeded(false);
    setWarning(null);
    setError(null);
    setName("");
    setUrl("");
    setBranch("");
    setSubdir("");
  }

  async function load() {
    const t = target();
    const lab = state.currentLab;
    if (!t || !lab) return;
    setLoading(true);
    setError(null);
    try {
      const result = await api.pkgRepos(lab, t.playbook);
      setRepos(result.repos);
      setSeeded(result.seeded);
      setWarning(result.warning ?? null);
      if (result.seeded) t.onChanged();
    } catch (cause) {
      setError(msg(cause));
    } finally {
      setLoading(false);
    }
  }

  // Fetch on open (the list itself is local + fast; seeding clones).
  createEffect(() => {
    if (target()) void load();
  });

  const nameOk = () => NAME_RE.test(name());
  const addReady = () => nameOk() && url().trim() !== "" && !busy();

  async function addRepo() {
    const t = target();
    const lab = state.currentLab;
    if (!t || !lab || !addReady()) return;
    setBusy(true);
    setError(null);
    try {
      await api.pkgRepoEdit(lab, t.playbook, "add", name().trim(), {
        url: url().trim(),
        branch: branch().trim() || undefined,
        subdir: subdir().trim() || undefined,
      });
      showToast(`Registered repo ${name().trim()}`);
      setName("");
      setUrl("");
      setBranch("");
      setSubdir("");
      t.onChanged();
      await load();
    } catch (cause) {
      // A failed sync still registers the entry (config-weave behavior) —
      // reload so the list reflects reality alongside the error.
      setError(msg(cause));
      t.onChanged();
      await load();
    } finally {
      setBusy(false);
    }
  }

  async function removeRepo(repo: PkgRepo) {
    const t = target();
    const lab = state.currentLab;
    if (!t || !lab || busy()) return;
    const ok = await confirmDialog({
      title: `Unregister repo ${repo.name}?`,
      body: "Installed packages from it stay in pkgs/, but updating them will fail until the repo is re-registered.",
      confirmLabel: "Unregister",
      danger: true,
    });
    if (!ok) return;
    setBusy(true);
    setError(null);
    try {
      await api.pkgRepoEdit(lab, t.playbook, "remove", repo.name);
      showToast(`Unregistered repo ${repo.name}`);
      t.onChanged();
      await load();
    } catch (cause) {
      setError(msg(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Modal
      open={target() !== null}
      onClose={close}
      title={`Package repos — ${target()?.playbook ?? ""}`}
      size="lg"
      footer={
        <Button variant="ghost" onClick={close}>
          Close
        </Button>
      }
    >
      <div class="stack">
        <Show when={loading()}>
          <div class="editor-loading">
            <Spinner /> loading repos… (first open registers and clones the stdlib repo)
          </div>
        </Show>
        <Show when={seeded()}>
          <Alert tone="info">The default stdlib repo was registered for this playbook.</Alert>
        </Show>
        <Show when={warning()}>
          <Alert tone="warning">{warning()}</Alert>
        </Show>
        <Show when={error()}>
          <Alert tone="danger">{error()}</Alert>
        </Show>
        <Show when={repos()}>
          {(list) => (
            <div class="pkg-repos">
              <For
                each={list()}
                fallback={<Alert tone="info">No repositories registered yet.</Alert>}
              >
                {(repo) => (
                  <div class="pkg-repo">
                    <span class="pkg-repo-name">{repo.name}</span>
                    <span class="pkg-repo-url" title={repo.url}>
                      {repo.url}
                    </span>
                    <Show when={repo.branch}>
                      <Badge>{repo.branch}</Badge>
                    </Show>
                    <Show when={repo.subdir}>
                      <Badge tone="neutral">/{repo.subdir}</Badge>
                    </Show>
                    <Badge tone={repo.cache === "not synced" ? "warning" : "neutral"}>
                      {repo.cache}
                    </Badge>
                    <IconButton
                      icon={Trash2}
                      label={`Unregister ${repo.name}`}
                      disabled={busy()}
                      onClick={() => void removeRepo(repo)}
                    />
                  </div>
                )}
              </For>
            </div>
          )}
        </Show>
        <div class="pkg-repo-add">
          <Input
            label="Name"
            placeholder="corp"
            value={name()}
            error={name() !== "" && !nameOk()}
            onInput={(e) => setName(e.currentTarget.value)}
          />
          <Input
            label="Git URL"
            placeholder="https://git.example.com/pkgs.git"
            value={url()}
            onInput={(e) => setUrl(e.currentTarget.value)}
          />
          <Input
            label="Branch"
            help="Remote default when empty"
            value={branch()}
            onInput={(e) => setBranch(e.currentTarget.value)}
          />
          <Input
            label="Subdir"
            help="Directory holding the packages; checkout root when empty"
            placeholder="pkgs"
            value={subdir()}
            onInput={(e) => setSubdir(e.currentTarget.value)}
          />
          <Button variant="primary" disabled={!addReady()} onClick={() => void addRepo()}>
            {busy() ? "Working…" : "Add repo"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
