// The Add-package search picker: search the playbook's registered
// config-weave repos (`pkg search`) and install a hit (`pkg add`).
// Mounted once in App; open from anywhere with openPkgSearch(). Searches
// sync the repos over the network (the first one seeds and clones the
// stdlib repo), so results can take a while — keep the busy note honest.

import { For, Show, createSignal } from "solid-js";
import { Alert, Badge, Button, Empty, Input, Modal, Spinner } from "@forge/ui";
import * as api from "../api";
import type { PkgSearchHit } from "../api";
import { showToast, state } from "../store";

const [target, setTarget] = createSignal<{
  playbook: string;
  onInstalled: () => void;
} | null>(null);

/** Open the picker for a declared playbook folder; `onInstalled` runs
 *  after each successful install (tree/buffer refresh). */
export function openPkgSearch(playbook: string, onInstalled: () => void) {
  setTarget({ playbook, onInstalled });
}

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default function PkgSearchModal() {
  const [term, setTerm] = createSignal("");
  const [hits, setHits] = createSignal<PkgSearchHit[] | null>(null);
  const [searching, setSearching] = createSignal(false);
  const [installing, setInstalling] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  function close() {
    setTarget(null);
    setTerm("");
    setHits(null);
    setError(null);
  }

  async function search() {
    const t = target();
    const lab = state.currentLab;
    if (!t || !lab || !term().trim() || searching()) return;
    setSearching(true);
    setError(null);
    try {
      const found = await api.pkgSearch(lab, t.playbook, term().trim());
      setHits(found);
    } catch (cause) {
      setError(msg(cause));
      setHits(null);
    } finally {
      setSearching(false);
    }
  }

  async function install(hit: PkgSearchHit) {
    const t = target();
    const lab = state.currentLab;
    if (!t || !lab || installing()) return;
    setInstalling(hit.package);
    setError(null);
    try {
      await api.pkgAction(lab, t.playbook, "add", hit.package);
      showToast(`Installed package ${hit.package}`);
      t.onInstalled();
      // Refresh the list so the installed marker appears.
      setHits((prev) =>
        prev?.map((h) =>
          h.package === hit.package && h.repo === hit.repo ? { ...h, installed: true } : h,
        ) ?? null,
      );
    } catch (cause) {
      setError(msg(cause));
    } finally {
      setInstalling(null);
    }
  }

  return (
    <Modal
      open={target() !== null}
      onClose={close}
      title={`Add package — ${target()?.playbook ?? ""}`}
      size="lg"
      footer={
        <Button variant="ghost" onClick={close}>
          Close
        </Button>
      }
    >
      <div class="stack">
        <form
          class="pkg-search-form"
          onSubmit={(e) => {
            e.preventDefault();
            void search();
          }}
        >
          <Input
            label="Search"
            help="Matches package names and descriptions across the registered repos"
            placeholder="e.g. nginx"
            value={term()}
            onInput={(e) => setTerm(e.currentTarget.value)}
          />
          <Button variant="primary" type="submit" disabled={!term().trim() || searching()}>
            {searching() ? "Searching…" : "Search"}
          </Button>
        </form>
        <Show when={searching()}>
          <div class="editor-loading">
            <Spinner /> syncing package repos…
          </div>
        </Show>
        <Show when={error()}>
          <Alert tone="danger">{error()}</Alert>
        </Show>
        <Show when={hits()}>
          {(found) => (
            <Show
              when={found().length > 0}
              fallback={<Empty title="No packages matched">Try a broader term.</Empty>}
            >
              <div class="pkg-hits">
                <For each={found()}>
                  {(hit) => (
                    <div class="pkg-hit">
                      <Badge>{hit.repo}</Badge>
                      <span class="pkg-hit-name">{hit.package}</span>
                      <span class="pkg-hit-desc">{hit.description}</span>
                      <Show
                        when={!hit.installed}
                        fallback={
                          <Badge tone="success">
                            {hit.installed_from
                              ? `installed from ${hit.installed_from}`
                              : "installed"}
                          </Badge>
                        }
                      >
                        <Button
                          size="sm"
                          variant="primary"
                          disabled={installing() !== null}
                          onClick={() => void install(hit)}
                        >
                          {installing() === hit.package ? "Installing…" : "Install"}
                        </Button>
                      </Show>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          )}
        </Show>
      </div>
    </Modal>
  );
}
