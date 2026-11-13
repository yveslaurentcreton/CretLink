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
command as it runs in Windows PowerShell, with the full path
— `& '…\cretlink-cli.exe' recover --remove` when the console build sits next
to the program, else `& '…\cretlink.exe' recover --remove | Out-Host` (or the
`CretLink-Support*` copy's path), since PowerShell waits for a GUI-subsystem
program only when its output is piped — and `#c-find` says so in plain words.
It also carries whether PowerShell is to be opened as administrator for it: a
process with administrator rights keeps its files in `CretLink Data` in
`Program Files`, a process without them in `CretLink` in the account's local
application data, and the command covers the place of the kind that runs it
(ST-M006-020). The same
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
| Settings | `settings.toml` and `relay-ca.pem` next to the record (ST-M007-011) | the same | the same |
| Entry | Start-menu `CretLink.lnk`, HKCU uninstall entry | the bundle | `~/.local/share/applications/cretlink.desktop` |

## The relay of this copy (ST-M007-011)

When the two computers are not on one network, their copies meet on a relay
that the technician hosts. A **relay profile** tells a copy which one: the
relay's `address`, an optional admission `key`, an optional certificate
authority file `ca` and an optional `name` in the certificate
([`docs/relay-server.md`](../../docs/relay-server.md), section 3).

| Read from | By | Rules |
| --- | --- | --- |
| `cretlink.toml` next to the program | both roles | the one place the customer role reads; nothing is written there |
| `settings.toml` next to the install record | the installed technician copy | written by **Settings → Relay**; removed by `cretlink uninstall` |

- **The lookup** is `relay::TechnicianRelays::active` for Give help and
  `relay::next_to_program` for Get help (`src-tauri/src/relay.rs`). A file
  next to the program wins over the settings, also when that file cannot be
  used: a profile that is wrong is explained and never passed over for
  another relay. A copy that runs once reads no settings. Without a profile
  nothing about a relay is offered or dialled (REQ-N-003).
- **The file is input that somebody else may have written**
  (`cretlink_cli::relay_profile`): a small form of TOML, read up to 16 KiB,
  with every unknown section and setting refused. A problem names the line and
  the setting in plain words and never repeats text from the file.
- **The key never comes back.** It is held as an admission token
  (`cretlink_relay::AdmissionToken`: erased when dropped, redacted in
  `Debug`). No view sent to the window holds it — `RelayView.keySet` says
  only whether there is one — and a form that leaves the key alone says
  `keep`. It is not logged and is no argument of any audit record.
- **Always TLS** (`relay::EndpointDial`). A relay is reached over `wss` only,
  with its certificate checked against the system's authorities or the
  profile's `ca` file, for the profile's `name` or else the host of its
  address. A check that fails ends the dial before anything is sent; nothing
  falls back to a connection without TLS.
- **Addressing only** (ADR-0016 §2, §4). A profile grants nothing. Saving one
  opens no connection.

| Command (Give help) | What it does |
| --- | --- |
| `get_relay_settings` | which relay this copy uses and where it comes from (`RelaySettings` in `src/shared/contract.ts`): the file next to the program and, in the installed copy, the settings; a profile that cannot be used with its file, setting and what is wrong |
| `choose_relay_ca` | the system's dialog for the certificate authority file, opened from Rust; the file is read, checked and kept under a number (`ChosenCa`), so the window names no path of this computer |
| `save_relay` `{ form }` | keeps the relay of the form (`RelayForm`) in the settings, and a copy of a chosen certificate file next to them; refuses in a copy that runs once |
| `test_relay` `{ form? }` | reaches the relay once and answers in words (`RelayTest`): the relay of the form as it stands, or without a form the one this copy uses. It presents a ticket of its own, meets nobody and saves nothing |
| `remove_relay` | removes the relay from the settings, with its key and its certificate file |

**Settings → Relay** (`src/technician/views/relay.ts`) shows the form in the
installed copy, the relay of the file when a file wins (with what the settings
hold as the relay that is not used), and in a copy that runs once where its
relay goes. The key field is always empty; after **Save** the form is drawn
anew so that no typed key stays in a field, and the keyboard focus is put back
where it was. **Remove** asks first. A refusal names the setting it is about
and moves the focus there.

On the customer's side a relay code typed into a copy whose profile cannot be
used is answered with the file and what is wrong with it, before anything is
dialled (`customer::typed_target`, stage `relay_profile`).

The settings file is created for this user only where the system has file
modes (`0600` on macOS and Linux); on Windows it has the access rules of the
user's `%LOCALAPPDATA%` folder.

To try the screen without a backend, run `npx vite` and open
`/dev/relay.html` (`?copy=installed|kept|file|file-problem|run-once`,
`?relay=admit|key|down`): the real controller against a copy and a relay held
in memory.

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

## The network to wait on (ST-M009-016)

A technician's computer is often on several private networks at once: its
local network, a VPN, the virtual switch of a hypervisor. The customer can
reach only one of them, so the session waits on exactly one address, and the
waiting screen (`#t-wait`) names its adapter.

| Command | What it does |
| --- | --- |
| `get_listen_networks` | this computer's private addresses that are up, each with the name of its network adapter (`ListenNetwork` in `src/shared/contract.ts`), in the order of `cretlink_discovery::listen_candidates` |
| `start_live_session` | waits on the adapter chosen earlier in this run while it still has an address, otherwise on the first choice of `cretlink_discovery::first_choice`: the address of the default route unless that adapter is a point-to-point link |
| `switch_listen_network` `{ address }` | only while waiting: stops the listener, waits until it is gone, and starts one on `address` with a new connection code; `address` must be one that `get_listen_networks` lists |

- With more than one network the waiting screen offers the others under
  **Wait on another network**. While the listener moves it shows no code. The
  first Escape closes the open list; Escape then cancels as always.
- The choice is kept for the later sessions, by adapter name
  (`src-tauri/src/technician/network.rs`). The installed copy writes it to its
  settings, next to the relay, so that it holds after the program is closed
  (ST-M007-011); when that cannot be written the window says so (the
  `console-notice` event). A copy that runs once keeps it in memory only and
  writes nothing to disk.
- Discovery announces the bound address alone, on its adapter alone
  (`cretlink_discovery::Advertiser`), so the customer's list never holds an
  address that nobody listens on.
- Without any private address the listener binds loopback and is not
  announced; a public or wildcard address is never bound (REQ-S-010).

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

The technician looking through files in the explorer (ST-M010-002) is one
item on both sides, "Looked through files and folders", which names the
folder that is being looked at and follows along
(`cretlink_session::ActivityFeed::look`). It goes on only while it is the
newest entry: a change, a transfer, a command, an action of the AI or a gap
is an entry of its own, and the next look starts a new item after it. A look
that was refused is a failed item of its own and is never replaced. A look of
the AI keeps its stated purpose and stays an item of its own.

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

## The file explorer (ST-M010-002)

The **Files** tab of the live session is a file explorer for the customer
computer: the drives, folders in a **Details** and a **List** view, a path bar,
back, forward, up and refresh. From it the technician sends files and folders
to the open folder, copies the selection to this computer, renames and
deletes. The transfers of the session are under it, as its queue.

Browsing:

| Command | What it does |
| --- | --- |
| `explorer_drives` | the drives of the customer computer (`ExplorerDrive` in `src/shared/contract.ts`): root, kind, label, total and free space; a value the customer computer could not read is absent, never zero |
| `explorer_open` `{ path, listing }` | opens the place `path` names and lists it. The folder arrives on the `console-explorer` event while the command runs (`ExplorerEvent`): `opened` once, with the folder as the customer computer names it and the first page, then a `page` for each further page. The answer says how the listing ended (`ExplorerListing`): `complete`, `cancelled` (left for another place) or `limit` |

- **No authority of its own.** Every command of the explorer exists in a
  technician process only. Those that ask something of the customer computer,
  or prepare to, work only while a session is under full control: without
  one, or without its open audit trail, they refuse with
  `no_consented_session` before anything is asked, opened or kept
  (`src-tauri/src/technician/explorer.rs`, `explorer/commands.rs`). Nothing
  is asked of the customer computer before the technician opens the tab. A
  stop or a revocation ends a listing, every transfer and every removal with
  the session, and what was listed and queued is dropped with the session:
  the next one starts with an empty explorer and an empty queue.
- **Every look is the technician's, and recorded.** The commands go through
  `cretlink_console::TechnicianFiles`: requests carry the origin `console`,
  and each look is a pair of audit records, `console_fs_drives` or
  `console_fs_list`, one pair for one folder however many pages it takes.
- **The real location.** `path` is a path on the customer computer, never one
  on this computer. A text that is no rooted path on a drive is refused before
  anything is asked. What is shown and sent afterwards is the place as the
  customer computer names it, built from its own answers: the root as the
  drive list gives it, each further name as a listing of the folder above it
  gives it (`cretlink_console::explorer`). A path typed in another letter case
  therefore shows its real names. A name the folder above does not list, a
  short name such as `PROGRA~1` included, is not found; a link is shown and
  not followed; a path to a file opens the folder that holds it, with the file
  selected. A name is looked for among at most 100 000 entries of a folder.
  Spelled as the folder lists it, it is asked for at its place in the folder's
  order and normally found with one request; in another letter case the
  folder is read from its start. When the search stops at that bound, the
  refusal says so, never that the name does not exist.
- **Stays where it was.** A place that is refused is shown as that refusal in
  the explorer, the folder that was open stays on screen, and a typed path
  comes back into its field to be corrected.
- **Thousands of entries.** A page is as long as the customer computer makes
  it (ST-M010-005); the first page is on screen at once and the rest fills in
  while the technician works. Only the rows in view are drawn, each at its
  place in a box as tall as the whole list (`src/technician/explorer.ts`,
  `views/explorer.ts`). Pages are sorted into what is listed — folders first,
  names without regard to case and digits by their value, or by date, type or
  size — while the selection stays on its entries and the row in focus, or
  else the first row in view, stays where it is. A listing stops at 100 000
  entries and says that the folder holds more.
- **Honest states.** Opening, loading, an empty folder, a list that is not the
  whole folder (with why), a place that was refused and a session that ended
  each have their own words. Nothing reads as an empty folder that is not one.
- **Selection and keys.** Click, `Ctrl` and `Shift` click, `Ctrl+A`; the arrow
  keys, `Home`, `End`, `Page Up` and `Page Down`, with `Shift` to extend and
  `Ctrl` to move without selecting; letters go to the row that starts with
  them. `Enter` or a double click opens a folder or a drive, `Backspace` or
  `Alt+Up` goes up, `Alt+Left` and `Alt+Right` go back and forward, `F5`
  refreshes, `Ctrl+L` opens the path field, `F2` renames and `Delete`
  deletes. The context menu (right click, `Shift+F10` or the menu key) and
  the buttons above the list are one list of actions (`actionsFor` in
  `explorer.ts`): **Open**, **Download**, **Download to…**, **Rename**,
  **Delete**, **Upload files…**, **Upload a folder…** and **Refresh**. An
  action of the menu is for the rows the menu opened on, a button and a key
  for the selection.
- **Escape.** An open question, the context menu, a name that is being typed
  or the path field takes the first `Escape`; `Escape` then stops the
  session, as always.
- **A grid for a screen reader.** The list is a `grid` with rows and cells,
  the number of all rows and the place of each, the column that sorts
  (`aria-sort`), the selected rows and the row the keyboard is on
  (`aria-activedescendant`). The grid itself takes the keyboard; changes of
  place are announced politely, a refusal assertively.

Sending, copying, renaming and deleting (`explorer/commands.rs`):

| Command | What it does |
| --- | --- |
| `explorer_pick_upload` `{ kind }` | the system's dialog for `"files"` or for one `"folder"`; what was picked is kept as an offer (`UploadOffer`: a number, the count and the first names), or `null` when the dialog was closed |
| `explorer_send_check` `{ offer, folder }` | lists `folder` on the customer computer and answers which names of the offer are taken there (`TransferCheck`) |
| `explorer_send` `{ offer, folder, choices }` | queues the offer for `folder`; `choices` holds `replace`, `keep_both` or `skip` for each name that is taken; answers how many were queued, left out and skipped (`TransferQueued`) |
| `explorer_choose_folder` | the system's dialog for a folder of this computer to copy to; the folder is kept under a number (`ChosenFolder`) |
| `explorer_download_check` `{ names, to? }` | which of the selected names are taken in the folder a copy would go to; nothing is asked of the customer computer and no folder is made |
| `explorer_download` `{ folder, names, to?, choices }` | queues the entries `names` of `folder` for a copy to this computer |
| `explorer_transfers` | the queue as it is (`TransferEntry[]`) |
| `explorer_cancel_transfer` `{ transfer }`, `explorer_cancel_transfers` | cancels one transfer, or all that wait or run |
| `explorer_show_folder` `{ transfer }` | shows the folder of a copy that arrived in this computer's file manager |
| `explorer_rename` `{ folder, name, to }` | gives one entry of `folder` another name in that folder |
| `explorer_delete` `{ folder, items, removal }`, `explorer_cancel_delete` `{ removal }` | deletes the selection and answers what was removed and what could not be (`DeleteReport`); cancels that delete |

| Event | Payload |
| --- | --- |
| `console-explorer-drop` | an `UploadOffer`: files and folders were dropped on the window |
| `console-transfers` | `TransfersEvent`: the entries of the queue that changed, each replaced by its number |
| `console-removal` | `RemovalProgress`: the item a delete is at, and the files and folders removed so far |

- **The window names no path of this computer.** Files to send come from a
  drop on the window or from the system's dialog, opened from Rust (`rfd`;
  the frontend has no dialog permission). The backend keeps the paths and
  tells the window the number of the offer and the names; a send names the
  offer. A copy goes to `Downloads\CretLink\<target>`, which is made when the
  first file goes there, or to a folder chosen in the system's dialog and
  named by its number. **Show in folder** shows the folder a copy of the
  queue went to, with the opener started by its full path: `explorer.exe`
  in the Windows folder the system names, `/usr/bin/open`, or `xdg-open` from
  `/usr/bin`, `/bin` or `/usr/local/bin` (`opener::show_folder`).
