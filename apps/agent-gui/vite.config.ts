import { defineConfig } from "vite";

// The agent consent frontend is bundled and served locally by Tauri under a
// strict CSP (ADR-0005/ADR-0011): no remote content, fixed dev port for the
// Tauri dev server. Port 1421 keeps it distinct from the console GUI (1420) so
// both dev servers can run side by side.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1421,
    strictPort: true,
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
  },
});
