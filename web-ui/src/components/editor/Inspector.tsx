// The property panel beside the canvas: renders the selected block's full
// schema surface as descriptor-driven forms, including every child
// collection. All edits mutate the editor draft; nothing touches disk
// until Save builds the op batch.

import { For, Show, createMemo, createResource, createSignal } from "solid-js";
import { produce } from "solid-js/store";
import { Badge, Button, IconButton, Input, Modal, Select, Table, Tabs, Toggle } from "@forge/ui";
import {
  ChevronDown,
  ChevronRight,
  Disc,
  FolderSearch,
  FileCode2,
  HardDrive,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-solid";
import type {
  LabModel,
  SegmentModel,
  ShareModel,
  VolumeModel,
  WebPageModel,
} from "../../editor/model";
import { HEALTHCHECK_DEFAULTS, emptyWebAuth, emptyWebPage, uniqueName } from "../../editor/model";
import * as F from "../../editor/fields";
import { formatByteSize, formatMemory, parseByteSize } from "../../editor/bytesize";
import {
  addMachineNic,
  addNic,
  addPlaybookPlayDraft,
  addScriptEventHandler,
  editor,
  mergePlaybookDuplicates,
  playbookGroup,
  removeContainer,
  removeEventHandler,
  removePlaybookFolder,
  removePlaybookPlay,
  removeProvision,
  renamePlaybookPath,
  setPlaybookAllMachines,
  removeRemote,
  removeSegment,
  removeVm,
  renameContainer,
  renameRemote,
  renameSegment,
  renameVm,
  select,
  setEditor,
  storeTemplateFor,
} from "../../editor/store";
import { anyVmRunning, state } from "../../store";
import { dnsTable } from "../../api";
import type { DnsLiveRecord } from "../../api";
import { confirmDialog } from "../dialogs";
import BlockForm from "./BlockForm";
import BlockList from "./BlockList";
import FileBrowserModal from "./FileBrowserModal";
import SliderRow from "./SliderRow";
import TemplatePicker from "./TemplatePicker";
import ArtifactPicker from "./ArtifactPicker";

const mutate = (fn: (d: LabModel) => void) =>
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (d) fn(d);
    }),
  );

export default function Inspector(props: {
  onEditProvision: (path: string) => void;
  onEditPlaybook: (path: string) => void;
}) {
  const sel = () => editor.selection;
  const readOnly = () => anyVmRunning();

  return (
    <div class="inspector">
      <Show when={readOnly()}>
        <div class="inspector-lock" role="status">
          Configuration is read-only while any VM or container is powered on.
        </div>
      </Show>
      <fieldset class="inspector-fields" disabled={readOnly()}>
        <Show when={sel().kind === "lab"}>
          <LabInspector />
        </Show>
        <Show when={sel().kind === "nat"}>
          <NatInspector />
        </Show>
        <Show when={sel().kind === "remote"}>
          <RemoteInspector host={(sel() as { host: string }).host} />
        </Show>
        <Show when={sel().kind === "vm" && editor.draft?.vms[(sel() as any).index]}>
          <VmInspector index={(sel() as { index: number }).index} />
        </Show>
        <Show
          when={sel().kind === "container" && editor.draft?.containers[(sel() as any).index]}
        >
          <ContainerInspector index={(sel() as { index: number }).index} />
        </Show>
        <Show
          when={sel().kind === "provision" && editor.draft?.provisions[(sel() as any).index]}
        >
          <ProvisionInspector
            index={(sel() as { index: number }).index}
            onEdit={props.onEditProvision}
          />
        </Show>
        <Show when={sel().kind === "playbook"}>
          <PlaybookInspector
            path={(sel() as { path: string }).path}
            play={(sel() as { play?: string }).play}
          />
        </Show>
      </fieldset>
      {/* Outside the read-only fieldset on purpose: the segment tabs stay
          navigable while machines run so the DNS tab's live registrations
          (runtime state, only meaningful while up) are reachable; it locks
          its own fields per tab. */}
      <Show when={sel().kind === "segment" && editor.draft?.segments[(sel() as any).index]}>
        <SegmentInspector index={(sel() as { index: number }).index} />
      </Show>
      {/* Outside the read-only fieldset on purpose: playbook FILES stay
          editable while machines run — that is the edit→check dev loop. */}
      <Show when={sel().kind === "playbook"}>
        <Button
          icon={FileCode2}
          variant="primary"
          onClick={() => props.onEditPlaybook((sel() as { path: string }).path)}
        >
          Edit in Files tab
        </Button>
      </Show>
    </div>
  );
}

// --- lab ----------------------------------------------------------------------

function LabInspector() {
  const lab = () => editor.draft!;
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">lab</span>
        <span class="inspector-name">{lab().name}</span>
      </div>
      <BlockList
        title="Event handlers"
        items={lab().handlers as any}
        fields={F.HANDLER_FIELDS}
        summary={(h) => `${h.event || "(event)"} → ${h.run || "(script)"}`}
        onAdd={() =>
          mutate((d) =>
            d.handlers.push({ span: null, event: "vm.ready", run: "", targets: [] }),
          )
        }
        onRemove={(i) => mutate((d) => void d.handlers.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.handlers[i] as any)[key] = v))}
      />
      <BlockList
        title="DNS records (lab-wide)"
        items={lab().records as any}
        fields={F.RECORD_FIELDS}
        summary={(r) => `${r.name || "(name)"} → ${r.ip || "?"}`}
        onAdd={() => mutate((d) => d.records.push({ span: null, name: "", ip: "" }))}
        onRemove={(i) => mutate((d) => void d.records.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.records[i] as any)[key] = v))}
      />
      <BlockList
        title="DNS sinkholes (lab-wide)"
        items={lab().sinkholes as any}
        fields={F.SINKHOLE_FIELDS}
        summary={(s) => s.pattern || "(pattern)"}
        onAdd={() =>
          mutate((d) => d.sinkholes.push({ span: null, pattern: "", mode: "nxdomain" }))
        }
        onRemove={(i) => mutate((d) => void d.sinkholes.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.sinkholes[i] as any)[key] = v))}
      />
      <Show when={editor.templatesInFile.length}>
        <div class="inspector-note">
          This file also defines {editor.templatesInFile.length} template block(s) — edit those
          in the Config page.
        </div>
      </Show>
    </>
  );
}

