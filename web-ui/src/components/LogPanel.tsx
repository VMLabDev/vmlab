// A live log pane for one source: the lab daemon's own logs on the lab
// page's Logs tab, or a single machine's logs on its page. Streams the
// lab's log WebSocket and keeps only the wanted source's lines, with
// search / clear / follow controls. `stream` narrows to one stream (e.g. a
// container's `console`), and `plain` drops the timestamp/stream columns for
// a raw console-style read.

import { For, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { Badge, Button, Input, Logs, Toggle } from "@forge/ui";
import { Search } from "lucide-solid";
import { logsWsUrl } from "../api";
import type { LogEntry } from "../api";

// Cap the in-memory buffer; oldest lines drop off the top.
const MAX = 5000;

// A log line plus its parsed epoch-ms time, so the view can sort
// chronologically without re-parsing the timestamp on every comparison.
type Row = LogEntry & { _t: number };

export default function LogPanel(props: {
  lab: string;
  source: string;
  stream?: string;
  plain?: boolean;
}) {
  const [entries, setEntries] = createSignal<Row[]>([]);
  const [query, setQuery] = createSignal("");
  const [follow, setFollow] = createSignal(true);
  const [connected, setConnected] = createSignal(false);
  let pane: HTMLDivElement | undefined;

  const filtered = createMemo(() => {
    const q = query().toLowerCase();
    return entries()
      .filter((e) => q === "" || e.text.toLowerCase().includes(q))
      .sort((a, b) => a._t - b._t);
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

  // (Re)connect the stream whenever the lab or the wanted source changes.
  createEffect(() => {
    const { lab, source, stream } = props;
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
          if (e.source !== source) return;
          if (stream && e.stream !== stream) return;
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
    <div class="log-panel">
      <div class="log-controls">
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
        <span class="log-count">{filtered().length} lines</span>
      </div>
      <Logs ref={pane} onScroll={onScroll}>
        <Show
          when={filtered().length}
          fallback={
            <div class="log-empty">No log lines{query() ? " match the filter" : " yet"}.</div>
          }
        >
          <For each={filtered()}>
            {(e) => (
              <div class={props.plain ? "logrow plain" : "logrow"}>
                <Show when={!props.plain}>
                  <span class="logrow-ts">{fmtTs(e.ts)}</span>
                  <span class={`logrow-stream ls-${e.stream}`}>{e.stream}</span>
                </Show>
                <span class="logrow-msg">{e.text}</span>
              </div>
            )}
          </For>
        </Show>
      </Logs>
    </div>
  );
}

function fmtTs(ts?: string | null): string {
  if (!ts) return "";
  const d = new Date(ts);
  if (isNaN(d.getTime())) return "";
  return d.toLocaleTimeString(undefined, { hour12: false });
}
