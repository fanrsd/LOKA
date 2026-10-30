import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const host = process.env.TAURI_DEV_HOST;

/** WebKit before Safari 15.4 (a Mac whose Safari was never updated, e.g. Big Sur's own 14) drops every `@layer`
 * block and every rule naming `::backdrop`: all of Tailwind. Unwrap the layers, in order, ahead of the unlayered CSS
 * so it still wins over them; no `<dialog>`/fullscreen/popover here, so `::backdrop` styling is never used. */
function flattenLayers(css: string): string {
  let layered = "", rest = "", i = 0;
  for (let at; (at = css.indexOf("@layer", i)) >= 0; ) {
    rest += css.slice(i, at);
    const open = at + css.slice(at).search(/[{;]/);
    i = open + 1;
    if (css[open] === ";") continue; // `@layer a, b;` only orders the layers
    for (let depth = 1; depth; i++) depth += css[i] === "{" ? 1 : css[i] === "}" ? -1 : 0;
    layered += css.slice(open + 1, i - 1);
  }
  return (layered + rest + css.slice(i)).replaceAll(",::backdrop", "");
}

const oldWebKit: Plugin = {
  name: "loka-old-webkit",
  apply: "build",
  generateBundle(_, bundle) {
    for (const a of Object.values(bundle)) if (a.type === "asset" && a.fileName.endsWith(".css")) a.source = flattenLayers(String(a.source));
  },
};

export default defineConfig({
  plugins: [react(), tailwindcss(), oldWebKit],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
