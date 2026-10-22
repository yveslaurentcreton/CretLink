# The CretLink application

One program, `cretlink` (`cretlink.exe` on Windows), for both sides of a
support session (**ST-M009-005**, ADR-0018). It is built as a
[Tauri v2](https://v2.tauri.app) application (ADR-0005) with a plain-TypeScript
+ Vite frontend (ADR-0011).

- **Give help** — the technician console: it listens for the customer's
  computer, compares the session code, hosts the MCP bridge's local IPC
  endpoint, and shows the live activity and the terminal.
- **Get help** — the customer's window, Windows only: it finds or dials the
  technician, shows this computer's honest identity, asks for the one informed
  full-control consent, mirrors the activity, and can stop at any time.
- **Subcommands** — `cretlink mcp`, `cretlink mcp install | uninstall | status`
  and `cretlink version` run without a window, exactly like the console build
  `cretlink-cli` (`apps/cli`). On Windows this program is a GUI-subsystem
  program, so the interactive `shell` is left to `cretlink-cli`.

## One role per process

`src-tauri/src/launch.rs` decides, before any window opens, what a process
does, and the role then stays fixed for the life of the process:

| Started as | Does |
| --- | --- |
| `cretlink` with a subcommand | runs the subcommand, no window |
| `cretlink` on Windows | start screen: Get help or Give help |
| `cretlink` on macOS or Linux | Give help |
| `CretLink-Support*.exe` | Get help; refuses subcommands and Give help |
| `cretlink --role technician` / `--role customer` | that role (the start screen starts a role this way) |

Each role registers only its own Tauri commands (`technician::attach`,
`customer::attach`), so a customer process has no technician command, no IPC
endpoint and no listener, and the reverse. The start screen does not switch
roles in place: it starts the chosen role as a new process and closes.

Get help needs administrator rights for the execution broker. A process that is
not elevated starts itself again through the UAC prompt (`ShellExecuteExW` with
`runas`, in `cretlink_cli::process`) and exits; when the customer declines, the
start screen says so and nothing starts. A process started with `--role` never
relaunches again; it opens and shows its real, possibly unelevated, identity
(REQ-S-011).

Without the WebView2 runtime the program cannot draw its window; it says so in
a native message box before anything starts.

## Live by default

The shipped build opens real transport only. **Clearly-labelled simulated**
targets, the consent demo and the fabricated-consent commands exist for local
development and are compiled in solely with the `dev-simulate` Cargo feature,
which also makes Get help available off Windows for local testing. A release
build cannot reach a simulated driver.

The authoritative state machines live in `cretlink-console`
(`apps/console/src/state.rs`) and `cretlink-support-agent`
(`apps/support-agent/src/state.rs`) and are covered by cross-platform tests
there; this crate is a thin renderer and holds no session authority.

## Why it is a detached crate

`apps/app/src-tauri` has its own `[workspace]` and `Cargo.lock` and is not a
member of the root Cargo workspace. Tauri's large dependency tree and the Linux
webview system libraries must not enter the portable core's cross-platform
`cargo build/test --workspace` matrix (ADR-0011). CI builds, lints and tests it
in its own job on Windows, macOS and Linux.

## Layout

```text
apps/app/
  package.json, vite.config.ts, tsconfig.json   frontend project
  index.html
  src/
    main.ts        asks the backend for the role and loads only that interface
    base.css       shared by every screen
    start/         the start screen
    technician/    Give help: api.ts, ui.ts, main.ts, styles.css
    customer/      Get help: api.ts, ui.ts, main.ts, styles.css
  src-tauri/
    Cargo.toml     detached crate; binary `cretlink`
    tauri.conf.json, capabilities/default.json, icons/
    src/main.rs    subcommand, start screen or role
    src/launch.rs  the launch decision (unit-tested)
    src/lib.rs     window per screen, customer elevation
    src/start.rs   get_role, choose_role
    src/technician/  console state, commands and live-session driver
    src/customer/    agent state, commands and live-session driver
```

## Build and run

Requires the Rust toolchain (see the repository `rust-toolchain.toml`) plus
Node.js 20+ and npm; on Linux also the webview libraries
(`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `librsvg2-dev`).

```sh
cd apps/app
npm install                 # first time only
npm run build               # tsc --noEmit + vite build → dist/
npm run tauri dev           # run with the dev server (debug build)
cd src-tauri
cargo build --release --features tauri/custom-protocol   # the shipped program
```

`npm run build` must run before a bare `cargo build` inside `src-tauri`, because
Tauri embeds the built frontend from `../dist`. Without `tauri/custom-protocol`
a build loads the frontend from the dev server instead.

## Accessibility notes

- Every screen has a skip link to its main controls.
- State is announced through `aria-live` regions; errors through an assertive
  alert (REQ-N-015).
- **Stop** is a native `<button>`, always reachable, with `Escape` as a shortcut
  while a session is live (REQ-N-014).
- No state relies on colour alone: every badge and level carries text
  (REQ-N-013).

## Known gaps

- A manual **screen-reader** pass (Narrator/NVDA on Windows, VoiceOver on macOS)
  of the start screen, identity, approval-code, disclosure, consent and stop
  surfaces (REQ-N-013, REQ-N-014).
- The UAC prompt of Get help and a Windows run of both roles against each other
  are manual checks.
- The new interfaces of ST-M009-008 and ST-M009-009 replace the current role
  screens.
