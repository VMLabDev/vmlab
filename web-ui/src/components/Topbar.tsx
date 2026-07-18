import { Show } from "solid-js";
import { Badge, DropdownMenu, Icon, IconButton } from "@forge/ui";
import { applyTheme } from "@forge/tokens";
import { Check, CircleHelp, LogOut, Moon, Plus } from "lucide-solid";
import { doLogout, selectLab, showLab, state } from "../store";
import { openNewLabModal } from "./NewLabModal";
import { openHelpTab } from "./WebView";

function fastpathHint(fp: NonNullable<typeof state.fastpath>): string {
  const skipped = Object.entries(fp.reasons).map(([tier, why]) => `${tier}: ${why}`);
  const head = `network fast path: ${fp.tier} (mode ${fp.mode})`;
  return skipped.length ? `${head}\n${skipped.join("\n")}` : head;
}

function toggleTheme() {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const current = document.documentElement.dataset.theme ?? (dark ? "dark" : "light");
  applyTheme(current === "dark" ? "light" : "dark");
}

export default function Topbar() {
  return (
    <>
      <a
        class="ftopbar-brand"
        href="#"
        onClick={(e) => {
          e.preventDefault();
          showLab();
        }}
      >
        <span>
          <span class="brand-vm">vm</span>
          <span class="brand-lab">lab</span>
        </span>
      </a>
      <DropdownMenu
        label={state.currentLab ?? "no lab"}
        size="sm"
        items={[
          ...state.labs.map((l) => ({
            label: (
              <span style={{ display: "inline-flex", "align-items": "center", gap: "8px" }}>
                {l.name}
                <Show when={l.name === state.currentLab}>
                  <Icon of={Check} size={12} />
                </Show>
              </span>
            ),
            onSelect: () => selectLab(l.name),
          })),
          { separator: true },
          { label: <span>New lab…</span>, icon: Plus, onSelect: openNewLabModal },
        ]}
      />
      <div style={{ flex: 1 }} />
      {/* Network fast-path tier (kernel acceleration vs plain userspace
          fabric); hover shows why the faster tiers were skipped. */}
      <Show when={state.fastpath} keyed>
        {(fp) => (
          <span title={fastpathHint(fp)}>
            <Badge tone={fp.tier === "userspace" ? "neutral" : "success"} dot>
              {`net ${fp.tier}`}
            </Badge>
          </span>
        )}
      </Show>
      {/* Host CPU virtualization. Cross-architecture guests still use TCG;
          the tooltip keeps that distinction without crowding the topbar. */}
      <Show when={state.hostLoaded}>
        <span
          title={
            state.host?.acceleration === "kvm"
              ? `KVM available — native ${state.host.arch} guests use hardware virtualization`
              : "KVM unavailable — guests use slower TCG software emulation"
          }
        >
          <Badge tone={state.host?.acceleration === "kvm" ? "success" : "warning"} dot>
            {`cpu ${state.host?.acceleration ?? "tcg"}`}
          </Badge>
        </span>
      </Show>
      <Badge tone={state.connected ? "success" : "neutral"} dot>
        {state.connected ? "connected" : "offline"}
      </Badge>
      {/* The embedded wskill book (or the hosted docs when not bundled),
          opened as an in-app Web tab. */}
      <IconButton icon={CircleHelp} label="Help" onClick={openHelpTab} />
      <IconButton icon={Moon} label="Toggle dark/light" onClick={toggleTheme} />
      <Show when={state.authRequired}>
        <IconButton
          icon={LogOut}
          label={`Sign out${state.authUser ? ` (${state.authUser})` : ""}`}
          onClick={doLogout}
        />
      </Show>
    </>
  );
}
