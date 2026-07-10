import {
  For,
  Show,
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
} from "solid-js";
import { Badge, Button, Empty, Input, Logs, PageHead, Select, Toggle } from "@forge/ui";
import { Search } from "lucide-solid";
import { state } from "../store";
import { logsWsUrl } from "../api";
import type { LogEntry } from "../api";

// Cap the in-memory buffer; oldest lines drop off the top.
const MAX = 5000;

// A log line plus its parsed epoch-ms time, so the view can sort chronologically
// without re-parsing the timestamp on every comparison.
type Row = LogEntry & { _t: number };

export default function LogsView() {
  const [entries, setEntries] = createSignal<Row[]>([]);
  const [vm, setVm] = createSignal("all");
  const [query, setQuery] = createSignal("");
  const [follow, setFollow] = createSignal(true);
  const [connected, setConnected] = createSignal(false);
  let pane: HTMLDivElement | undefined;

  // Distinct sources for the filter dropdown: lab + each VM in the lab.
  const sources = () => [
    { value: "all", label: "all sources" },
    { value: "lab", label: "lab" },
    ...(state.status?.vms ?? []).map((v) => ({ value: v.name, label: v.name })),
  ];

  const filtered = createMemo(() => {
    const v = vm();
    const q = query().toLowerCase();
    return entries()
      .filter(
        (e) =>
          (v === "all" || e.source === v) &&
          (q === "" || e.text.toLowerCase().includes(q)),
      )
      .sort((a, b) => a._t - b._t); // chronological across all sources
  });

  // Auto-scroll to the bottom as lines arrive, unless the user scrolled up.
  createEffect(() => {
    filtered();
    if (follow() && pane) queueMicrotask(() => (pane!.scrollTop = pane!.scrollHeight));
  });

  const onScroll = () => {
    if (!pane) return;
    const atBottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
    setFollow(atBottom);
  };

  // (Re)connect the log stream whenever the current lab changes.
  createEffect(() => {
    const lab = state.currentLab;
    if (!lab) return;
    setEntries([]);
    let ws: WebSocket | null = null;
    let closed = false;

    const connect = () => {
      ws = new WebSocket(logsWsUrl(lab));
      ws.onopen = () => setConnected(true);
      ws.onclose = () => {
        setConnected(false);
        if (!closed) setTimeout(connect, 2000);
      };
      ws.onmessage = (msg) => {
        try {
          const e: LogEntry = JSON.parse(msg.data);
          const row: Row = { ...e, _t: e.ts ? Date.parse(e.ts) : Date.now() };
          setEntries((prev) => {
            const next = prev.length >= MAX ? prev.slice(prev.length - MAX + 1) : prev.slice();
            next.push(row);
            return next;
          });
        } catch {
          /* ignore malformed */
        }
      };
    };
    connect();

    onCleanup(() => {
      closed = true;
      ws?.close();
    });
  });

  const setFollowing = (on: boolean) => {
    setFollow(on);
    if (on && pane) pane.scrollTop = pane.scrollHeight;
  };

  return (
    <Show when={state.currentLab} fallback={<Empty title="No lab selected" />}>
      <PageHead
        title="logs"
        sub={`${filtered().length} lines${
          query() || vm() !== "all" ? ` (of ${entries().length})` : ""
        }`}
        actions={
          <div class="log-controls">
            <Select options={sources()} value={vm()} onChange={setVm} />
            <Input
              icon={Search}
              type="search"
              placeholder="filter…"
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
            />
            <Button variant="ghost" onClick={() => setEntries([])} title="Clear the view">
              clear
            </Button>
            <Toggle checked={follow()} onChange={setFollowing}>
              follow
            </Toggle>
            <Badge tone={connected() ? "success" : "neutral"} dot>
              {connected() ? "live" : "offline"}
            </Badge>
          </div>
        }
      />
      <Logs ref={pane} onScroll={onScroll}>
        <Show
          when={filtered().length}
          fallback={
            <div class="log-empty">
              No log lines{query() ? " match the filter" : " yet"}.
            </div>
          }
        >
          <For each={filtered()}>
            {(e) => (
              <div class="logrow">
                <span class="logrow-ts">{fmtTs(e.ts)}</span>
                <span class="logrow-src" classList={{ "is-lab": e.source === "lab" }}>
                  {e.source}
                </span>
                <span class={`logrow-stream ls-${e.stream}`}>{e.stream}</span>
                <span class="logrow-msg">{e.text}</span>
              </div>
            )}
          </For>
        </Show>
      </Logs>
    </Show>
  );
}

function fmtTs(ts?: string | null): string {
  if (!ts) return "";
  const d = new Date(ts);
  if (isNaN(d.getTime())) return "";
  return d.toLocaleTimeString(undefined, { hour12: false });
}
