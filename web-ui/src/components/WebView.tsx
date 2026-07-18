// The "Web" area: guest-served HTTP UIs (declared `web {}` blocks) opened as
// in-app tabs, each an iframe pointed at the same-origin proxy. Tab state is
// module-scoped (like FilesView) so it survives view switches; every iframe
// stays mounted with display toggled, so switching tabs never reloads a page.

import { For, Show, createSignal } from "solid-js";
import { Empty, IconButton } from "@forge/ui";
import { RotateCw, X } from "lucide-solid";
import * as api from "../api";
import type { WebPage } from "../api";
import { showWeb } from "../store";

interface WebTab {
  id: string;
  lab: string;
  kind: "vms" | "containers";
  machine: string;
  page: string;
  title: string;
  src: string;
  /** Bumped to force the iframe to reload. */
  nonce: number;
}

const [tabs, setTabs] = createSignal<WebTab[]>([]);
const [activeId, setActiveId] = createSignal<string | null>(null);

/** Count of open tabs, for the sidebar badge. */
export function openWebCount(): number {
  return tabs().length;
}

/** Open (or focus) a page in the Web area and switch to it. */
export function openWebPage(
  lab: string,
  kind: "vms" | "containers",
  machine: string,
  page: WebPage,
) {
  const id = `${lab}/${kind}/${machine}/${page.name}`;
  if (!tabs().some((t) => t.id === id)) {
    setTabs([
      ...tabs(),
      {
        id,
        lab,
        kind,
        machine,
        page: page.name,
        title: `${machine} · ${page.name}`,
        src: api.webPageUrl(lab, kind, machine, page),
        nonce: 0,
      },
    ]);
  }
  setActiveId(id);
  showWeb();
}

function closeTab(id: string, e: MouseEvent) {
  e.stopPropagation();
  const remaining = tabs().filter((t) => t.id !== id);
  setTabs(remaining);
  if (activeId() === id) {
    setActiveId(remaining.length ? remaining[remaining.length - 1].id : null);
  }
}

function reloadTab(id: string, e: MouseEvent) {
  e.stopPropagation();
  setTabs(tabs().map((t) => (t.id === id ? { ...t, nonce: t.nonce + 1 } : t)));
}

export default function WebView() {
  // Drop tabs whose lab is no longer the current one? Keep them — the proxy
  // is lab-scoped in the URL, so cross-lab tabs still resolve.
  return (
    <div class="web-view">
      <Show
        when={tabs().length > 0}
        fallback={
          <Empty title="No web pages open">
            Launch a page from a machine's console or the topology canvas — pages declared with a{" "}
            <code>web {"{}"}</code> block appear here as tabs.
          </Empty>
        }
      >
        <div class="web-tabstrip">
          <For each={tabs()}>
            {(tab) => (
              <div
                class="web-tab"
                classList={{ active: tab.id === activeId() }}
                role="button"
                onClick={() => setActiveId(tab.id)}
              >
                <span class="web-tab-title">{tab.title}</span>
                <IconButton
                  icon={RotateCw}
                  label={`Reload ${tab.title}`}
                  onClick={(e: MouseEvent) => reloadTab(tab.id, e)}
                />
                <IconButton
                  icon={X}
                  label={`Close ${tab.title}`}
                  onClick={(e: MouseEvent) => closeTab(tab.id, e)}
                />
              </div>
            )}
          </For>
        </div>
        <div class="web-frames">
          <For each={tabs()}>
            {(tab) => (
              <iframe
                class="web-frame"
                classList={{ active: tab.id === activeId() }}
                src={frameSrc(tab)}
                title={tab.title}
              />
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

/** The reload button bumps `nonce`; folding it into the query changes `src`
 *  so the iframe reloads (the proxy forwards the query harmlessly). */
function frameSrc(tab: WebTab): string {
  if (tab.nonce === 0) return tab.src;
  const sep = tab.src.includes("?") ? "&" : "?";
  return `${tab.src}${sep}_vmlab_reload=${tab.nonce}`;
}