- **A name from the customer computer is untrusted.** Every name of a listing
  must be one plain name before a copy is written under it: no separator, no
  colon, not `.` or `..`, no reserved device name, no trailing dot or space
  (`explorer/local.rs`). Anything else is left out with a row that says why,
  and nothing is written outside the folder that was chosen. What a selected
  name is — a file, a folder, a link — comes from a listing the backend asks
  for itself, not from what the window says. The folder for `<target>` is
  the host name with everything but letters, digits, `-`, `_` and `.`
  replaced.
- **A name that is taken.** A look at the folder comes before anything is
  queued, and the window asks once for each name that is taken: **Replace**,
  **Keep both** (the new one becomes `name (2).ext`) or **Skip**, with a
  button to do the same for the names that follow. A file replaces a file;
  a folder goes into a folder, where its files replace those of the same
  name and everything else stays. **Replace** is not offered where the kinds
  differ, and is refused when it is asked for all the same. Nothing is
  replaced without the request: a name that turns out to be taken after the
  look fails its transfer and leaves what is there.
- **Folders go file by file.** A dropped folder is walked on this computer,
  a folder of the customer computer through its listings; each file is a
  transfer of its own, and the folders are made on the way, empty ones too.
  A link is never followed and has a row that says so. One send or one copy
  queues at most 10 000 files and folders, 32 folders deep; a folder that
  holds more, or that could not be listed to its end, has a row that says it
  is not complete.
