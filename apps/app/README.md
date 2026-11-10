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
- **Subcommands** — `cretlink mcp`, `cretlink mcp install | uninstall | status`,
  `cretlink uninstall` and `cretlink version` run without a window, exactly like
  the console build `cretlink-cli` (`apps/cli`). On Windows this program is a GUI-subsystem
  program, so the interactive `shell` is left to `cretlink-cli`.

## One role per process

`src-tauri/src/launch.rs` decides, before any window opens, what a process
does, and the role then stays fixed for the life of the process:

| Started as | Does |
| --- | --- |
| `cretlink` with a subcommand | runs the subcommand, no window |
| `cretlink` on Windows | start screen: Get help or Give help |
| the installed `cretlink` on macOS or Linux | Give help |
| any other `cretlink` on macOS or Linux | start screen with Give help only: install or run once |
| `CretLink-Support*.exe` | Get help; refuses subcommands (`install`, `uninstall` included) and Give help, except `recover [--remove]` |
| `cretlink --role technician` / `--role customer` | that role, never relaunching (Give help from the start screen, the elevated customer after the UAC prompt) |
| `cretlink --role customer --from-start` | Get help from the start screen: unelevated, explains the UAC prompt first; Back returns to the start screen |

Each role registers only its own Tauri commands (`technician::attach`,
`customer::attach`), so a customer process has no technician command, no IPC
endpoint and no listener, and the reverse. The start screen does not switch
roles in place: it starts the chosen role as a new process and closes.

Get help needs administrator rights for the execution broker (**ST-M009-009**,
mock `#c-admin`). A customer process that is not elevated — Get help from the
start screen, or a `CretLink-Support*` copy — first explains the UAC prompt in
four steps and says that nothing happens before step 4 and nothing is
installed. Only **Continue** starts the program again with `--role customer`
through the UAC prompt (`ShellExecuteExW` with `runas`, in
`cretlink_cli::process`) and closes this window; **Back** returns to the start
screen, or closes a `CretLink-Support*` copy. When the customer declines, the
screen says that administrator tasks will not work and offers **Try again** and
**Continue without admin rights**, which goes on in the same unelevated window
("Admin rights: No"). A process started with `--role` never explains or
relaunches again; it opens and shows its real, possibly unelevated, identity
(REQ-S-011). No dial is accepted before the step is passed. The decisions are
pure functions in `launch.rs` (`customer_admin_step`, `may_relaunch`,
`back_target`); the commands are in `src-tauri/src/customer/admin.rs`
(`get_launch`, `relaunch_elevated`, `continue_without_admin`, `go_back`). In a
`dev-simulate` build off Windows there is no UAC: the step says so, and
**Continue** starts the child as a normal, unelevated process.

