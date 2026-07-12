import { createSignal } from "solid-js";
import * as api from "./api";

export interface RegistryEntry {
  namespace: string;
  vms: boolean;
  containers: boolean;
  authenticated?: boolean;
}

const DOCKER_HUB = "registry-1.docker.io";
const [configured, setConfigured] = createSignal<RegistryEntry[]>([]);
const [removed, setRemoved] = createSignal<string[]>([]);
let loading: Promise<void> | null = null;

export async function refreshRegistries(): Promise<void> {
  if (loading) return loading;
  loading = api
    .listRegistries()
    .then((settings) => {
      setConfigured(settings.entries);
      setRemoved(settings.removed.map(normaliseNamespace));
    })
    .finally(() => (loading = null));
  return loading;
}

export function normaliseNamespace(value: string): string {
  const trimmed = value.trim().replace(/^https?:\/\//, "").replace(/\/+$/, "");
  const slash = trimmed.indexOf("/");
  const host = (slash < 0 ? trimmed : trimmed.slice(0, slash)).toLowerCase();
  const path = slash < 0 ? "" : trimmed.slice(slash + 1);
  const canonicalHost =
    host === "docker.io" || host === "index.docker.io" ? DOCKER_HUB : host;
  return path ? `${canonicalHost}/${path}` : canonicalHost;
}

function merge(into: Map<string, RegistryEntry>, entry: RegistryEntry) {
  const namespace = normaliseNamespace(entry.namespace);
  if (!namespace) return;
  const old = into.get(namespace);
  into.set(namespace, {
    namespace,
    vms: !!entry.vms || !!old?.vms,
    containers: !!entry.containers || !!old?.containers,
    authenticated: !!entry.authenticated || !!old?.authenticated,
  });
}

export function registryEntries(discovered: RegistryEntry[] = []): RegistryEntry[] {
  const hidden = new Set(removed());
  const entries = new Map<string, RegistryEntry>();
  for (const entry of [...configured(), ...discovered]) {
    if (!hidden.has(normaliseNamespace(entry.namespace))) merge(entries, entry);
  }
  return [...entries.values()].sort((a, b) => a.namespace.localeCompare(b.namespace));
}

export async function addRegistry(entry: RegistryEntry) {
  const useFor = entry.vms && entry.containers ? "both" : entry.vms ? "vms" : "containers";
  await api.addRegistrySetting(normaliseNamespace(entry.namespace), useFor);
  await refreshRegistries();
}

export async function removeRegistry(namespace: string) {
  await api.removeRegistrySetting(normaliseNamespace(namespace));
  await refreshRegistries();
}

export async function loginRegistry(namespace: string, username: string, password: string) {
  await api.loginRegistry(normaliseNamespace(namespace), username, password);
  await refreshRegistries();
}

function withoutSelector(reference: string): string {
  const digestless = reference.trim().split("@", 1)[0];
  const slash = digestless.lastIndexOf("/");
  const colon = digestless.lastIndexOf(":");
  return colon > slash ? digestless.slice(0, colon) : digestless;
}

function explicitHost(first: string): boolean {
  return first === "localhost" || first.includes(".") || first.includes(":");
}

export function vmRegistry(reference: string): RegistryEntry | null {
  const parts = withoutSelector(reference).split("/");
  if (parts.length < 3 || !explicitHost(parts[0])) return null;
  return { namespace: parts.slice(0, -1).join("/"), vms: true, containers: false };
}

export function containerRegistry(reference: string): RegistryEntry | null {
  const raw = withoutSelector(reference).split("/").filter(Boolean);
  if (!raw.length) return null;
  const parts = explicitHost(raw[0])
    ? raw
    : [DOCKER_HUB, ...(raw.length === 1 ? ["library"] : []), ...raw];
  if (parts.length < 2) return null;
  return {
    namespace: normaliseNamespace(parts.slice(0, -1).join("/")),
    vms: false,
    containers: true,
  };
}