- **The queue** (`explorer/transfers.rs`) carries one transfer at a time,
  because the session serves one transfer at a time; the others wait. What
  did not arrive is shown first, with its reason in the words the activity
  has for it, then what runs, with its progress, and what waits, then what
  ended. The panel draws a bounded number of rows of each and counts the
  rest. A transfer that waits is cancelled at once; a running one ends at its
  next step and its unfinished part goes with it — the customer computer
  discards the part of a send, and the row says so when it could not; the
  part of a copy is removed from this computer.
- **Honest states** (REQ-N-008). `done` is a transfer whose length and
  SHA-256 were verified, and only that reads as sent or copied. A failed,
  cancelled or left-out transfer never does. When the session ends, the
  running transfer and every waiting one end as stopped; the queue, the
  explorer's alert and, once the explorer is dropped with the session, the
  window's own alert say so.
- **Rename** types the new name in the row (`F2`). `Enter` takes the name,
  and so does a click elsewhere in the explorer, as in the Windows explorer;
  `Escape` leaves the name as it was. A click on a row goes on to select
  that row. A click on a control is not also that control's press when it
  sent a new name: what the control would act on is being renamed at that
  moment. A name must be one name. One that cannot be used keeps the field
  open after `Enter`; after a click the name is left as it was and the
  window says why. A name that is taken is refused by the customer computer,
  which never replaces by a move.