function ProvisionInspector(props: { index: number; onEdit: (path: string) => void }) {
  const provision = () => editor.draft!.provisions[props.index];
  const [newEvent, setNewEvent] = createSignal("vm.ready");
  const handlers = createMemo(() =>
    editor.draft!.handlers
      .map((handler, index) => ({ handler, index }))
      .filter(({ handler }) => handler.run === provision().script),
  );
  async function remove() {
    if (
      await confirmDialog({
        title: `Delete provision "${provision().script}"?`,
        body: "The provision block will be removed. Its script file will be preserved.",
        confirmLabel: "Delete provision",
        danger: true,
      })
    ) {
      removeProvision(props.index);
    }
  }
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">provision</span>
        <span class="inspector-name">#{props.index + 1}</span>
        <Button variant="danger" size="sm" icon={Trash2} onClick={() => void remove()}>
          Delete
        </Button>
      </div>
      <div class="provision-inspector-path">{provision().script}</div>
      <Badge tone={provision().vms.length ? "info" : "neutral"}>
        {provision().vms.length ? `${provision().vms.length} targeted` : "LAB-WIDE"}
      </Badge>
      <Show when={provision().vms.length}>
        <div class="remote-attached">
          <For each={provision().vms}>
            {(name) => (
              <button
                class="remote-attached-row"
                onClick={() => {
                  const vm = editor.draft!.vms.findIndex((candidate) => candidate.name === name);
                  const container = editor.draft!.containers.findIndex(
                    (candidate) => candidate.name === name,
                  );
                  if (vm >= 0) select({ kind: "vm", index: vm });
                  else if (container >= 0) select({ kind: "container", index: container });
                }}
              >
                {name}
              </button>
            )}
          </For>
        </div>
      </Show>
      <Button icon={FileCode2} variant="primary" onClick={() => props.onEdit(provision().script)}>
        Edit script
      </Button>
      <div class="inspector-section-title">Event handlers</div>
      <div class="provision-event-add">
        <Select
          label="Event"
          options={(editor.catalog.meta?.events ?? []).map((event) => ({ value: event, label: event }))}
          value={newEvent()}
          onChange={setNewEvent}
        />
        <Button
          icon={Plus}
          disabled={!newEvent() || handlers().some(({ handler }) => handler.event === newEvent())}
          onClick={() => addScriptEventHandler(provision().script, newEvent())}
        >
          Add handler
        </Button>
      </div>
      <Show
        when={handlers().length}
        fallback={<div class="inspector-note">No events are mapped to this script.</div>}
      >
        <div class="provision-event-list">
          <For each={handlers()}>
            {({ handler, index }) => (
              <div class="provision-event-row">
                <div>
                  <strong>{handler.event}</strong>
                  <span>
                    {handler.targets.length
                      ? `${handler.targets.length} targeted`
                      : handler.event.startsWith("vm.")
                        ? "all VMs"
                        : handler.event.startsWith("container.")
                          ? "all containers"
                          : handler.event.startsWith("snapshot.")
                            ? "all machines"
                            : "global"}
                  </span>
                </div>
                <IconButton
                  icon={Trash2}
                  label={`Remove ${handler.event} handler`}
                  onClick={() => removeEventHandler(index)}
                />
              </div>
            )}
          </For>
        </div>
      </Show>
      <div class="inspector-note">
        Drag the TARGETS port on the canvas onto VMs or containers. With no targets, this script
        runs lab-wide in the final provisioning pass.
      </div>
    </>
  );
}

function PlaybookInspector(props: { path: string; play?: string }) {
  const group = () => playbookGroup(props.path);
  const card = () =>
    props.play !== undefined
      ? group()?.cards.find((candidate) => candidate.play === props.play)
      : undefined;
  return (
    <Show
      when={props.play !== undefined && card()}
      fallback={<PlaybookFolderInspector path={props.path} />}
    >
      <PlayCardInspector path={props.path} play={props.play!} />
    </Show>
  );
}

function playCardBadge(card: {
  allMachines: boolean;
  targets: string[];
  blockIndex: number | null;
}) {
  return card.allMachines
    ? { tone: "success" as const, label: "all machines" }
    : card.targets.length
      ? { tone: "success" as const, label: `${card.targets.length} targeted` }
      : { tone: "neutral" as const, label: "not run" };
}

function PlaybookFolderInspector(props: { path: string }) {
  const group = () => playbookGroup(props.path);
  const plays = () => group()?.plays;
  const [newPlay, setNewPlay] = createSignal("");
  const pathIssue = () => {
    const p = props.path;
    if (!p) return "Path is required";
    if (p.startsWith("/")) return "Path must be relative to the lab root";
    if (p.split("/").some((part) => part === "..")) return "Path cannot leave the lab root";
    return null;
  };
  const blockCount = () =>
    editor.draft?.playbooks.filter((playbook) => playbook.path === props.path).length ?? 0;
  async function remove() {
    if (
      await confirmDialog({
        title: `Delete playbook "${props.path}"?`,
        body: blockCount()
          ? `${blockCount()} playbook block(s) will be removed from vmlab.wcl. The folder and its files will be preserved.`
          : "The folder and its files will be preserved.",
        confirmLabel: "Delete playbook",
        danger: true,
      })
    ) {
      removePlaybookFolder(props.path);
    }
  }
  function addPlay() {
    const play = newPlay().trim();
    if (!play) return;
    addPlaybookPlayDraft(props.path, play);
    setNewPlay("");
    select({ kind: "playbook", path: props.path, play });
  }
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">playbook</span>
        <span class="inspector-name">{props.path.split("/").pop() || props.path}</span>
        <Button variant="danger" size="sm" icon={Trash2} onClick={() => void remove()}>
          Delete
        </Button>
      </div>
      <Input
        label="Folder"
        value={props.path}
        placeholder="playbooks/baseline"
        error={pathIssue() !== null}
        help={pathIssue() ?? "config-weave playbook folder, relative to the lab root"}
        onInput={(e: InputEvent) =>
          renamePlaybookPath(props.path, (e.currentTarget as HTMLInputElement).value)
        }
      />
      <Show
        when={typeof plays() === "object" && (plays() as { error: string | null }).error}
        keyed
      >
        {(error) => (
          <div class="inspector-note inspector-warn">
            playbook.wcl could not be scanned: {error}
          </div>
        )}
      </Show>
      <div class="inspector-subhead">Plays</div>
      <Show
        when={group()?.cards.length}
        fallback={
          <div class="inspector-note">
            {plays() === "loading" || plays() === undefined
              ? "Scanning playbook.wcl…"
              : "No plays yet — add one below; the folder is scaffolded on save."}
          </div>
        }
      >
        <div class="remote-attached">
          <For each={group()?.cards ?? []}>
            {(playCard) => {
              const badge = () => playCardBadge(playCard);
              return (
                <button
                  class="remote-attached-row"
                  onClick={() =>
                    select({ kind: "playbook", path: props.path, play: playCard.play })
                  }
                >
                  {playCard.play}
                  {playCard.missingFromFolder || playCard.duplicateBlockIndexes.length
                    ? " ⚠"
                    : ""}
                  <Badge tone={badge().tone}>{badge().label}</Badge>
                </button>
              );
            }}
          </For>
        </div>
      </Show>
      <div class="inspector-row">
        <Input
          label="Add play"
          value={newPlay()}
          placeholder="baseline"
          onInput={(e: InputEvent) => setNewPlay((e.currentTarget as HTMLInputElement).value)}
          onKeyDown={(e: KeyboardEvent) => {
            if (e.key === "Enter") addPlay();
          }}
        />
        <Button size="sm" icon={Plus} onClick={addPlay}>
          Add
        </Button>
      </div>
      <Show when={typeof plays() === "object" && !(plays() as { exists: boolean }).exists}>
        <div class="inspector-note">
          The folder has no playbook.wcl yet — save the lab config, then open it in the Files
          tab to scaffold its files.
        </div>
      </Show>
      <div class="inspector-note">
        Each play wears its own TARGETS port on the canvas — drag it onto the VMs or
        containers that play converges. A play with no connections does not run.
      </div>
    </>
  );
}

