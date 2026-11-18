import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { viteSingleFile } from "vite-plugin-singlefile";

// One self-contained dist/index.html: scripts, styles and fonts inlined, so it loads from file:// with no server.
export default defineConfig({
  plugins: [svelte(), viteSingleFile()],
  base: "./",
  build: {
    target: "es2022",
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    cssCodeSplit: false,
    reportCompressedSize: false,
  },
});
