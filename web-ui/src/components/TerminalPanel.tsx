// Interactive shell into a container (PRD §18): an xterm.js terminal over
// the /containers/{name}/tty WebSocket. Binary frames are raw PTY bytes both
// ways; resizes go as a JSON text frame, which the server proxies to the lab
// daemon (→ TIOCSWINSZ on the guest PTY).

import { createSignal, onCleanup, onMount } from "solid-js";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { Button, Card } from "@forge/ui";
import { RotateCcw } from "lucide-solid";
import * as api from "../api";

export default function TerminalPanel(p: { lab: string; container: string }) {
  let host!: HTMLDivElement;
  const [closed, setClosed] = createSignal(false);
  let term: Terminal | undefined;
  let ws: WebSocket | undefined;
  let fit: FitAddon | undefined;
  let observer: ResizeObserver | undefined;

  const connect = () => {
    setClosed(false);
    ws?.close();
    const sock = new WebSocket(
      api.wsUrl(
        `/api/labs/${encodeURIComponent(p.lab)}/containers/${encodeURIComponent(p.container)}/tty`,
      ),
    );
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

  onMount(() => {
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
    connect();
  });

  onCleanup(() => {
    observer?.disconnect();
    ws?.close();
    term?.dispose();
  });

  return (
    <Card
      title="Terminal"
      action={
        closed() ? (
          <Button icon={RotateCcw} onClick={connect}>
            Reconnect
          </Button>
        ) : undefined
      }
    >
      <div ref={host} class="ctr-term" />
    </Card>
  );
}