function PlayCardInspector(props: { path: string; play: string }) {
  const group = () => playbookGroup(props.path);
  const card = () => group()?.cards.find((candidate) => candidate.play === props.play);
  async function stopRunning() {
    if (
      await confirmDialog({
        title: `Stop running play "${props.play}"?`,
        body: "Removes its playbook block from vmlab.wcl. The files are preserved.",
        confirmLabel: "Stop running",
        danger: true,
      })
    ) {
      removePlaybookPlay(props.path, props.play);
    }
  }
  async function toggleAllMachines(on: boolean) {
    const targets = card()?.targets ?? [];
    if (
      on &&
      targets.length &&
      !(await confirmDialog({
        title: "Run on all machines?",
        body: `The ${targets.length} explicit target(s) will be replaced by an all-machines scope.`,
        confirmLabel: "Run on all",
      }))
    ) {
      return;
    }
    setPlaybookAllMachines(props.path, props.play, on);
  }
  return (
    <Show when={card()} keyed>
      {(playCard) => (
        <>
          <div class="inspector-head">
            <span class="inspector-kind">play</span>
            <span class="inspector-name">{props.play}</span>
            <Show when={playCard.blockIndex !== null}>
              <Button variant="danger" size="sm" icon={Trash2} onClick={() => void stopRunning()}>
                Stop
              </Button>
            </Show>
          </div>
          <button
            class="inspector-backlink"
            onClick={() => select({ kind: "playbook", path: props.path })}
          >
            ← {props.path}
          </button>
          <Show when={playCard.description}>
            <div class="inspector-note">{playCard.description}</div>
          </Show>
          <Show when={playCard.missingFromFolder}>
            <div class="inspector-note inspector-warn">
              This play is referenced in vmlab.wcl but not defined in the folder's
              playbook.wcl.
            </div>
          </Show>
          <Show when={playCard.duplicateBlockIndexes.length}>
            <div class="inspector-note inspector-warn">
              {playCard.duplicateBlockIndexes.length + 1} playbook blocks declare this play.
              <Button
                size="sm"
                onClick={() => mergePlaybookDuplicates(props.path, props.play)}
              >
                Merge targets
              </Button>
            </div>
          </Show>
          <Toggle
            checked={playCard.allMachines}
            onChange={(on: boolean) => void toggleAllMachines(on)}
          >
            Run on all machines
          </Toggle>
          <Badge tone={playCardBadge(playCard).tone}>{playCardBadge(playCard).label}</Badge>
          <Show when={playCard.targets.length}>
            <div class="remote-attached">
              <For each={playCard.targets}>
                {(name) => (
                  <button
                    class="remote-attached-row"
                    onClick={() => {
                      const vm = editor.draft!.vms.findIndex(
                        (candidate) => candidate.name === name,
                      );
                      const container = editor.draft!.containers.findIndex(
                        (candidate) => candidate.name === name,
                      );
                      if (vm >= 0) select({ kind: "vm", index: vm });
                      else if (container >= 0) select({ kind: "container", index: container });
                    }}
                  >
                    {name}
                  </button>
                )}
              </For>
            </div>
          </Show>
          <div class="inspector-note">
            Drag this play's port on the canvas onto VMs or containers. Deleting its last
            connection stops the play (unless "all machines" is on).
          </div>
        </>
      )}
    </Show>
  );
}

function NatInspector() {
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">wan</span>
        <span class="inspector-name">NAT · internet uplink</span>
      </div>
      <div class="inspector-note">
        The lab's internet uplink. Plug a VM's NIC into it for direct NAT egress (
        <code>nat = true</code> on the NIC), or cable a switch's side port to it to give that
        whole segment internet access (<code>nat = true</code> on the segment). It isn't
        declared in the config — it appears through those connections.
      </div>
    </>
  );
}

// --- remote vmlab ---------------------------------------------------------------

function splitPeerAddress(value: string): { hostname: string; port: string } {
  const colon = value.lastIndexOf(":");
  if (colon < 0) return { hostname: value, port: "" };
  const port = value.slice(colon + 1);
  // The runtime's current host[:port] syntax is hostname/IPv4-oriented. Only
  // split a numeric suffix so a malformed/partial hostname is never silently
  // rewritten while the user types.
  return /^\d+$/.test(port)
    ? { hostname: value.slice(0, colon), port }
    : { hostname: value, port: "" };
}

function joinPeerAddress(hostname: string, port: string): string {
  return port ? `${hostname}:${port}` : hostname;
}

function RemoteInspector(props: { host: string }) {
  const address = createMemo(() => splitPeerAddress(props.host));
  const attached = () =>
    (editor.draft?.segments ?? [])
      .map((s, i) => ({ seg: s, index: i }))
      .filter(({ seg }) => seg.connect?.host === props.host);
  const del = async () => {
    if (
      await confirmDialog({
        title: `Delete remote vmlab "${props.host || "(no address)"}"?`,
        body: "Segments cabled to it lose their peer link (they stay global).",
        confirmLabel: "Delete",
        danger: true,
      })
    ) {
      removeRemote(props.host);
    }
  };
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">remote</span>
        <span class="inspector-name">Remote vmlab</span>
        <IconButton icon={Trash2} label="Delete remote vmlab" onClick={del} />
      </div>
      <div class="remote-address-fields">
        <Input
          label="IP / hostname"
          value={address().hostname}
          placeholder="otherhost"
          error={!address().hostname.trim()}
          onInput={(e: InputEvent) =>
            renameRemote(
              props.host,
              joinPeerAddress((e.currentTarget as HTMLInputElement).value, address().port),
            )
          }
        />
        <Input
          label="Port"
          type="number"
          min="1"
          max="65535"
          value={address().port}
          placeholder="13947"
          help="Default: 13947"
          error={
            address().port !== "" &&
            (Number(address().port) < 1 || Number(address().port) > 65535)
          }
          onInput={(e: InputEvent) =>
            renameRemote(
              props.host,
              joinPeerAddress(address().hostname, (e.currentTarget as HTMLInputElement).value),
            )
          }
        />
      </div>
      <div class="inspector-section-title">Bridged segments</div>
      <Show
        when={attached().length}
        fallback={
          <div class="inspector-note">
            Not cabled to any segment yet — drag a switch's side port onto this node (or this
            node's port onto a switch) to bridge that segment to the remote vmlab instance.
          </div>
        }
      >
        <div class="remote-attached">
          <For each={attached()}>
            {({ seg, index }) => (
              <button
                type="button"
                class="remote-attached-row"
                onClick={() => select({ kind: "segment", index })}
              >
                {seg.name}
              </button>
            )}
          </For>
        </div>
      </Show>
      <div class="inspector-note">
        Cabling writes <code>connect {"{ host }"}</code> and <code>global = true</code> on the
        segment: both vmlab instances share the segment over a TCP trunk, authenticated by the
        <code> psk</code> in each host's config (<code>~/.config/vmlab/config.wcl</code>). The
        LED and cable animate while the trunk is up.
      </div>
    </>
  );
}

// --- vm -----------------------------------------------------------------------

const GIB = 1024 * 1024 * 1024;
const MIB = 1024 * 1024;