Before it connects, Get help removes the staging trees an earlier session left
behind (**ST-M009-010**, the support agent's startup sweep): once the window is
past the UAC step, off the window's thread, and a dial waits for it
(`src-tauri/src/customer/startup.rs`). Every customer process holds a shared
lock on `.in-use.lock` in the staging base until it exits, and the sweep runs
only when it can take that lock exclusively, so a second Get help window never
removes the staging of a session that is live in the first; it skips the sweep
and a later start retries. When the sweep keeps files it could not
remove, `ConsentView.startup_cleanup` carries their number and the recovery
command as it runs in an administrator Windows PowerShell, with the full path
— `& '…\cretlink-cli.exe' recover --remove` when the console build sits next
to the program, else `& '…\cretlink.exe' recover --remove | Out-Host` (or the
`CretLink-Support*` copy's path), since PowerShell waits for a GUI-subsystem
program only when its output is piped — and `#c-find` says so in plain words. The same
`recover [--remove]` runs from `cretlink`, `cretlink-cli` and a
`CretLink-Support*` copy, with the report and exit codes of the support
agent's verb.

Without the WebView2 runtime the program cannot draw its window; it says so in
a native message box before anything starts.

## Run once or install (ST-M009-006)

The start screen (`src/start/`, mock `#start`) shows the version and the
folder the program runs from. Give help offers **Install for me** and **Run
once without installing**; a copy that runs from the install location says it
is installed and offers only Give help. On macOS and Linux, where the
technician role used to open directly, a copy that is not installed shows the
start screen with Give help only, so it can still be installed; the installed
copy opens Give help directly, as before.

**Install for me** opens the install screen (mock `#t-install`): the fixed
location, the Start-menu shortcut (Windows) or desktop entry (Linux) option,
and each AI client with its detection result and the exact command install
would run, each selectable. **Back** (or `Escape`) returns to the start
screen. **Install** copies the program through `cretlink_cli::install`, connects
the selected clients through `cretlink_cli::setup`, starts the installed copy
in the technician role and closes; when a client could not be connected it
says so first and offers **Open CretLink**.

The start-screen process registers, besides `get_role` and `choose_role`, only
`get_app_info`, `install_options`, `install_app` and `open_installed`
(`technician/setup.rs`). The technician process registers `get_app_info`,
`ai_clients`, `ai_client_install`, `ai_client_uninstall`, `uninstall_plan` and
`uninstall_app` for the AI assistants and Settings screens. Settings offers
**Uninstall** only in the installed copy; it shows the plan first, and on
Windows the folder is removed once the window closes.

| | Windows | macOS | Linux |
| --- | --- | --- | --- |
| Program | `%LOCALAPPDATA%\Programs\CretLink\cretlink.exe` | `~/Applications/CretLink.app/Contents/MacOS/cretlink` | `~/.local/bin/cretlink` |
| Record | `…\CretLink\install.json` | `…/Contents/Resources/install.json` | `~/.local/share/CretLink/install.json` |
| Entry | Start-menu `CretLink.lnk`, HKCU uninstall entry | the bundle | `~/.local/share/applications/cretlink.desktop` |

## Live by default

The shipped build opens real transport only. **Clearly-labelled simulated**
targets, the consent demo and the fabricated-consent commands exist for local
development and are compiled in solely with the `dev-simulate` Cargo feature,
which also makes Get help available off Windows for local testing. Both builds
boot the customer window resting; the `dev-simulate` build offers the consent
demo from the technician list (**Simulated demo**). A release build cannot reach
a simulated driver.

The authoritative state machines live in `cretlink-console`
(`apps/console/src/state.rs`) and `cretlink-support-agent`
(`apps/support-agent/src/state.rs`) and are covered by cross-platform tests
there; this crate is a thin renderer and holds no session authority.

## The activity feed (ST-M009-011)

Both roles build the same activity item for every action on the customer
computer (`cretlink_session::ActivityItem`, serialized exactly as
`ActivityItem` / `ActivityEntry` in `src/shared/contract.ts`): its source, the
AI's stated purpose, a plain description, its kind, the real operation, path
and byte count, the honest state, the start time and the duration. A running
item is sent again with the same `id` when it finishes, and items that could
not be shown are marked by a `gap` entry where they were dropped.

| Role | Live event | Snapshot command | Built from | Kept |
| --- | --- | --- | --- | --- |
| Give help | `console-activity` | `get_activity` | the console's audit records of the AI (with the client label), the terminal and manual actions | until the next session starts |
| Get help | `agent-activity` | `get_activity` | the agent's own events | as long as the window's session |

`get_activity` returns the current session's entries, newest last; each role
registers only its own. The technician's display name (the account's full
name, else the user name) is in `ConsoleState.technician.display_name`; the
customer view carries it as `technician`, labelled as what the technician's
computer says.

The rail of the live session (`#t-live`, **ST-M009-008**) asks
`get_bridge_status` and follows the `console-bridge` event for the local MCP
bridge (`BridgeStatus` in the contract): whether its IPC endpoint is `ready`
(bound when the session reaches full control) or `unavailable` (before that,
after the session, or when the bind failed, with a plain-language `message`
and next step that the rail shows as an assertive alert), the number of remote
tools the bridge offers (`cretlink_mcp::McpBridge::tool_names`, the same list
a client's `tools/list` returns), and the labels of the AI clients that called
over local IPC during this session, in first-call order
(`src-tauri/src/technician/bridge.rs`). Every request the agent refuses — one
bound to another session, or a refused transfer chunk — is a failed item on
the customer side too.

The Live tab **Audit** lists this session's audit records, not the feed:
`get_session_audit` reads the session's own audit file (the one being written,
or the sealed one after the end) through `cretlink_console::session_entries`,
which reads only the records the hash chain verified and builds each line from
the record — an action as the feed words it, any other record from its
arguments with the audit's own rendering (`[REDACTED]`, digests) — never a
payload or a chain field. The frontend asks again, at most once a second,
when the feed or the phase changes, and when the tab opens.

## Session history and end reports (ST-M009-012)

| Role | Command | What it does |
| --- | --- | --- |
| Give help | `list_sessions` | the `SessionSummary` of every session in the audit folder within retention, newest first |
| Give help | `open_audit` `{ session? }` | opens the audit folder, or the audit file of a session `list_sessions` lists |
| Give help | `get_session_audit` | the Live tab **Audit** (ST-M009-008): the current or last session's audit records, oldest first, at most 500, as `AuditEntryView` (time, who, record kind, outcome and one secret-free line); pairing, consent, cleanup and end records included |
| Get help | `get_end_report` | the `CustomerReport` of the session that just ended, or `null` |
| Get help | `save_report` | **Save a copy**: a save dialog, then one self-contained HTML file; the saved path, or `null` when cancelled |
| Get help | `open_saved_report` `{ path }` | opens a copy this process saved, and nothing else |

- **The technician's history** is read back from the audit files by
  `cretlink_console::history`. Before sealing, the console writes who ended
  the session and the cleanup report the agent sent with every note
  (protocol minor 11). Each file's chain and seal are verified; a broken or
  unsealed file is listed as such. A summary's `id` is the session id of
  `ConsoleState.session`, so the end screen finds the session that just ended.
  The log still being written is left out, and right after a session ends
  `list_sessions` waits up to ten seconds for its seal.
- **The cleanup result** follows one rule, documented in
  `cretlink_session::report`: the agent's report ok → `cleaned_up`; staged
  artifacts it could not remove → `residuals` with their number; its staging
  sweep failed, or the report is not ok without naming a residual →
  `failed`; no report (a lost connection, an interrupted session) →
  `unknown`, never success. A session that never reached full control needed
  no cleanup. Notes are listed failures and residuals first.
- **The customer's report** is built in memory from what the window showed:
  the activity feed, the agent's own teardown report, who ended the session,
  this computer's hostname and the technician's name as the technician's
  computer reports it. No session secret is in it, and nothing is written
  unless the customer chooses **Save a copy**. The window waits up to fifteen
  seconds after the end for the session's teardown (stopping jobs, sweeping
  the staging area) before building the report, rather than aborting it.
