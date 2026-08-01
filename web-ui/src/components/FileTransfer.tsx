// Moving one file between the browser and a guest, over the vmlab-agent
// channel — no guest network, no share, no host path in between.
//
// The Files tab is the *lab directory on the host*; this reaches inside the
// machine, which is a different filesystem entirely, so it lives on the
// machine's own page rather than in that tree. Both directions carry the file
// inline over the wire, so INLINE_FILE_LIMIT (generated from the protocol
// vocabulary) is the ceiling — checked here so an oversized file is refused
// before it is uploaded, and again by the daemon for every other caller.

import { Show, createSignal } from "solid-js";
import { Alert, Button, Card, Input } from "@forge/ui";
import { Download, Upload } from "lucide-solid";
import { pullGuestFile, pushGuestFile } from "../api";
import { INLINE_FILE_LIMIT } from "../protocol";

/** Byte sizes at the scale one file lands in. The store's helpers round to
 *  whole megabytes, which reads as "0 MB" for most of what moves through
 *  here. */
function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** Join a guest folder and a file name with the separator the folder uses, so
 *  a Windows path stays a Windows path and an empty folder means the root. */
function guestJoin(folder: string, name: string): string {
  const sep = folder.includes("\\") ? "\\" : "/";
  const base = folder.trim().replace(/[/\\]+$/, "");
  return `${base}${sep}${name}`;
}

/** The last segment of a guest path, whichever separator it uses. */
function baseName(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() ?? "download";
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default function FileTransfer(p: {
  lab: string;
  machine: string;
  running: boolean;
  hasAgent: boolean;
}) {
  const [file, setFile] = createSignal<File | null>(null);
  const [dest, setDest] = createSignal("");
  const [source, setSource] = createSignal("");
  const [busy, setBusy] = createSignal<"push" | "pull" | null>(null);
  const [note, setNote] = createSignal<string | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  const ready = () => p.running && p.hasAgent;

  const start = (which: "push" | "pull") => {
    setBusy(which);
    setNote(null);
    setError(null);
  };

  async function send() {
    const chosen = file();
    if (!chosen || !ready()) return;
    if (chosen.size > INLINE_FILE_LIMIT) {
      setNote(null);
      setError(
        `${chosen.name} is ${fmtSize(chosen.size)}; a transfer carries at most ` +
          `${fmtSize(INLINE_FILE_LIMIT)}.`,
      );
      return;
    }
    const to = guestJoin(dest(), chosen.name);
    start("push");
    try {
      const result = await pushGuestFile(p.lab, p.machine, to, chosen);
      setNote(`Sent ${fmtSize(result.len)} to ${to}`);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(null);
    }
  }

  async function fetchFile() {
    const from = source().trim();
    if (!from || !ready()) return;
    start("pull");
    try {
      const blob = await pullGuestFile(p.lab, p.machine, from);
      // Hand the bytes to the browser's own download machinery: an anchor with
      // `download`, clicked once, then the object URL released.
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = baseName(from);
      anchor.click();
      URL.revokeObjectURL(url);
      setNote(`Fetched ${fmtSize(blob.size)} from ${from}`);
    } catch (e) {
      setError(message(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card title="File transfer">
      <Show
        when={ready()}
        fallback={
          <div class="inspector-note">
            {p.hasAgent
              ? "Start the machine to move files in or out of it."
              : "This template predates the vmlab agent — rebuild it with `vmlab template build` to transfer files."}
          </div>
        }
      >
        <div class="xfer">
          <input
            class="xfer-pick"
            type="file"
            onChange={(e) => setFile(e.currentTarget.files?.[0] ?? null)}
          />
          <div class="xfer-row">
            <Input
              label="Destination folder in the guest"
              placeholder="/tmp"
              value={dest()}
              onInput={(e) => setDest(e.currentTarget.value)}
            />
            <Button
              icon={Upload}
              disabled={!file() || busy() !== null}
              onClick={() => void send()}
            >
              {busy() === "push" ? "Sending…" : "Send"}
            </Button>
          </div>
          <div class="xfer-row">
            <Input
              label="Guest file to fetch"
              placeholder="/var/log/syslog"
              value={source()}
              onInput={(e) => setSource(e.currentTarget.value)}
            />
            <Button
              icon={Download}
              disabled={!source().trim() || busy() !== null}
              onClick={() => void fetchFile()}
            >
              {busy() === "pull" ? "Fetching…" : "Fetch"}
            </Button>
          </div>
          <div class="inspector-note">
            One file at a time, up to {fmtSize(INLINE_FILE_LIMIT)}, over the agent channel.
          </div>
          <Show when={note()}>
            <Alert tone="success">{note()}</Alert>
          </Show>
          <Show when={error()}>
            <Alert tone="danger">{error()}</Alert>
          </Show>
        </div>
      </Show>
    </Card>
  );
}
