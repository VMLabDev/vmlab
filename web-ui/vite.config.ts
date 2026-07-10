import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

// Dev server proxies the API + WebSockets to a locally-running vmlab-web so
// `pnpm dev` gives hot-reload against the real backend.
export default defineConfig({
  plugins: [solid()],
  resolve: {
    // One copy each of solid (reactivity breaks silently otherwise) and the
    // CodeMirror core (duplicate @codemirror/state throws at editor mount).
    dedupe: ["solid-js", "@codemirror/state", "@codemirror/view", "@codemirror/language"],
  },
  optimizeDeps: {
    // Forge packages ship preserved-JSX source under the `solid` export
    // condition; keep them out of esbuild pre-bundling so vite-plugin-solid
    // compiles them.
    exclude: ["@forge/ui", "@forge/tokens", "@forge/code", "@forge/desktop"],
  },
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:7878", ws: true },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
});