/** The "Web" tab shared by VM and container inspectors: a list of `web {}`
 *  pages, each with an optional nested `auth {}` block whose fields switch on
 *  the chosen method. `pages()` reads the live array; `edit` runs a mutation
 *  against it. */
function WebPagesTab(props: {
  pages: () => WebPageModel[];
  edit: (fn: (list: WebPageModel[]) => void) => void;
}) {
  return (
    <div class="stack">
      <For each={props.pages()}>
        {(page, i) => (
          <div class="web-page-block">
            <div class="web-page-head">
              <Input
                value={page.name}
                placeholder="page name"
                title="Page name (DNS label); unique per machine"
                onInput={(e) => props.edit((list) => (list[i()].name = e.currentTarget.value))}
              />
              <IconButton
                icon={Trash2}
                label={`Remove ${page.name || "page"}`}
                onClick={() => props.edit((list) => list.splice(i(), 1))}
              />
            </div>
            <BlockForm
              fields={F.WEB_FIELDS}
              value={page as any}
              onSet={(key, v) => props.edit((list) => ((list[i()] as any)[key] = v))}
            />
            <div class="field-row">
              <div class="field-row-label" title="Proxy-injected upstream credentials">
                Upstream auth
              </div>
              <div class="field-row-control">
                <Toggle
                  checked={page.auth !== null}
                  onChange={(on) =>
                    props.edit((list) => (list[i()].auth = on ? emptyWebAuth() : null))
                  }
                />
              </div>
            </div>
            <Show when={page.auth}>
              <FieldRowMethod
                value={page.auth!.method}
                onChange={(m) => props.edit((list) => (list[i()].auth!.method = m))}
              />
              <BlockForm
                fields={F.WEB_AUTH_FIELDS[page.auth!.method] ?? []}
                value={page.auth as any}
                onSet={(key, v) => props.edit((list) => ((list[i()].auth as any)[key] = v))}
              />
            </Show>
          </div>
        )}
      </For>
      <Button
        size="sm"
        icon={Plus}
        onClick={() =>
          props.edit((list) => list.push(emptyWebPage(uniqueName("page", list.map((p) => p.name)))))
        }
      >
        Add web page
      </Button>
    </div>
  );
}

/** The auth method Select, rendered as a labeled field row. */
function FieldRowMethod(props: { value: string; onChange: (m: string) => void }) {
  return (
    <div class="field-row">
      <div class="field-row-label" title={F.WEB_AUTH_METHOD.doc}>
        {F.WEB_AUTH_METHOD.label}
      </div>
      <div class="field-row-control">
        <Select
          value={props.value}
          options={(F.WEB_AUTH_METHOD.options ?? []).map((o) => ({ value: o, label: o }))}
          onChange={props.onChange}
        />
      </div>
    </div>
  );
}