- **Delete** asks once for the whole selection, with the number of items and
  the sentence that there is no recycle bin on this path; **Cancel** has the
  focus. A folder is removed with everything in it, a file or a link as that
  one entry, a link as the link. The answer counts an item as removed only
  when the customer computer said so and names what could not be removed
  with its reason; the list is then read again. One delete runs at a time
  and can be cancelled between two of its steps.
- **Recorded and shown.** Every transfer and every change goes through
  `cretlink_console::TechnicianFiles` and is a pair of audit records of the
  technician: `console_send`, `console_download`, `console_make_dir`,
  `console_move`, `console_delete`, `console_remove_tree`, and
  `console_fs_stat` for the look after a cancelled send. In the customer's
  activity each is an item of its own; the looks stay the one item that
  follows along. The path of a file on this computer is no argument of a
  record; it stands only in the reason of a transfer that failed on this
  computer, which names the file that could not be read or written.
- **Questions for a screen reader.** A name that is taken is a `dialog` and a
  delete an `alertdialog`, each with its title and its text, `Tab` going
  round its buttons and the first `Escape` leaving it. The queue is a list
  whose rows say their state in words; a running transfer has a progress bar
  that says its name and its number.

To try the explorer without a customer computer, run `npx vite` and open
`/dev/explorer.html`: the real controller and views against a customer
computer held in memory (`src/technician/simulated-computer.ts`), with Tauri
replaced by its own mocks. It has a folder of five thousand entries and one of
sixty thousand. A browser cannot hand the page a real drop, so a bar at the
foot of the page drops prepared files and folders and ends the session. The
controller's tests drive the same simulated computer.

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
                   main.ts (controller), fixtures.ts, *.test.ts; the file
                   explorer in explorer.ts (state), views/explorer.ts,
                   explorer-control.ts (controller) and simulated-computer.ts
                   (development and tests only)
    customer/      Get help: api.ts (commands), model.ts (snapshot), views/
                   (one pure function per screen, and the compact bar),
                   main.ts (controller of both windows), fixtures.ts, *.test.ts
  dev/gallery.html every screen of both roles from its fixtures (development only)
  dev/explorer.html the file explorer against a simulated customer computer
                    (development only)
  dev/relay.html   Settings → Relay against a simulated copy and relay
                   (development only)
  vitest.config.ts frontend tests (jsdom)
  src-tauri/
    Cargo.toml     detached crate; binary `cretlink`
    tauri.conf.json, capabilities/default.json, icons/
    src/main.rs    subcommand, start screen or role
    src/launch.rs  the launch decision (unit-tested)
    src/lib.rs     window per screen, process start and elevated relaunch
    src/start.rs   get_role, choose_role
    src/opener.rs  opens a validated file or folder with the platform opener
    src/relay.rs   the relay of this copy, for both roles: where its profile
                   comes from, the dial over wss, and the words of a failure
    src/technician/  console state, commands, live-session driver, Settings →
                     Relay (relay_settings.rs), the
                     session history (history.rs) and the file explorer
                     (explorer.rs; in explorer/: commands.rs, the queue in
                     transfers.rs, rename and delete in changes.rs, the rules
                     for names and folders of this computer in local.rs, and
                     the customer computer as they reach it in target.rs)
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
- The Files tab is the file explorer (ST-M010-002, above). Its state is part
  of the snapshot for one session's full control; a folder that arrives in
  pages and a change of the queue redraw the tab alone, and scrolling redraws
  only the rows.
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
- The file explorer has not been run against a customer computer on real
  hardware, and no screen reader has read its grid, its questions or its
  queue yet: the steps of scenario MT-14 in `docs/manual-tests.md`
  (ST-M010-002). Its frontend was tested in jsdom and looked at in Chromium
  through `dev/explorer.html`, not in the application's own window; a drop
  from a real file manager and the system's own dialogs were not exercised.
  Off Windows a customer computer lists its root file system as its one
  drive, which no path the wire accepts can name, so the explorer shows that
  drive and a refusal there, and the queue is tested against a customer
  computer held in memory; the way through a real session runs on Windows.
- While a file is being sent or copied, the session serves nothing else: a
  folder that is opened, the terminal and an AI assistant's call wait until
  that file has ended. Cancelling the transfer frees the session at the
  transfer's next step.
- A copy to this computer that is cancelled, or that fails on this computer,
  stays "in progress" in the customer's activity until the session ends: the
  customer computer is not told that the copy was given up.
