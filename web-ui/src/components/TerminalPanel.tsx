// Interactive terminal into a guest — a VM's vmlab-agent shell or a
// container's recovery shell — over the /vms/{vm}/tty or
// /containers/{name}/tty WebSocket. Binary frames are raw PTY bytes both
// ways; resizes go as a JSON text frame, which the server proxies to the lab
// daemon (→ the agent resizes the guest PTY).
//
// The session opens on demand (a Start button), not on mount — an idle page
// shouldn't hold a PTY session in the guest. Every start/reconnect opens a
// fresh session (multi-session: concurrent terminals are independent).

import { Show, createSignal, onCleanup } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Button, Card } from "@forge/ui";
import { RotateCcw, TerminalSquare } from "lucide-solid";
import * as api from "../api";

export type TerminalTarget =
  | { kind: "vm"; name: string }
  | { kind: "container"; name: string };

export default function TerminalPanel(p: {
  lab: string;
  target: TerminalTarget;
  title?: string;
  hint?: string;
}) {
  let host!: HTMLDivElement;
  const [started, setStarted] = createSignal(false);
  const [closed, setClosed] = createSignal(false);
  let term: Terminal | undefined;
  let ws: WebSocket | undefined;
  let fit: FitAddon | undefined;
  let observer: ResizeObserver | undefined;

  const path = () =>
    p.target.kind === "vm"
      ? `/api/labs/${encodeURIComponent(p.lab)}/vms/${encodeURIComponent(p.target.name)}/tty`
      : `/api/labs/${encodeURIComponent(p.lab)}/containers/${encodeURIComponent(p.target.name)}/tty`;

  const connect = () => {
    setClosed(false);
    ws?.close();
    const sock = new WebSocket(api.wsUrl(path()));
    sock.binaryType = "arraybuffer";
    ws = sock;
    sock.onopen = () => {
      fit?.fit();
      if (term) {
        sock.send(JSON.stringify({ resize: { cols: term.cols, rows: term.rows } }));
      }
      term?.focus();
    };
    sock.onmessage = (ev) => {
      if (ev.data instanceof ArrayBuffer) term?.write(new Uint8Array(ev.data));
    };
    sock.onclose = () => setClosed(true);
  };

  // First start: the xterm mounts into the (now visible) host, then the
  // socket opens. `started` flips before this runs, so `fit` measures the
  // real pane size.
  const start = () => {
    setStarted(true);
    if (!term) {
      term = new Terminal({
        fontFamily: "var(--font-mono, ui-monospace, monospace)",
        fontSize: 13,
        cursorBlink: true,
        theme: { background: "#0b0e14" },
      });
      fit = new FitAddon();
      term.loadAddon(fit);
      term.open(host);
      term.onData((d) => {
        if (ws?.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(d));
      });
      term.onResize(({ cols, rows }) => {
        if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ resize: { cols, rows } }));
      });
      observer = new ResizeObserver(() => fit?.fit());
      observer.observe(host);
    }
    connect();
  };

  onCleanup(() => {
    observer?.disconnect();
    ws?.close();
    term?.dispose();
  });

  return (
    <Card
      title={p.title ?? "Terminal"}
      action={
        started() && closed() ? (
          <Button icon={RotateCcw} onClick={connect}>
            Reconnect
          </Button>
        ) : undefined
      }
    >
      <Show when={!started()}>
        <div class="ctr-term-start">
          <Button variant="primary" icon={TerminalSquare} onClick={start}>
            Start terminal
          </Button>
          <span class="ctr-term-hint">
            {p.hint ??
              "Opens a shell inside the guest over virtio-serial (no guest network needed)."}
          </span>
        </div>
      </Show>
      <div ref={host} class="ctr-term" style={{ display: started() ? undefined : "none" }} />
    </Card>
  );
}