function VmInspector(props: { index: number }) {
  const vm = () => editor.draft!.vms[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.vms[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete VM "${vm().name}"?`,
        body: "Removed from the config on the next save.",
        danger: true,
      })
    ) {
      removeVm(props.index);
    }
  }

  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">vm</span>
        <Input
          value={vm().name}
          onInput={(e) => renameVm(props.index, e.currentTarget.value)}
          title="VM name (DNS label); references update automatically"
        />
        <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
          Delete
        </Button>
      </div>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "hardware", label: "Hardware" },
          { id: "storage", label: "Storage" },
          { id: "network", label: "Network", count: vm().nics.length || undefined },
          { id: "sharing", label: "Shares" },
          { id: "web", label: "Web", count: vm().web.length || undefined },
          { id: "overrides", label: "Overrides" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <TemplatePicker
          value={vm().template}
          onChange={(v) => setField("template", v)}
          profile={vm().profile}
          onMeta={setField}
        />
      </Show>
      <Show when={tab() === "hardware"}>
        <SliderRow
          label="vCPUs"
          doc="vCPU count; inherited from template→profile if not set"
          value={vm().cpus}
          fallback={storeTemplateFor(vm().template)?.cpus ?? null}
          min={1}
          max={editor.catalog.host?.cpus ?? 16}
          step={1}
          fmt={(v) => `${v} vCPU`}
          editText={(v) => String(v)}
          parse={(t) => {
            const n = parseInt(t, 10);
            return Number.isFinite(n) ? n : null;
          }}
          onChange={(v) => setField("cpus", v)}
        />
        <SliderRow
          label="Memory"
          doc="RAM; inherited from template→profile if not set"
          value={vm().memory}
          fallback={storeTemplateFor(vm().template)?.memory ?? null}
          min={256 * MIB}
          max={editor.catalog.host?.memory ?? 16 * GIB}
          step={256 * MIB}
          fmt={formatMemory}
          parse={parseByteSize}
          onChange={(v) => setField("memory", v)}
        />
        <BlockForm fields={F.VM_HARDWARE} value={vm() as any} onSet={setField} />
        <div class="field-row">
          <div class="field-row-label" title="GPU acceleration (passthrough / virgl / vulkan)">
            GPU
          </div>
          <div class="field-row-control">
            <Toggle
              checked={vm().gpu !== null}
              onChange={(on) =>
                setField("gpu", on ? { span: null, mode: "virgl", address: null } : null)
              }
            />
          </div>
        </div>
        <Show when={vm().gpu}>
          <BlockForm
            fields={F.GPU_FIELDS}
            value={vm().gpu as any}
            onSet={(key, v) => mutate((d) => ((d.vms[props.index].gpu as any)[key] = v))}
          />
        </Show>
      </Show>
      <Show when={tab() === "storage"}>
        <VmStorage index={props.index} />
      </Show>
      <Show when={tab() === "network"}>
        <BlockList
          title="NICs"
          items={vm().nics as any}
          fields={F.NIC_FIELDS}
          summary={(n) =>
            n.nat ? `NAT${n.ip ? ` · ${n.ip}` : ""}` : `${n.segment ?? "(no segment)"}${n.ip ? ` · ${n.ip}` : ""}`
          }
          addLabel="Add NIC"
          onAdd={() => addNic(props.index, editor.draft!.segments[0]?.name ?? null)}
          onRemove={(i) => mutate((d) => void d.vms[props.index].nics.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const nic = d.vms[props.index].nics[i] as any;
              nic[key] = v;
              // Picking a segment moves a NAT-attached NIC off the NAT bus.
              if (key === "segment" && v) nic.nat = false;
            })
          }
        />
        <div class="inspector-note">
          No NICs = air-gapped VM. Drag a connection onto the NAT bus in the canvas for
          internet egress.
        </div>
      </Show>
      <Show when={tab() === "sharing"}>
        <BlockList
          title="SMB shares"
          items={vm().shares as any}
          fields={F.SHARE_FIELDS}
          summary={(s: ShareModel) => (s.guest ? `${s.host || "?"} → ${s.guest}` : "(share)")}
          addLabel="Add share"
          onAdd={() =>
            mutate((d) =>
              d.vms[props.index].shares.push({
                span: null,
                host: "",
                guest: "",
                readonly: false,
                smb1: false,
                name: "",
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.vms[props.index].shares.splice(i, 1))}
          onSet={(i, key, v) => mutate((d) => ((d.vms[props.index].shares[i] as any)[key] = v))}
        />
      </Show>
      <Show when={tab() === "web"}>
        <WebPagesTab
          pages={() => vm().web}
          edit={(fn) => mutate((d) => fn(d.vms[props.index].web))}
        />
      </Show>
      <Show when={tab() === "overrides"}>
        <div class="inspector-note">
          Everything here is normally supplied by the template/profile — set a value only to
          override it for this VM.
        </div>
        <BlockForm fields={F.VM_OVERRIDES} value={vm() as any} onSet={setField} />
      </Show>
    </>
  );
}

/** The Storage tab: one unified disk list — extra HDDs (`disk {}` blocks)
 *  plus at most one CD-ROM (the `cdrom` attr), added through a kind picker;
 *  the CD-ROM's ISO is chosen with the server-side file browser. */
function VmStorage(props: { index: number }) {
  const vm = () => editor.draft!.vms[props.index];
  // Add-disk flow: pick the kind, then (for HDDs) lock in a size.
  const [addPhase, setAddPhase] = createSignal<null | "kind" | "hdd">(null);
  const [hddSize, setHddSize] = createSignal("10GiB");
  const [browseOpen, setBrowseOpen] = createSignal(false);
  const [openDisk, setOpenDisk] = createSignal<number | null>(null);

  // The lab root (dirname of vmlab.wcl) anchors browsing, and ISO paths
  // inside it are stored lab-relative.
  const labRoot = createMemo(() => {
    const p = editor.path;
    const cut = p.lastIndexOf("/");
    return cut > 0 ? p.slice(0, cut) : "/";
  });

  const setCdrom = (v: string | null) =>
    mutate((d) => (d.vms[props.index].cdrom = v));

  function pickIso(abs: string) {
    const root = labRoot();
    setCdrom(abs.startsWith(`${root}/`) ? abs.slice(root.length + 1) : abs);
    setBrowseOpen(false);
  }

  function addHdd() {
    const size = parseByteSize(hddSize());
    if (size == null) return;
    setAddPhase(null);
    mutate((d) => {
      const disks = d.vms[props.index].extra_disks;
      disks.push({
        span: null,
        name: uniqueName("data", disks.map((x) => x.name)),
        size,
        from: null,
      });
    });
  }

  return (
    <>
      <div class="block-list">
        <div class="block-list-head">
          <span class="block-list-title">Disks</span>
          <Button size="sm" variant="ghost" icon={HardDrive} onClick={() => setAddPhase("kind")}>
            Add disk
          </Button>
        </div>
        <Show
          when={vm().cdrom || vm().extra_disks.length}
          fallback={<div class="block-list-empty">none — the template supplies the boot disk</div>}
        >
          <Show when={vm().cdrom}>
            <div class="block-list-item">
              <div class="block-list-row">
                <Disc size={14} />
                <span class="block-list-summary">CD-ROM · {vm().cdrom}</span>
                <IconButton
                  icon={FolderSearch}
                  label="Choose another ISO"
                  onClick={() => setBrowseOpen(true)}
                />
                <IconButton icon={Trash2} label="Remove CD-ROM" onClick={() => setCdrom(null)} />
              </div>
            </div>
          </Show>
          <For each={vm().extra_disks}>
            {(disk, i) => (
              <div class="block-list-item">
                <div
                  class="block-list-row"
                  onClick={() => setOpenDisk(openDisk() === i() ? null : i())}
                >
                  {openDisk() === i() ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                  <span class="block-list-summary">
                    {disk.name || "(disk)"}
                    {disk.size != null ? ` · ${formatByteSize(disk.size)}` : ""}
                  </span>
                  <IconButton
                    icon={Trash2}
                    label="Remove disk"
                    onClick={(e: MouseEvent) => {
                      e.stopPropagation();
                      if (openDisk() === i()) setOpenDisk(null);
                      mutate((d) => void d.vms[props.index].extra_disks.splice(i(), 1));
                    }}
                  />
                </div>
                <Show when={openDisk() === i()}>
                  <div class="block-list-body">
                    <BlockForm
                      fields={F.DISK_FIELDS}
                      value={disk as unknown as Record<string, unknown>}
                      onSet={(key, v) =>
                        mutate((d) => ((d.vms[props.index].extra_disks[i()] as any)[key] = v))
                      }
                    />
                  </div>
                </Show>
              </div>
            )}
          </For>
        </Show>
      </div>

      <Modal
        open={addPhase() !== null}
        onClose={() => setAddPhase(null)}
        title={addPhase() === "hdd" ? "New hard disk" : "Add disk"}
        footer={
          <>
            <Button variant="ghost" onClick={() => setAddPhase(null)}>
              Cancel
            </Button>
            <Show when={addPhase() === "hdd"}>
              <Button
                variant="primary"
                disabled={parseByteSize(hddSize()) == null}
                onClick={addHdd}
              >
                Add disk
              </Button>
            </Show>
          </>
        }
      >
        <Show when={addPhase() === "kind"}>
          <div class="disk-kind-choices">
            <Button
              icon={HardDrive}
              onClick={() => {
                setHddSize("10GiB");
                setAddPhase("hdd");
              }}
            >
              Hard disk (blank)
            </Button>
            <Button
              icon={Disc}
              disabled={vm().cdrom !== null}
              title={vm().cdrom !== null ? "Only one CD-ROM per VM" : undefined}
              onClick={() => {
                setAddPhase(null);
                setBrowseOpen(true);
              }}
            >
              CD-ROM (browse for an ISO)
            </Button>
          </div>
        </Show>
        <Show when={addPhase() === "hdd"}>
          <div class="disk-kind-choices">
            <Input
              label="Disk size"
              help="Blank disk created in the VM's directory on first boot, e.g. `10GiB`"
              value={hddSize()}
              error={parseByteSize(hddSize()) == null}
              onInput={(e) => setHddSize(e.currentTarget.value)}
            />
          </div>
        </Show>
      </Modal>

      <FileBrowserModal
        open={browseOpen()}
        title="Choose an ISO on the server"
        start={labRoot()}
        extensions={[".iso"]}
        onClose={() => setBrowseOpen(false)}
        onPick={pickIso}
      />
    </>
  );
}

// --- container ------------------------------------------------------------------

/** Focused inspector for a `container {}` block: image, dependencies,
 *  micro-VM hardware, networking, volumes, environment, and healthcheck. */
function ContainerInspector(props: { index: number }) {
  const ctr = () => editor.draft!.containers[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.containers[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete container "${ctr().name}"?`,
        body: "Removed from the config on the next save.",
        danger: true,
      })
    ) {
      removeContainer(props.index);
    }
  }

  const setHealth = (key: string, v: unknown) =>
    mutate((d) => {
      const h = d.containers[props.index].healthcheck;
      if (!h) return;
      // Clearing a timing/count field falls back to the schema default —
      // the model always carries concrete values (matching the DTO).
      const value =
        v == null && key in HEALTHCHECK_DEFAULTS
          ? HEALTHCHECK_DEFAULTS[key as keyof typeof HEALTHCHECK_DEFAULTS]
          : v;
      (h as any)[key] = value;
    });

  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">container</span>
        <Input
          value={ctr().name}
          onInput={(e) => renameContainer(props.index, e.currentTarget.value)}
          title="Container name (DNS label, shared namespace with VMs); references update automatically"
        />
        <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
          Delete
        </Button>
      </div>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "hardware", label: "Hardware" },
          { id: "network", label: "Network", count: ctr().nics.length || undefined },
          { id: "storage", label: "Volumes", count: ctr().volumes.length || undefined },
          { id: "env", label: "Env", count: ctr().env.length || undefined },
          { id: "health", label: "Health" },
          { id: "web", label: "Web", count: ctr().web.length || undefined },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <ArtifactPicker
          kind="container"
          value={ctr().image}
          onSelect={(reference) => setField("image", reference)}
        />
      </Show>
      <Show when={tab() === "hardware"}>
        <div class="inspector-note">
          The container runs in its own micro-VM — these size that VM (defaults: 1 vCPU,
          256MiB).
        </div>
        <SliderRow
          label="vCPUs"
          doc="vCPU count for the micro-VM (> 0); default 1"
          value={ctr().cpus}
          fallback={1}
          min={1}
          max={editor.catalog.host?.cpus ?? 16}
          step={1}
          fmt={(v) => `${v} vCPU`}
          editText={(v) => String(v)}
          parse={(t) => {
            const n = parseInt(t, 10);
            return Number.isFinite(n) ? n : null;
          }}
          unsetLabel="default"
          onChange={(v) => setField("cpus", v)}
        />
        <SliderRow
          label="Memory"
          doc="RAM for the micro-VM, e.g. `512MiB`; default 256MiB"
          value={ctr().memory}
          fallback={256 * MIB}
          min={128 * MIB}
          max={editor.catalog.host?.memory ?? 16 * GIB}
          step={128 * MIB}
          fmt={formatMemory}
          parse={parseByteSize}
          unsetLabel="default"
          onChange={(v) => setField("memory", v)}
        />
      </Show>
      <Show when={tab() === "network"}>
        <BlockList
          title="NICs"
          items={ctr().nics as any}
          fields={F.NIC_FIELDS}
          summary={(n) =>
            n.nat
              ? `NAT${n.ip ? ` · ${n.ip}` : ""}`
              : `${n.segment ?? "(no segment)"}${n.ip ? ` · ${n.ip}` : ""}`
          }
          addLabel="Add NIC"
          onAdd={() =>
            addMachineNic("container", props.index, editor.draft!.segments[0]?.name ?? null)
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].nics.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const nic = d.containers[props.index].nics[i] as any;
              nic[key] = v;
              // Picking a segment moves a NAT-attached NIC off the NAT bus.
              if (key === "segment" && v) nic.nat = false;
            })
          }
        />
        <div class="inspector-note">
          No NICs = air-gapped container (exec/copy/logs still work via the agent channel).
        </div>
        <BlockList
          title="Port forwards (host → container)"
          items={ctr().ports as any}
          fields={F.PORT_FIELDS}
          summary={(p) => `:${p.host || "?"} → :${p.container || "?"}${p.proto && p.proto !== "tcp" ? ` ${p.proto}` : ""}`}
          addLabel="Add port"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].ports.push({
                span: null,
                host: 8080,
                container: 80,
                proto: "tcp",
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].ports.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => ((d.containers[props.index].ports[i] as any)[key] = v))
          }
        />
      </Show>
      <Show when={tab() === "storage"}>
        <BlockList
          title="Volumes"
          items={ctr().volumes as any}
          fields={F.VOLUME_FIELDS}
          summary={(v: VolumeModel) =>
            `${v.host ?? v.name ?? "(source)"} → ${v.target || "(target)"}${v.read_only ? " · ro" : ""}`
          }
          addLabel="Add volume"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].volumes.push({
                span: null,
                host: null,
                name: null,
                target: "",
                read_only: false,
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].volumes.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const vol = d.containers[props.index].volumes[i] as any;
              vol[key] = v;
              // Exactly one of host / name: setting one clears the other.
              if (key === "host" && v) vol.name = null;
              if (key === "name" && v) vol.host = null;
            })
          }
        />
        <div class="inspector-note">
          A host path bind-mounts a lab-root-relative directory; a named volume is kept under
          the lab dir and shared by name — set exactly one of the two.
        </div>
      </Show>
      <Show when={tab() === "env"}>
        <BlockList
          title="Environment variables"
          items={ctr().env as any}
          fields={F.ENV_FIELDS}
          summary={(e) => (e.name ? `${e.name}=${e.value}` : "(env var)")}
          addLabel="Add variable"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].env.push({ span: null, name: "", value: "" }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].env.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => ((d.containers[props.index].env[i] as any)[key] = v ?? ""))
          }
        />
      </Show>
      <Show when={tab() === "health"}>
        <div class="field-row">
          <div
            class="field-row-label"
            title="Health probe gating readiness; without one the container is ready once its process starts"
          >
            Healthcheck
          </div>
          <div class="field-row-control">
            <Toggle
              checked={ctr().healthcheck !== null}
              onChange={(on) =>
                setField(
                  "healthcheck",
                  on ? { span: null, command: [], ...HEALTHCHECK_DEFAULTS } : null,
                )
              }
            />
          </div>
        </div>
        <Show when={ctr().healthcheck}>
          <BlockForm
            fields={F.HEALTHCHECK_FIELDS}
            value={ctr().healthcheck as any}
            onSet={setHealth}
          />
        </Show>
      </Show>
      <Show when={tab() === "web"}>
        <WebPagesTab
          pages={() => ctr().web}
          edit={(fn) => mutate((d) => fn(d.containers[props.index].web))}
        />
      </Show>
    </>
  );
}

