import { Show, createEffect, createSignal, onCleanup } from "solid-js";
import RFB from "@novnc/novnc";
import { wsUrl } from "../api";
import { Keyboard, Fullscreen } from "./icons";

export default function VncScreen(props: { lab: string; vm: string; powered: boolean }) {
  let container: HTMLDivElement | undefined;
  let rfb: RFB | null = null;
  const [status, setStatus] = createSignal("disconnected");

  const disconnect = () => {
    if (rfb) {
      try {
        rfb.disconnect();
      } catch {
        /* already gone */
      }
      rfb = null;
    }
  };

  const connect = () => {
    if (!container) return;
    disconnect();
    setStatus("connecting");
    try {
      const url = wsUrl(
        `/vnc/${encodeURIComponent(props.lab)}/${encodeURIComponent(props.vm)}`,
      );
      rfb = new RFB(container, url, {});
      // Scale the remote framebuffer to fit the fixed box rather than letting
      // it drive the layout. clipViewport must be off so noVNC scales (and
      // shrinks) instead of clipping to native resolution.
      rfb.scaleViewport = true;
      rfb.clipViewport = false;
      rfb.addEventListener("connect", () => setStatus("connected"));
      rfb.addEventListener("disconnect", () => setStatus("disconnected"));
    } catch {
      setStatus("error");
    }
  };

  // (Re)connect when the target VM or its power state changes.
  createEffect(() => {
    const powered = props.powered;
    void props.vm;
    void props.lab;
    disconnect();
    if (powered) {
      // Defer so the <Show> below has mounted the container element.
      queueMicrotask(connect);
    } else {
      setStatus("disconnected");
    }
  });
  onCleanup(disconnect);

  const dotColor = () =>
    status() === "connected"
      ? "var(--success-fg)"
      : status() === "error"
        ? "var(--danger-fg)"
        : status() === "connecting"
          ? "var(--warning-fg)"
          : "var(--fg-3)";

  return (
    <div class="vnc">
      <div class="vnctoolbar">
        <div class="vncconn">
          <span class="vncdot" style={`background:${dotColor()}`} />
          {status()}
        </div>
        <div class="vnctools">
          <button
            class="vtbtn"
            title="Send Ctrl+Alt+Del"
            onClick={() => rfb?.sendCtrlAltDel()}
          >
            <Keyboard />
          </button>
          <button
            class="vtbtn"
            title="Fullscreen"
            onClick={() => container?.requestFullscreen?.()}
          >
            <Fullscreen />
          </button>
        </div>
      </div>
      <div class="screen">
        <Show when={props.powered} fallback={<Off vm={props.vm} />}>
          <div ref={container} class="vncmount" />
        </Show>
      </div>
    </div>
  );
}

function Off(props: { vm: string }) {
  return (
    <div class="offscreen">
      <span class="offic">
        <svg
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          stroke-width="1.4"
          stroke-linecap="round"
          stroke-linejoin="round"
        >
          <rect x="2" y="3" width="20" height="14" rx="2" />
          <line x1="8" y1="21" x2="16" y2="21" />
          <line x1="12" y1="17" x2="12" y2="21" />
        </svg>
      </span>
      <div class="offt">{props.vm} is powered off</div>
      <div class="offs">No framebuffer · VNC disconnected</div>
    </div>
  );
}
