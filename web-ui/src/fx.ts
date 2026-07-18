// Topology-node particle effects (forge fx). The canvas registers its VM /
// container <g> nodes here so the store's destroy session can shatter them
// without any store→component coupling. Plain module state, not a signal:
// nothing renders from this registry — it is imperative DOM work only.

import { fx } from "@forge/ui";

const nodes = new Map<string, Element>(); // "vm:<name>" | "container:<name>"

/** Register a topology node; returns the unregister fn (for onCleanup). */
export function registerFxNode(key: string, el: Element): () => void {
  nodes.set(key, el);
  return () => {
    if (nodes.get(key) === el) nodes.delete(key);
  };
}

/** Destroy feedback: shatter the node, then fade it back in.
 *  Fire-and-forget; skips silently when the canvas isn't showing the node. */
export function playDestroyRecreate(key: string): void {
  const el = nodes.get(key);
  if (!el || !el.isConnected) return;
  // SVG <g> has no backgroundColor for the fallback raster to sample, so
  // hand it theme colors — read at call time to honor theme switches.
  const root = getComputedStyle(document.documentElement);
  const colors = ["--danger", "--accent", "--fg-2"]
    .map((v) => root.getPropertyValue(v).trim())
    .filter(Boolean);
  void fx.recreate(el as unknown as HTMLElement, {
    colors,
    duration: 550,
    holdMs: 150,
    particleSize: 3,
    reappear: "fade",
  });
}
