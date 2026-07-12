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
import { KeyRound, Play, Plus, RefreshCw, Trash2, Upload } from "lucide-solid";
import {
  state,
  buildTemplate,
  publishTemplate,
  dismissTemplateOp,
  showToast,
  type TemplateOp,
} from "../store";
import { templateRemote } from "../api";
import type { TemplateInfo, RemoteStatus } from "../api";
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
  const key = () => `${state.currentLab}/${p.t.name}`;
  const op = (): TemplateOp | undefined => state.templateOps[key()];
  const running = () => op()?.status === "running" || p.t.op !== null;
  const [version, setVersion] = createSignal<string | null>(null);
  // Selected publish version, defaulting to the newest local build.
  const selected = () => version() ?? p.t.local_versions[0] ?? null;

  const publish = () => {
    const v = selected();
    if (v) publishTemplate(p.t.name, v);
  };

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
          <Button
            size="sm"
            variant="primary"
            icon={Play}
            disabled={running()}
            onClick={() => buildTemplate(p.t.name)}
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
    </Card>
  );
}

function RemotePanel(p: { t: TemplateInfo }) {
  const [tick, setTick] = createSignal(0);
  const [remote] = createResource(
    () => (p.t.registry ? `${p.t.name}:${tick()}` : null),
    (): Promise<RemoteStatus> => templateRemote(state.currentLab!, p.t.name),
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
      default:
        return `${verb} failed`;
    }
  };
  return (
    <div class="tpl-op">
      <div class="tpl-op-head">
        <StatusDot
          tone={
            p.op.status === "running" ? "warning" : p.op.status === "done" ? "success" : "danger"
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
