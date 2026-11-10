import { defineConfig } from "vitest/config";

// Frontend rendering tests (ST-M009-008): every screen is a pure function of a
// state snapshot, rendered in jsdom without Tauri. Times are formatted in
// local time, so the tests pin the zone.
process.env.TZ = "UTC";

export default defineConfig({
  // The disclosure test reads the Rust source it must match.
  server: { fs: { allow: [".", "../../crates/session/src"] } },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