- **Save a copy** opens the native save dialog (`rfd`, the backend the Tauri
  dialog plugin uses, called from Rust; the frontend gets no dialog
  permission) on `Documents\CretLink\` with `YYYY-MM-DD <hostname>.html`.
  The folder is created only then, and removed again when the customer
  cancels and it is still empty. The HTML file has inline CSS, no script and
  no external resource, and every string from the session is escaped
  (snapshot tests in `crates/session/src/report`).
- Both open commands use the platform opener with the path as its only
  argument, never a shell. It is started by its absolute path
  (`%SystemRoot%\explorer.exe`, `/usr/bin/open`), because Windows searches the
  running program's own folder first for a bare name; Linux finds `xdg-open`
  on the user's `PATH`.

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
    base.css       design tokens and building blocks shared by both roles
    shared/        contract.ts (the data of the new interfaces), dom.ts (the
                   text-only DOM builder), morph.ts (in-place patching),
                   icons.ts (the mock's icons as inline SVG)
    start/         the start screen
    technician/    Give help: api.ts (commands), model.ts (snapshot), views/
                   (one pure function per screen), terminal.ts (xterm),
                   main.ts (controller), fixtures.ts, *.test.ts
    customer/      Get help: api.ts (commands), model.ts (snapshot), views/
                   (one pure function per screen, and the compact bar),
                   main.ts (controller of both windows), fixtures.ts, *.test.ts
  dev/gallery.html every screen of both roles from its fixtures (development only)
  vitest.config.ts frontend tests (jsdom)
  src-tauri/
    Cargo.toml     detached crate; binary `cretlink`
    tauri.conf.json, capabilities/default.json, icons/
    src/main.rs    subcommand, start screen or role
    src/launch.rs  the launch decision (unit-tested)
    src/lib.rs     window per screen, process start and elevated relaunch
    src/start.rs   get_role, choose_role
    src/opener.rs  opens a validated file or folder with the platform opener
    src/technician/  console state, commands, live-session driver and the
                     session history (history.rs)
    src/customer/    agent state, commands, live-session driver, the end
                     report (report.rs), the admin step (admin.rs), and the
                     compact bar and closing (windows.rs)
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

`npm test` runs the frontend tests (Vitest in jsdom, no Tauri). To look at
every screen without the backend, run `npx vite` and open `/dev/gallery.html`
(`?f=<technician fixture>`, `?c=<customer fixture>`); the gallery is not part
of the production build.

`npm run build` must run before a bare `cargo build` inside `src-tauri`, because
Tauri embeds the built frontend from `../dist`. Without `tauri/custom-protocol`
a build loads the frontend from the dev server instead.

## The icon (ST-M009-015)

`src-tauri/icons/icon.svg` is the source of the mark. The six files next to it
are generated from it and are the ones the application uses: `32x32.png`,
`128x128.png`, `128x128@2x.png`, `icon.png`, `icon.ico` (16, 24, 32, 48, 64 and
256 pixels) and `icon.icns`. To regenerate them, run the Tauri CLI into an
empty folder and copy those six files back; the store logos and the Android
and iOS files it also writes are not used:

```sh
cd apps/app
npx tauri icon src-tauri/icons/icon.svg -o <an empty folder>
```

The same source gives the same PNG and ICO files byte for byte; `icon.icns`
holds the same images in an order that differs from run to run. No other logo
is drawn inside the windows.

## The technician interface

The technician window (**ST-M009-008**) follows the approved mock: Home,
Sessions, AI assistants and Settings in the navigation, whose foot names this
computer, the version and whether this copy is installed or runs once. The
window draws no bar of its own: the operating system's title bar is the only
one, and the live session starts with its full-control band.

- This computer is its host name and its operating system by name and
  release, without build number or architecture
  (`src-tauri/src/technician/host_os.rs`): on Windows from the registry facts
  the customer side reports too (`cretlink_support_agent::sysinfo::os_name`,
  for example `Windows 11 Pro 25H2`), on macOS `macOS` with the product
  version of `SystemVersion.plist`, on Linux the `PRETTY_NAME` of
  `/etc/os-release`. What cannot be read is not guessed: the foot then says
  `Windows`, `macOS` or `Linux`. In a window narrower than 760 pixels the
  navigation shows its icons only, without the foot.
- Every screen is a pure function of one `TechnicianSnapshot`
  (`technician/model.ts`): the backend's `ConsoleState`, the activity feed,
  the session history, the AI clients and this copy's details. The controller
  (`technician/main.ts`) patches the live DOM with each new render
  (`shared/morph.ts`), so focus, scroll positions and the embedded terminal
  survive every update.
- Interactive elements carry `data-action`; one delegated listener turns them
  into commands. Text from the customer computer or an AI (operations, paths,
  hostnames, notes, purposes, codes) only ever becomes a text node
  (REQ-S-016, REQ-S-018).
- The activity feed (`console-activity`, ST-M009-011) is upserted by item id,
  survives tab switches and the end of the session, and is cleared only when
  the next session starts. Items shed under load show as "N items were not
  shown".
- The end report and the history come from `list_sessions` (ST-M009-012).
  Until the sealed audit file is read, the end screen says it is preparing the
  report; unknown cleanup is never shown as success.
- The AI-assistant page shows the exact commands before Connect, Replace or
  Disconnect runs them, and warns when a run-once copy registers itself. Its
  Claude Code entry says that it also serves the Code tab of Claude Desktop,
  which uses the same registration; Claude Desktop has no entry of its own
  (`docs/mcp-clients.md` covers its Chat tab).
- A command the backend does not offer yet (`list_sessions`, …) is shown as
  "not available yet" rather than failing.
- The consent screen shows `DisclosureSummary::DEFAULT_TEXT` from a copy in
  `technician/disclosure.ts`; a test fails when the two differ.

## The customer interface

The customer window (**ST-M009-009**) follows the approved mock (`#c-admin`,
`#c-find`, `#c-sac`, `#c-consent`, `#c-live`, `#c-mini`, `#c-end`), on the
same pattern as the technician: every screen is a pure function of one
`CustomerSnapshot` (`customer/model.ts`) — the backend's `ConsentView` and
`LaunchView`, the activity feed, the consoles found nearby, the timeline
filter and the end report.

