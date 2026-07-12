import { For, Show, createEffect, createMemo, createResource, createSignal, onMount } from "solid-js";
import {
  Alert,
  Badge,
  Button,
  Card,
  Empty,
  IconButton,
  Input,
  ListBox,
  Modal,
  PageHead,
  Select,
  Spinner,
  StatusDot,
  Table,
} from "@forge/ui";
import { KeyRound, Monitor, Play, Plus, RefreshCw, Square, Trash2, Upload } from "lucide-solid";
import {
  state,
  buildTemplate,
  stopTemplateBuild,
  publishTemplate,
  dismissTemplateOp,
  loadTemplates,
  showToast,
  type TemplateOp,
} from "../store";
import { listStoreTemplates, removeStoreTemplate, templateRemote } from "../api";
import type { TemplateInfo, RemoteStatus, StoreTemplate } from "../api";
import { confirmDialog } from "./dialogs";
import ConsoleScreen from "./ConsoleScreen";
import {
  addRegistry,
  containerRegistry,
  registryEntries,
  refreshRegistries,
  loginRegistry,
  removeRegistry,
  vmRegistry,
  type RegistryEntry,
} from "../registries";

export default function TemplatesView() {
  onMount(() => void refreshRegistries().catch((error) => showToast(String(error), "danger")));
  const registries = createMemo(() =>
    registryEntries([
      ...state.templates.flatMap((t) => (t.registry ? [vmRegistry(t.registry)!] : [])).filter(Boolean),
      ...(state.status?.vms ?? []).map((vm) => vmRegistry(vm.template)).filter(Boolean),
      ...(state.status?.containers ?? []).map((c) => containerRegistry(c.image)).filter(Boolean),
    ] as RegistryEntry[]),
  );

  return (
    <Show
      when={state.currentLab}
      fallback={<Empty title="No lab selected">Select a lab to view its templates.</Empty>}
    >
      <PageHead
        title="templates"
        sub={`${state.templates.length} defined · ${registries().length} OCI ${registries().length === 1 ? "registry" : "registries"}`}
      />
      <div class="stack">
        <CachedTemplates />
        <RegistryInventory uses={registries()} />
        <Show
          when={state.templates.length}
          fallback={<Empty title="No templates">No template blocks in this lab's vmlab.wcl.</Empty>}
        >
          <For each={state.templates}>{(t) => <TemplateCard t={t} />}</For>
        </Show>
      </div>
    </Show>
  );
}

function CachedTemplates() {
  const [templates, { mutate, refetch }] = createResource(listStoreTemplates);
  const [removing, setRemoving] = createSignal<string | null>(null);
  const key = (template: StoreTemplate) =>
    `${template.arch}/${template.name}@${template.version}`;
  const created = (value: string) =>
    new Intl.DateTimeFormat(undefined, { dateStyle: "medium" }).format(new Date(value));

  const remove = async (template: StoreTemplate) => {
    const ref = key(template);
    const confirmed = await confirmDialog({
      title: `Remove ${template.name}@${template.version}?`,
      body: (
        <div class="cached-template-warning">
          This permanently removes <code>{ref}</code> from this machine. Any linked clones that
          still use this template may stop working.
        </div>
      ),
      confirmLabel: "Remove template",
      danger: true,
    });
    if (!confirmed) return;

    setRemoving(ref);
    try {
      await removeStoreTemplate(template);
      mutate((current) => current?.filter((item) => key(item) !== ref));
      await loadTemplates();
      showToast(`Removed ${ref}`);
    } catch (error) {
      showToast(String(error), "danger");
    } finally {
      setRemoving(null);
    }
  };

  return (
    <Card
      title={
        <span class="cached-template-heading">
          Cached on this machine
          <Show when={templates()}>
            {(items) => <Badge tone="neutral">{items().length}</Badge>}
          </Show>
        </span>
      }
      padded={false}
      action={
        <IconButton
          icon={RefreshCw}
          label="Refresh cached templates"
          disabled={templates.loading}
          onClick={() => void refetch()}
        />
      }
    >
      <Show when={!templates.loading} fallback={<div class="cached-template-state"><Spinner /> Reading local template store…</div>}>
        <Show
          when={!templates.error}
          fallback={<Alert tone="danger">Could not read the local template store: {String(templates.error)}</Alert>}
        >
          <Show
            when={templates()?.length}
            fallback={<div class="cached-template-state">No templates are cached on this machine.</div>}
          >
            <div class="cached-template-table">
              <Table aria-label="Templates cached on this machine">
                <thead>
                  <tr>
                    <th>Template</th>
                    <th>Version</th>
                    <th>Profile</th>
                    <th>Cached</th>
                    <th aria-label="Actions" />
                  </tr>
                </thead>
                <tbody>
                  <For each={templates()}>
                    {(template) => (
                      <tr>
                        <td>
                          <span class="cached-template-name">{template.name}</span>
                          <Badge>{template.arch}</Badge>
                        </td>
                        <td class="cached-template-version">{template.version}</td>
                        <td>{template.profile ?? "—"}</td>
                        <td>{created(template.created)}</td>
                        <td class="cached-template-actions">
                          <IconButton
                            icon={Trash2}
                            label={`Remove ${key(template)}`}
                            disabled={removing() !== null}
                            onClick={() => void remove(template)}
                          />
                        </td>
                      </tr>
                    )}
                  </For>
                </tbody>
              </Table>
            </div>
          </Show>
        </Show>
      </Show>
    </Card>
  );
}

function RegistryInventory(p: { uses: RegistryEntry[] }) {
  const [adding, setAdding] = createSignal(false);
  const [namespace, setNamespace] = createSignal("");
  const [kind, setKind] = createSignal("both");
  const [loginTarget, setLoginTarget] = createSignal<string | null>(null);
  const [username, setUsername] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const usage = (use: RegistryEntry) => {
    if (use.vms && use.containers) return { label: "both", tone: "success" as const };
    if (use.vms) return { label: "VMs", tone: "accent" as const };
    return { label: "containers", tone: "info" as const };
  };

  return (
    <Card
      title="OCI registries"
      padded={false}
      action={
        <Button size="sm" icon={Plus} onClick={() => setAdding(true)}>
          Add registry
        </Button>
      }
    >
      <Show
        when={p.uses.length}
        fallback={<div class="registry-empty">No OCI registries are referenced by this lab.</div>}
      >
        <div class="registry-table">
          <Table aria-label="OCI registries used by this lab">
            <thead>
              <tr>
                <th>Registry</th>
                <th>Used by</th>
                <th>Authentication</th>
                <th aria-label="Actions" />
              </tr>
            </thead>
            <tbody>
              <For each={p.uses}>
                {(use) => (
                  <tr>
                    <td class="registry-host">{use.namespace}</td>
                    <td>
                      <Badge tone={usage(use).tone}>{usage(use).label}</Badge>
                    </td>
                    <td>
                      <Badge tone={use.authenticated ? "success" : "neutral"}>
                        {use.authenticated ? "credentials configured" : "public / anonymous"}
                      </Badge>
                    </td>
                    <td class="registry-actions">
                      <IconButton
                        icon={KeyRound}
                        label={`Configure credentials for ${use.namespace}`}
                        onClick={() => {
                          setUsername("");
                          setPassword("");
                          setLoginTarget(use.namespace);
                        }}
                      />
                      <IconButton
                        icon={Trash2}
                        label={`Remove ${use.namespace}`}
                        onClick={() =>
                          void removeRegistry(use.namespace).catch((error) =>
                            showToast(String(error), "danger"),
                          )
                        }
                      />
                    </td>
                  </tr>
                )}
              </For>
            </tbody>
          </Table>
        </div>
      </Show>
      <Modal
        open={adding()}
        title="Add OCI registry"
        onClose={() => setAdding(false)}
        footer={
          <>
            <Button variant="ghost" onClick={() => setAdding(false)}>Cancel</Button>
            <Button
              variant="primary"
              disabled={busy() || !namespace().includes("/")}
              onClick={() => {
                setBusy(true);
                void addRegistry({
                  namespace: namespace(),
                  vms: kind() === "vms" || kind() === "both",
                  containers: kind() === "containers" || kind() === "both",
                })
                  .then(() => {
                    setNamespace("");
                    setAdding(false);
                    showToast("Registry added to CLI and web settings");
                  })
                  .catch((error) => showToast(String(error), "danger"))
                  .finally(() => setBusy(false));
              }}
            >
              Add registry
            </Button>
          </>
        }
      >
        <div class="registry-add-form">
          <Input
            label="Registry namespace"
            help="Include the owner or group whose repositories should be searched."
            placeholder="ghcr.io/owner/templates"
            value={namespace()}
            onInput={(e) => setNamespace(e.currentTarget.value)}
          />
          <Select
            label="Use for"
            value={kind()}
            options={[
              { value: "both", label: "VMs and containers" },
              { value: "vms", label: "VMs" },
              { value: "containers", label: "Containers" },
            ]}
            onChange={setKind}
          />
        </div>
      </Modal>
      <Modal
        open={loginTarget() !== null}
        title="Registry credentials"
        onClose={() => setLoginTarget(null)}
        footer={
          <>
            <Button variant="ghost" onClick={() => setLoginTarget(null)}>
              Cancel
            </Button>
            <Button
              variant="primary"
              disabled={busy() || !username().trim() || !password()}
              onClick={() => {
                const target = loginTarget();
                if (!target) return;
                setBusy(true);
                void loginRegistry(target, username().trim(), password())
                  .then(() => {
                    setPassword("");
                    setLoginTarget(null);
                    showToast("Registry credentials saved for CLI and web");
                  })
                  .catch((error) => showToast(String(error), "danger"))
                  .finally(() => setBusy(false));
              }}
            >
              Verify and save
            </Button>
          </>
        }
      >
        <div class="registry-add-form">
          <div class="registry-credential-target">{loginTarget()}</div>
          <Input
            label="Username"
            autocomplete="username"
            value={username()}
            onInput={(event) => setUsername(event.currentTarget.value)}
          />
          <Input
            label="Password or access token"
            type="password"
            autocomplete="current-password"
            value={password()}
            onInput={(event) => setPassword(event.currentTarget.value)}
          />
          <div class="registry-credential-help">
            Credentials are verified, then stored in Docker-compatible host credentials. The web
            console never reads the saved password back.
          </div>
        </div>
      </Modal>
    </Card>
  );
}

function TemplateCard(p: { t: TemplateInfo }) {
  const key = () => `${state.currentLab}/${p.t.arch}/${p.t.name}`;
  const op = (): TemplateOp | undefined => state.templateOps[key()];
  const running = () => op()?.status === "running" || p.t.op !== null;
  const [version, setVersion] = createSignal<string | null>(null);
  const [consoleOpen, setConsoleOpen] = createSignal(false);
  // Selected publish version, defaulting to the newest local build.
  const selected = () => version() ?? p.t.local_versions[0] ?? null;

  const publish = () => {
    const v = selected();
    if (v) publishTemplate(p.t.name, p.t.arch, v);
  };
  const consoleEndpoint = () =>
    `/api/labs/${encodeURIComponent(state.currentLab!)}/templates/${encodeURIComponent(p.t.arch)}/${encodeURIComponent(p.t.name)}/vnc`;

  return (
    <Card
      title={
        <span class="tpl-title">
          {p.t.name}
          <Badge>{p.t.arch}</Badge>
          <span class="tpl-meta">prefix {p.t.version_prefix}</span>
          <Show when={p.t.registry}>
            <span class="tpl-meta">{p.t.registry}</span>
          </Show>
        </span>
      }
      action={
        <span style={{ display: "inline-flex", gap: "8px" }}>
          <Show when={op()?.kind === "build" && op()?.status === "running"}>
            <Button
              size="sm"
              icon={Monitor}
              disabled={!op()?.consoleReady}
              onClick={() => setConsoleOpen(true)}
              title={op()?.consoleReady ? "Attach to the build VM console" : "Console is starting"}
            >
              Console
            </Button>
            <Button
              size="sm"
              variant="danger"
              icon={Square}
              onClick={() => stopTemplateBuild(p.t.name, p.t.arch)}
              title={`Stop the ${p.t.arch} build`}
            >
              Stop
            </Button>
          </Show>
          <Button
            size="sm"
            variant="primary"
            icon={Play}
            disabled={running()}
            onClick={() => buildTemplate(p.t.name, p.t.arch)}
            title={`Build only the ${p.t.arch} variant`}
          >
            Build
          </Button>
          <Button
            size="sm"
            icon={Upload}
            disabled={running() || !selected()}
            onClick={publish}
            title={
              selected() ? `Push ${selected()} to the registry` : "No local build to publish"
            }
          >
            Publish
          </Button>
        </span>
      }
    >
      <div class="tpl-cols">
        <div>
          <div class="tpl-sec">Local builds</div>
          <Show
            when={p.t.local_versions.length}
            fallback={<div class="tpl-meta">Nothing built yet.</div>}
          >
            <ListBox
              options={p.t.local_versions.map((v, i) => ({
                value: v,
                label: (
                  <span class="tpl-title">
                    {v}
                    <Show when={i === 0}>
                      <Badge tone="accent">newest</Badge>
                    </Show>
                  </span>
                ),
              }))}
              value={selected() ?? undefined}
              onChange={(v) => setVersion(v)}
            />
          </Show>
        </div>
        <RemotePanel t={p.t} />
      </div>

      <Show when={op()}>
        <OpPanel op={op()!} opKey={key()} />
      </Show>
      <Modal
        open={consoleOpen()}
        title={`${p.t.arch}/${p.t.name} build console`}
        onClose={() => setConsoleOpen(false)}
      >
        <ConsoleScreen
          lab={state.currentLab!}
          vm={`${p.t.name} build`}
          powered={op()?.status === "running" && !!op()?.consoleReady}
          endpoint={consoleEndpoint()}
        />
      </Modal>
    </Card>
  );
}

function RemotePanel(p: { t: TemplateInfo }) {
  const [tick, setTick] = createSignal(0);
  const [remote] = createResource(
    () => (p.t.registry ? `${p.t.arch}/${p.t.name}:${tick()}` : null),
    (): Promise<RemoteStatus> => templateRemote(state.currentLab!, p.t.name, p.t.arch),
  );
  return (
    <div>
      <div class="tpl-sec">
        Registry
        <Show when={p.t.registry}>
          <Button
            size="sm"
            variant="ghost"
            icon={RefreshCw}
            onClick={() => setTick(tick() + 1)}
            title="Re-query the registry"
          >
            refresh
          </Button>
        </Show>
      </div>
      <Show when={p.t.registry} fallback={<div class="tpl-meta">No registry configured.</div>}>
        <Show
          when={!remote.loading}
          fallback={
            <div class="tpl-meta">
              <Spinner size={12} /> Checking registry…
            </div>
          }
        >
          <Show
            when={!remote.error}
            fallback={<Alert tone="danger">{String(remote.error)}</Alert>}
          >
            <Show
              when={remote()?.tags.length}
              fallback={<div class="tpl-meta">Nothing published yet.</div>}
            >
              <div>
                <For each={remote()!.tags}>
                  {(tag) => (
                    <div class="tag-row">
                      <span>{tag.tag}</span>
                      <span class="tag-row-arches">{tag.arches.join(", ") || "—"}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </Show>
      </Show>
    </div>
  );
}

function OpPanel(p: { op: TemplateOp; opKey: string }) {
  let pane: HTMLDivElement | undefined;
  // Follow the tail as log lines arrive.
  createEffect(() => {
    p.op.log.length;
    if (pane) queueMicrotask(() => (pane!.scrollTop = pane!.scrollHeight));
  });
  const label = () => {
    const verb = p.op.kind === "push" ? "Publish" : "Build";
    switch (p.op.status) {
      case "running":
        return `${verb} running…`;
      case "done":
        return `${verb} finished${p.op.version ? ` — ${p.op.version}` : ""}`;
      case "cancelled":
        return `${verb} stopped`;
      default:
        return `${verb} failed`;
    }
  };
  return (
    <div class="tpl-op">
      <div class="tpl-op-head">
        <StatusDot
          tone={
            p.op.status === "running"
              ? "warning"
              : p.op.status === "done"
                ? "success"
                : p.op.status === "cancelled"
                  ? "neutral"
                  : "danger"
          }
        />
        <span>{label()}</span>
        <Show when={p.op.status !== "running"}>
          <Button size="sm" variant="ghost" onClick={() => dismissTemplateOp(p.opKey)}>
            dismiss
          </Button>
        </Show>
      </div>
      <Show when={p.op.error}>
        <Alert tone="danger">{p.op.error}</Alert>
      </Show>
      <Show when={p.op.log.length}>
        <div class="tpl-log" ref={pane}>
          <For each={p.op.log}>{(line) => <div>{line}</div>}</For>
        </div>
      </Show>
    </div>
  );
}
