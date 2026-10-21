import { defineConfig } from "vite";

// The console frontend is bundled and served locally by Tauri under a strict
// CSP (ADR-0005/ADR-0011): no remote content, fixed dev port for the Tauri dev
// server.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
  },
});