// --- segment --------------------------------------------------------------------

const SINKHOLE_MODES = [
  { value: "nxdomain", label: "NXDOMAIN" },
  { value: "zero", label: "0.0.0.0" },
];

function SegmentDnsTable(props: {
  segment: SegmentModel;
  setSegment: (fn: (segment: SegmentModel) => void) => void;
}) {
  const empty = () => props.segment.records.length === 0 && props.segment.sinkholes.length === 0;
  return (
    <div class="dns-editor">
      <div class="dns-table">
        <Table aria-label="DNS entries">
          <thead>
            <tr>
              <th>Type</th>
              <th>Name / pattern</th>
              <th>Answer / behavior</th>
              <th aria-label="Actions" />
            </tr>
          </thead>
          <tbody>
            <For each={props.segment.records}>
              {(record, index) => (
                <tr>
                  <td class="dns-type">
                    <Badge>record</Badge>
                  </td>
                  <td>
                    <Input
                      aria-label="DNS record name"
                      value={record.name}
                      placeholder="host.example"
                      error={!record.name.trim()}
                      onInput={(e: InputEvent) =>
                        props.setSegment(
                          (s) =>
                            (s.records[index()].name = (
                              e.currentTarget as HTMLInputElement
                            ).value),
                        )
                      }
                    />
                  </td>
                  <td>
                    <Input
                      aria-label="DNS record IPv4 address"
                      value={record.ip}
                      placeholder="10.0.0.10"
                      error={!record.ip.trim()}
                      onInput={(e: InputEvent) =>
                        props.setSegment(
                          (s) =>
                            (s.records[index()].ip = (e.currentTarget as HTMLInputElement).value),
                        )
                      }
                    />
                  </td>
                  <td class="dns-actions">
                    <IconButton
                      icon={Trash2}
                      label="Delete DNS record"
                      onClick={() => props.setSegment((s) => void s.records.splice(index(), 1))}
                    />
                  </td>
                </tr>
              )}
            </For>
            <For each={props.segment.sinkholes}>
              {(sinkhole, index) => (
                <tr>
                  <td class="dns-type">
                    <Badge tone="warning">sinkhole</Badge>
                  </td>
                  <td>
                    <Input
                      aria-label="DNS sinkhole pattern"
                      value={sinkhole.pattern}
                      placeholder="*.telemetry.example"
                      error={!sinkhole.pattern.trim()}
                      onInput={(e: InputEvent) =>
                        props.setSegment(
                          (s) =>
                            (s.sinkholes[index()].pattern = (
                              e.currentTarget as HTMLInputElement
                            ).value),
                        )
                      }
                    />
                  </td>
                  <td>
                    <Select
                      aria-label="DNS sinkhole behavior"
                      options={SINKHOLE_MODES}
                      value={sinkhole.mode}
                      onChange={(mode) =>
                        props.setSegment((s) => (s.sinkholes[index()].mode = mode))
                      }
                    />
                  </td>
                  <td class="dns-actions">
                    <IconButton
                      icon={Trash2}
                      label="Delete DNS sinkhole"
                      onClick={() => props.setSegment((s) => void s.sinkholes.splice(index(), 1))}
                    />
                  </td>
                </tr>
              )}
            </For>
            <Show when={empty()}>
              <tr class="dns-empty">
                <td colSpan={4}>No static records or sinkholes.</td>
              </tr>
            </Show>
          </tbody>
        </Table>
      </div>
      <div class="dns-table-actions">
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() =>
            props.setSegment((s) => s.records.push({ span: null, name: "", ip: "" }))
          }
        >
          Add record
        </Button>
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() =>
            props.setSegment((s) =>
              s.sinkholes.push({ span: null, pattern: "", mode: "nxdomain" }),
            )
          }
        >
          Add sinkhole
        </Button>
      </div>
    </div>
  );
}

