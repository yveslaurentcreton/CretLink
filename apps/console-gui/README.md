# CretLink Console GUI shell

The technician-side console UI (**ST-M002-002**), built as a
[Tauri v2](https://v2.tauri.app) application (ADR-0005) with a plain-TypeScript
+ Vite frontend (ADR-0011).

This crate is a thin renderer. The authoritative session-UI state machine — the
technician host, discovered targets, single-selection, the pairing/consent
lifecycle, target identity, and the fail-closed emergency stop — lives in the
`cretlink-console` library (`apps/console/src/state.rs`) and is covered by
cross-platform automated tests there. The frontend holds no session authority;
it renders `ConsoleState` snapshots and forwards intents through a small,
schema-validated Tauri command set.

## Live by default

The shipped build starts live and idle: this technician host with no targets,
ready to open a real listening Noise XX session. Every pairing, consent, and
teardown then flows from the real transport through the shared session adapter
(`src-tauri/src/session_driver.rs`); no control is fabricated locally.

**Clearly-labelled simulated** seed targets and a fabricated-consent command
exist for local development only. They are compiled in solely with the
`dev-simulate` Cargo feature (for example
`npm run tauri dev -- --features dev-simulate`); a release build cannot reach a
simulated driver.

## Why it is a detached crate

`apps/console-gui/src-tauri` has its own `[workspace]` and `Cargo.lock` and is
**not** a member of the root Cargo workspace. Tauri's large dependency tree and
the Linux webview system libraries must not enter the portable core's
cross-platform `cargo build/test --workspace` matrix (ADR-0011). The workspace
therefore stays webview-free, and the state model is verified cross-platform via
`cretlink-console`.

## Layout

```text
apps/console-gui/
  package.json, vite.config.ts, tsconfig.json   frontend project
  index.html
  src/
    api.ts        typed IPC bridge (mirrors the Rust serde types)
    ui.ts         accessible DOM rendering (radiogroup, aria-live, stop)
    main.ts       bootstrap + intent dispatch
    styles.css
  src-tauri/
    Cargo.toml    detached crate; depends on cretlink-console by path
    tauri.conf.json
    capabilities/default.json
    src/lib.rs     managed state + Tauri commands
    src/session_driver.rs   live-transport driver (session events → state)
    src/main.rs
    icons/
```

## Build and run

Requires the Rust toolchain (see the repository `rust-toolchain.toml`) plus
Node.js 20+ and npm.

```sh
cd apps/console-gui
npm install            # first time only
npm run build          # tsc --noEmit + vite build → dist/
npm run tauri dev      # launch the console (dev server + native window)
npm run tauri build    # produce a platform bundle (.app/.dmg, .msi/.exe, …)
```

`npm run build` must run before a bare `cargo build` inside `src-tauri`, because
Tauri embeds the built frontend from `../dist`. `npm run tauri dev` / `tauri
build` run the frontend build automatically.

## Verification status

Verified on macOS Apple Silicon:

- The Tauri v2 shell compiles and links the WKWebView stack on
  `aarch64-apple-darwin`.
- The frontend type-checks and bundles; `npm audit` reports no advisories.
- The state model's invariants (single-selection, target-switching,
  stale-target invalidation, no-control-before-consent, idempotent fail-closed
  stop) are covered by automated tests in `cretlink-console`.

## Known gaps

- A verified Windows console GUI build and a manual **screen-reader** pass
  (Narrator/NVDA on Windows, VoiceOver on macOS) of the identity, approval-code,
  consent, and stop surfaces (REQ-N-013, REQ-N-014).
- `npm audit` and the GUI crate builds are not part of CI.

## Accessibility notes

- Targets are a `radiogroup` with roving `tabindex`; arrow keys move, Enter/Space
  select, enforcing single selection.
- Session state is announced through an `aria-live="polite"` status region;
  errors through an `aria-live="assertive"` alert.
- The **Stop** control is a native `<button>`, always reachable, with `Escape` as
  an always-available shortcut while a session is live.
- No state relies on colour alone — every badge and level carries text.