- Every screen starts with the state band in words: **No access**, **Your
  decision** or **Under full control**, with a short explanation.
- **Choose your technician** lists nearby consoles as an untrusted list:
  nothing is preselected and nothing connects on its own. Each shows the name
  and host name its computer advertises (the technician advertises its host
  name as the DNS-SD instance name), the address, and "this network" or
  "unreachable". The code field is always there. This computer's host name,
  signed-in account and admin rights are shown as the process reports them.
- **Your decision** shows a scannable summary above the verbatim
  `DisclosureSummary::DEFAULT_TEXT`; **Decline** is as prominent as **Allow
  full control**, and neither is focused by default.
- **Under full control** shows the start and elapsed time, **Happening now**
  (the running item with its purpose or plain description, source and elapsed
  time, the real operation beneath), the **Screen** tile from the agent's
  `DesktopActivity` signals (`ConsentView.screen`: not used, being observed,
  being controlled, or not in use now), **Files** and **Commands**, and **What
  happened** newest first with the filters All, Commands, Files and Screen. A
  file that left the computer says "left this computer" in words and is amber;
  shed items show as "N items were not shown". There is no reboot-resume
  control (FND-0005).
- **Stop** and `Escape` act only while a session exists
  (`ConsentView.session_active`); `stop_session` refuses on the resting window.