// The runtime half of the DNS tab: names the daemon auto-registers for
// VMs/containers (leases and static IPs) in this segment's zone —
// read-only, badged `dynamic`; the editable statics above stay the
// config's source of truth. With no live zone (lab powered off) it falls
// back to the registrations the draft config predicts, so the table is
// useful before the first `up` too.
function SegmentLiveDns(props: { segment: SegmentModel }) {
  // Gated on the console's status poll so opening the tab never spawns a
  // lab daemon. The source is the segment *name* (a stable string), so
  // status polling doesn't retrigger the fetch; refresh is manual.
  const [live, { refetch }] = createResource(
    () => (state.status && state.currentLab === editor.lab ? props.segment.name : null),
    async (segment): Promise<DnsLiveRecord[] | null> => {
      try {
        const table = await dnsTable(editor.lab!);
        const zone = table.segments.find((s) => s.segment === segment)?.zone;
        return zone?.records.filter((r) => r.kind === "dynamic") ?? null;
      } catch {
        return null;
      }
    },
  );
  // What the daemon WILL register, derived from the draft: both name forms
  // per machine with a NIC here; DHCP addresses are unknown until lease.
  const predicted = createMemo<DnsLiveRecord[]>(() => {
    const d = editor.draft;
    const dns = props.segment.dns;
    if (!d || !editor.lab || (dns.declared && !dns.enabled)) return [];
    const suffix = editor.catalog.host?.dns_suffix ?? "vmlab.internal";
    const rows: DnsLiveRecord[] = [];
    for (const m of [...d.vms, ...d.containers]) {
      const nic = m.nics.find((n) => n.segment === props.segment.name);
      if (!nic) continue;
      const ip = nic.ip ?? "auto (DHCP)";
      rows.push(
        { name: `${m.name}.${editor.lab}.${suffix}`, ip, kind: "dynamic" },
        { name: `${m.name}.${suffix}`, ip, kind: "dynamic" },
      );
    }
    rows.sort((a, b) => a.name.localeCompare(b.name));
    return rows;
  });
  // Live rows win per name; config-predicted rows fill the gaps (all of
  // them when the lab is powered off, the not-yet-leased machines when it
  // is up).
  const merged = createMemo(() => {
    if (live.loading) return { rows: [] as DnsLiveRecord[], anyLive: false, anyPredicted: false };
    const l = live() ?? [];
    const seen = new Set(l.map((r) => r.name));
    const add = predicted().filter((r) => !seen.has(r.name));
    const rows = [...l, ...add].sort((a, b) => a.name.localeCompare(b.name));
    return { rows, anyLive: l.length > 0, anyPredicted: add.length > 0 };
  });
  const rows = () => merged().rows;
  return (
    <Show when={rows().length > 0}>
      <div class="dns-live-section">
        <div class="dns-live-header">
          <div class="inspector-section-title">
            {merged().anyLive ? "Live registrations" : "Expected registrations"}
          </div>
          <Show when={merged().anyLive}>
            <IconButton
              icon={RefreshCw}
              label="Refresh live DNS registrations"
              onClick={() => refetch()}
            />
          </Show>
        </div>
        <div class="dns-table">
          <Table aria-label="DNS registrations">
            <thead>
              <tr>
                <th>Type</th>
                <th>Name</th>
                <th>Address</th>
              </tr>
            </thead>
            <tbody>
              <For each={rows()}>
                {(record) => (
                  <tr>
                    <td class="dns-type">
                      <Badge tone="info">dynamic</Badge>
                    </td>
                    <td class="dns-live-name">{record.name}</td>
                    <td>{record.ip}</td>
                  </tr>
                )}
              </For>
            </tbody>
          </Table>
        </div>
        <Show when={merged().anyPredicted}>
          <div class="dns-live-note">
            {merged().anyLive
              ? "Entries not in the running zone yet are expected registrations; auto (DHCP) addresses are assigned at first lease."
              : "Registered by the built-in DNS when the lab runs; auto (DHCP) addresses are assigned at first lease."}
          </div>
        </Show>
      </div>
    </Show>
  );
}

const BLOCK_PROTOCOLS = [
  { value: "", label: "any" },
  { value: "tcp", label: "TCP" },
  { value: "udp", label: "UDP" },
  { value: "icmp", label: "ICMP" },
];
const REDIRECT_PROTOCOLS = BLOCK_PROTOCOLS.filter((option) => option.value !== "icmp");

