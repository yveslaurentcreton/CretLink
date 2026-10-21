# CretLink Agent consent window

The customer-side attended-consent window (**ST-M002-005**), built as a
[Tauri v2](https://v2.tauri.app) application (ADR-0005) with a plain-TypeScript
+ Vite frontend (ADR-0011). It is the agent-side counterpart of
`apps/console-gui`.

This crate is a thin renderer. The authoritative agent-side state machine — the
honest effective identity, the announce → pair → **await consent** → full
control lifecycle, the single informed consent that is the only path to control,
and the bilateral fail-closed emergency stop — lives in the
`cretlink-support-agent` library (`apps/support-agent/src/state.rs`) and is
covered by cross-platform automated tests there. The full-control disclosure is
the canonical `DisclosureSummary::DEFAULT_TEXT` from `cretlink-session`, shown
verbatim so the customer reads exactly the words sent on the wire and mirrored
on the technician's console (REQ-S-016). The frontend holds no session
authority; it renders `ConsentView` snapshots and forwards the customer's
decision through a small Tauri command set.

## Live by default

The shipped build starts resting and live: it shows this machine's honest
identity and waits for the customer to enter the technician's connection code.
It then dials the listening technician console as the Noise initiator
(ADR-0003) and folds the real session event stream through the shared agent
adapter (`src-tauri/src/session_driver.rs`). Full control is shown only when
the customer's real consent echoes back from the wire; a dropped transport ends
in an honest terminal state, never a stale "under control" surface.

A **clearly-labelled simulated** consent demo exists for local development
only. It is compiled in solely with the `dev-simulate` Cargo feature (for
example `npm run tauri dev -- --features dev-simulate`); granting there binds a
simulated `SessionId` and confers **no real control** over any computer. A
release build cannot reach the simulated path.

## Why it is a detached crate

`apps/agent-gui/src-tauri` has its own `[workspace]` and `Cargo.lock` and is
**not** a member of the root Cargo workspace. Tauri's large dependency tree and
the platform webview system libraries must not enter the portable core's
cross-platform `cargo build/test --workspace` matrix (ADR-0011). The workspace
therefore stays webview-free, and the state model is verified cross-platform via
`cretlink-support-agent`.

## Layout

```text
apps/agent-gui/
  package.json, vite.config.ts, tsconfig.json   frontend project
  index.html
  src/
    api.ts        typed IPC bridge (mirrors the Rust serde types)
    ui.ts         accessible DOM rendering (control indicator, aria-live, stop)
    main.ts       bootstrap + intent dispatch
    styles.css
  src-tauri/
    Cargo.toml    detached crate; depends on cretlink-support-agent by path
    tauri.conf.json
    capabilities/default.json
    src/lib.rs     managed state + Tauri commands
    src/session_driver.rs   live-transport driver (session events → view)
    src/main.rs
    icons/
```

## Build and run

Requires the Rust toolchain (see the repository `rust-toolchain.toml`) plus
Node.js 20+ and npm.

```sh
cd apps/agent-gui
npm install            # first time only
npm run build          # tsc --noEmit + vite build → dist/
npm run tauri dev      # launch the consent window (dev server + native window)
npx tauri build --no-bundle   # produce the raw release executable (no installer)
```

`npm run build` must run before a bare `cargo build` inside `src-tauri`, because
Tauri embeds the built frontend from `../dist`. `npm run tauri dev` /
`tauri build` run the frontend build automatically. The dev server uses port
1421 so it can run alongside the console GUI (port 1420).

## Accessibility notes

- The control-state indicator is always visible and never relies on colour
  alone — it pairs a glyph (`●`/`◐`/`○`) with a full text label (REQ-N-013), and
  lives in an `aria-live="polite"` region so a change of control is announced.
- The full-control disclosure is presented verbatim before the decision.
- **Grant full control** and **Decline** are native `<button>`s; the **Stop /
  revoke** control is always reachable, with `Escape` as an always-available
  shortcut while a session is live (REQ-N-014).
- Errors are announced through an `aria-live="assertive"` alert (REQ-N-015).

## Verification status

- The state model's invariants (no control before consent, consent as the only
  path to control, bilateral idempotent fail-closed stop, reconnect cannot
  restore control, secret-free terminal failure) are covered by automated tests
  in `cretlink-support-agent`, plus this crate's own unit tests over the
  live-transport driver and, under `dev-simulate`, the simulated consent demo.

## Known gaps

These need manual verification on real hardware (ADR-0005, ADR-0011):

- A manual **screen-reader** pass (Narrator/NVDA on Windows, VoiceOver on macOS)
  of the identity, approval-code, disclosure, consent, and stop surfaces
  (REQ-N-013, REQ-N-014).
- Visual/UX and contrast polish of the consent, disclosure, and stop surfaces.