- **Make small** (`make_small`) shows the compact bar — a second window,
  labelled `mini`, always on top, without a system frame, moved by dragging
  (`drag_window`) — and hides the main window; **Open** (`open_main`) does the
  reverse. The bar loads the same frontend, which renders the compact view in
  the window with that label, and uses the customer role's commands only. While
  a session exists one of the two windows is always visible: minimising the
  main window shows the bar, the bar refuses to close, and when the session
  ends the main window comes back with the end report (REQ-S-002).
- Closing the main window during a session — the close button, `Alt+F4`, or an
  exit request — is held in Rust (`CloseRequested`, `ExitRequested`) and the
  window asks "Stop the session and close?". **Stop and close**
  (`stop_and_close`) revokes the session, waits for its cleanup report like the
  teardown of ST-M009-012, then exits. Outside a session the window closes at
  once; right after **Stop**, while the session task still revokes and cleans
  up, closing takes the same bounded wait before it exits. The decision is
  `windows::close_action`, a pure function.
- A **Stop** or decline is always put on the wire before the window waits for
  the teardown: the session pump prefers the customer's intents over wire
  events and sends any still queued when it ends. Each dial has its own
  generation, so a finished dial still tearing down never writes into the next
  session's window.
- **Session ended** names who ended it ("You stopped the session", "<name>
  ended the session", "The connection was lost"), the counts, each cleanup
  step with failures first (unknown is never shown as success), **Save a
  copy**, then the saved path with **Open**, and **Close CretLink**.
- A session that ends before control returns to the technician list, saying
  how it ended.

| Event | Window | Payload |
| --- | --- | --- |
| `agent-view` | both | the whole `ConsentView` after every change |
| `agent-activity` | both | one `ActivityEntry` per change (upsert by `item.id`) |
| `customer-close-requested` | main | none: a close or exit was held during a session |

## Accessibility notes

- Every screen has a skip link to its main content.
- State is announced through `aria-live` regions; errors through an assertive
  alert (REQ-N-015).
- **Stop** is a native `<button>`, always reachable, with `Escape` as a shortcut
  in every live phase: listening, connecting, comparing the code, waiting for
  consent and full control (REQ-N-014, REQ-F-018). On the customer's resting
  window there is no Stop, and `Escape` does nothing.
- No state relies on colour alone: every badge and level carries text
  (REQ-N-013).

## Known gaps

- A manual **screen-reader** pass (Narrator/NVDA on Windows, VoiceOver on macOS)
  of the start screen, identity, approval-code, disclosure, consent and stop
  surfaces (REQ-N-013, REQ-N-014).
- The UAC prompt of Get help (`#c-admin`, accepted and declined), the compact
  bar (always on top, movable, minimising the main window), closing during a
  session with `Alt+F4`, and a Windows run of both roles against each other
  are manual checks of ST-M009-009 (`docs/manual-tests.md`, MT-12). On Linux
  under GTK the compact bar is at least 200 pixels high.
- Install, update and uninstall on Windows (Start-menu shortcut, uninstall
  entry, folder removal after exit) and macOS (the generated bundle) are manual
  checks of ST-M009-006 (`docs/manual-tests.md`, MT-11).
- A run-once copy writes nothing of its own but the audit trail, but the
  webview keeps its profile in the per-user data folder Tauri chooses
  (`dev.cretlink.app`).
- A manual check on Windows that a customer session leaves no file unless
  **Save a copy** is used, and that the save dialog opens on
  `Documents\CretLink\` (ST-M009-012).
- A manual walk-through of the technician interface on Windows and macOS, and
  a keyboard and screen-reader pass (Narrator, VoiceOver), are manual checks
  of ST-M009-008.