function SegmentRulesTables(props: {
  segment: SegmentModel;
  setSegment: (fn: (segment: SegmentModel) => void) => void;
}) {
  return (
    <div class="rules-tables">
      <div class="rules-table-section">
        <div class="inspector-section-title">Block rules</div>
        <div class="dns-table rules-table">
          <Table aria-label="Network block rules">
            <thead>
              <tr>
                <th>CIDR</th>
                <th>Protocol</th>
                <th>Port</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              <For each={props.segment.block_rules}>
                {(rule, index) => (
                  <tr>
                    <td>
                      <Input
                        aria-label="Blocked CIDR"
                        value={rule.cidr}
                        placeholder="0.0.0.0/0"
                        error={!rule.cidr.trim()}
                        onInput={(e: InputEvent) =>
                          props.setSegment(
                            (s) =>
                              (s.block_rules[index()].cidr = (
                                e.currentTarget as HTMLInputElement
                              ).value),
                          )
                        }
                      />
                    </td>
                    <td>
                      <Select
                        aria-label="Blocked protocol"
                        options={BLOCK_PROTOCOLS}
                        value={rule.proto ?? ""}
                        onChange={(proto) =>
                          props.setSegment(
                            (s) => (s.block_rules[index()].proto = proto || null),
                          )
                        }
                      />
                    </td>
                    <td>
                      <Input
                        aria-label="Blocked port"
                        type="number"
                        min="1"
                        max="65535"
                        value={rule.port == null ? "" : String(rule.port)}
                        placeholder="any"
                        onInput={(e: InputEvent) => {
                          const raw = (e.currentTarget as HTMLInputElement).value;
                          props.setSegment(
                            (s) => (s.block_rules[index()].port = raw === "" ? null : Number(raw)),
                          );
                        }}
                      />
                    </td>
                    <td class="dns-actions">
                      <IconButton
                        icon={Trash2}
                        label="Delete block rule"
                        onClick={() =>
                          props.setSegment((s) => void s.block_rules.splice(index(), 1))
                        }
                      />
                    </td>
                  </tr>
                )}
              </For>
              <Show when={props.segment.block_rules.length === 0}>
                <tr class="dns-empty">
                  <td colSpan={4}>No block rules.</td>
                </tr>
              </Show>
            </tbody>
          </Table>
        </div>
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() =>
            props.setSegment((s) =>
              s.block_rules.push({ span: null, cidr: "", proto: null, port: null }),
            )
          }
        >
          Add block rule
        </Button>
      </div>

      <div class="rules-table-section">
        <div class="inspector-section-title">Redirect rules</div>
        <div class="dns-table rules-table">
          <Table aria-label="Network redirect rules">
            <thead>
              <tr>
                <th>From</th>
                <th>To</th>
                <th>Protocol</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              <For each={props.segment.redirect_rules}>
                {(rule, index) => (
                  <tr>
                    <td>
                      <Input
                        aria-label="Redirect source"
                        value={rule.from}
                        placeholder="1.2.3.4:443"
                        error={!rule.from.trim()}
                        onInput={(e: InputEvent) =>
                          props.setSegment(
                            (s) =>
                              (s.redirect_rules[index()].from = (
                                e.currentTarget as HTMLInputElement
                              ).value),
                          )
                        }
                      />
                    </td>
                    <td>
                      <Input
                        aria-label="Redirect target"
                        value={rule.to}
                        placeholder="10.0.0.5:8443"
                        error={!rule.to.trim()}
                        onInput={(e: InputEvent) =>
                          props.setSegment(
                            (s) =>
                              (s.redirect_rules[index()].to = (
                                e.currentTarget as HTMLInputElement
                              ).value),
                          )
                        }
                      />
                    </td>
                    <td>
                      <Select
                        aria-label="Redirect protocol"
                        options={REDIRECT_PROTOCOLS}
                        value={rule.proto ?? ""}
                        onChange={(proto) =>
                          props.setSegment(
                            (s) => (s.redirect_rules[index()].proto = proto || null),
                          )
                        }
                      />
                    </td>
                    <td class="dns-actions">
                      <IconButton
                        icon={Trash2}
                        label="Delete redirect rule"
                        onClick={() =>
                          props.setSegment((s) => void s.redirect_rules.splice(index(), 1))
                        }
                      />
                    </td>
                  </tr>
                )}
              </For>
              <Show when={props.segment.redirect_rules.length === 0}>
                <tr class="dns-empty">
                  <td colSpan={4}>No redirect rules.</td>
                </tr>
              </Show>
            </tbody>
          </Table>
        </div>
        <Button
          size="sm"
          variant="ghost"
          icon={Plus}
          onClick={() =>
            props.setSegment((s) =>
              s.redirect_rules.push({ span: null, from: "", to: "", proto: null }),
            )
          }
        >
          Add redirect rule
        </Button>
      </div>
    </div>
  );
}

function SegmentInspector(props: { index: number }) {
  const seg = () => editor.draft!.segments[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.segments[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete segment "${seg().name}"?`,
        body: "NICs attached to it will need a new segment.",
        danger: true,
      })
    ) {
      removeSegment(props.index);
    }
  }

  const setSeg = (fn: (s: SegmentModel) => void) =>
    mutate((d) => fn(d.segments[props.index]));

  // This inspector renders outside the shared read-only fieldset so its
  // tabs stay navigable while machines run (the DNS tab shows runtime
  // state); each tab's config surface locks itself instead.
  const locked = () => anyVmRunning();

  return (
    <>
      <fieldset class="inspector-fields" disabled={locked()}>
        <div class="inspector-head">
          <span class="inspector-kind">segment</span>
          <Input
            value={seg().name}
            onInput={(e) => renameSegment(props.index, e.currentTarget.value)}
            title="Segment name (DNS label); references update automatically"
          />
          <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
            Delete
          </Button>
        </div>
      </fieldset>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "services", label: "DHCP" },
          { id: "dns", label: "DNS" },
          { id: "rules", label: "Rules" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <fieldset class="inspector-fields" disabled={locked()}>
          <BlockForm fields={F.SEGMENT_GENERAL} value={seg() as any} onSet={setField} />
          <SliderRow
            label="MTU"
            doc="Link MTU (576–65535); default jumbo (9000) on NAT segments, else 1500"
            value={seg().mtu}
            fallback={seg().nat ? 9000 : 1500}
            min={576}
            max={65535}
            step={1}
            fmt={(v) => String(v)}
            parse={(t) => {
              const n = parseInt(t, 10);
              return Number.isFinite(n) ? n : null;
            }}
            unsetLabel="default"
            onChange={(v) => setField("mtu", v)}
          />
        </fieldset>
      </Show>
      <Show when={tab() === "services"}>
        <fieldset class="inspector-fields" disabled={locked()}>
          <BlockForm fields={F.SEGMENT_SERVICES} value={seg() as any} onSet={setField} />
          <Input
            label="DNS server"
            help="Leave blank to use vmlab built-in DNS"
            placeholder="Use vmlab built-in DNS"
            value={seg().dns.server ?? ""}
            onInput={(e: InputEvent) => {
              const value = (e.currentTarget as HTMLInputElement).value;
              setSeg((s) => {
                s.dns.declared = value.trim() !== "";
                s.dns.server = value.trim() || null;
                s.dns.enabled = true;
              });
            }}
          />
        </fieldset>
      </Show>
      <Show when={tab() === "dns"}>
        <fieldset class="inspector-fields" disabled={locked()}>
          <SegmentDnsTable segment={seg()} setSegment={setSeg} />
        </fieldset>
        <SegmentLiveDns segment={seg()} />
      </Show>
      <Show when={tab() === "rules"}>
        <fieldset class="inspector-fields" disabled={locked()}>
          <SegmentRulesTables segment={seg()} setSegment={setSeg} />
        </fieldset>
      </Show>
    </>
  );
}
