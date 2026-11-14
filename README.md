# CretLink

**Attended remote support, driven by your own AI agent.**

CretLink lets an AI agent on a technician's computer work on a customer's
Windows computer, in a session that the customer starts, approves, watches and
can stop at any moment. The agent is the one you already use: Claude Code,
Codex CLI, Gemini CLI or another client that speaks the Model Context Protocol
(MCP). The customer's computer needs no AI program, no AI account and no API
key.

<p align="center">
  <img src="docs/images/technician-session.png" width="61%" align="top" alt="The technician's window during a session: the customer's computer, the connected AI assistants, and every action with its stated purpose and result.">
  &nbsp;
  <img src="docs/images/customer-working.png" width="30.5%" align="top" alt="The customer's window during the same session: what is happening now, what happened before, and a Stop now button.">
</p>
<p align="center"><sub>One session from both sides: the technician's window and the customer's window. Sample data.</sub></p>

**New here?** The [documentation site](https://yveslaurentcreton.github.io/CretLink/)
explains CretLink in six short pages with pictures: what it is, how a session
works, how to get started, and what it promises about security.

## How a session goes

1. **The technician** starts `cretlink`, chooses **Give help** and waits for
   the customer. The window names the network it waits on; a computer that is
   on several, such as a VPN next to the local network, can choose another.
2. **The customer** starts the same program, chooses **Get help** and answers
   the Windows administrator prompt. A copy named `CretLink-Support.exe` opens
   Get help straight away, for example from a USB stick. Nothing is installed,
   unless the customer chooses to [keep the support program](#a-customer-keeps-the-support-program)
   for next time.
3. The customer picks the technician from the computers found on the network,
   or types the connection code the technician gives them.
4. Both windows show the same six digits. The technician reads them aloud;
   when they differ, either person stops.
5. The customer reads what full control means, then allows or declines it.
6. The technician's AI agent works through CretLink: it runs PowerShell and
   Command Prompt commands, follows and cancels jobs, and sends and fetches
   files. Both windows show every action as it happens, with the purpose the
   agent stated for it.
7. Either person stops the session. CretLink removes what it placed on the
   customer's computer and reports what it could not remove.

The connection is direct, authenticated and encrypted end to end, between two
computers on the same network. AI credentials, provider tokens and login state
stay on the technician's computer.

## Get started

1. Download the archive for your operating system from the
   [latest release](https://github.com/yveslaurentcreton/CretLink/releases/latest)
   and [verify it](docs/verifying-artifacts.md). Use the same release on both
   computers: a session starts only when both speak the same protocol version.
2. On the technician's computer, unpack it, start `cretlink` and choose
   **Give help**, then **Install for me** or **Run once without installing**
   (see below).
3. Connect your AI clients from the **AI assistants** screen, or with
   `cretlink mcp install`. [MCP client setup](docs/mcp-clients.md) has the
   details and the manual configuration for each client.
4. Give the customer `cretlink.exe` from the Windows archive, renamed to
   `CretLink-Support.exe`, and follow the steps above.

The releases are not code-signed yet, so Windows shows a warning for an
unknown publisher. The checksums are the integrity check for now.

## Run once or install

The technician side is one program, `cretlink`, that runs from wherever it was
unpacked. Its start screen offers, under **Give help**:

- **Run once without installing.** The copy writes nothing but its audit trail
  (in the per-user data folder). Its window keeps its webview profile in a
  temporary folder, which it removes when it closes (see below). AI clients
  you connect from it point at this copy and break when it is moved or
  deleted; the AI assistants screen says so.
- **Install for me.** Copies `cretlink` (and `cretlink-cli`) to a fixed
  per-user location — `%LOCALAPPDATA%\Programs\CretLink\` on Windows,
  `~/Applications/CretLink.app` on macOS, `~/.local/bin/` on Linux — adds a
  Start-menu shortcut (Windows) or a desktop entry (Linux), connects the AI
  clients you select, and starts the installed copy. It needs no administrator
  rights and adds no service and nothing that starts by itself. Installing a
  newer download again updates the copy in place, so the AI clients keep
  working.

`cretlink uninstall` (or **Uninstall** in Settings, or the Windows *Installed
apps* entry) shows what it will remove and asks first, then removes the
installed files, the settings of the installed copy (its relay with its key,
and the network it waits on), the shortcut or desktop entry, the uninstall
entry, the installed copy's webview profile and the AI client registrations
the install made, but only while they still point at the installed copy. The
audit trail stays. On macOS and Linux an installed copy
opens Give help directly; a copy that is not installed shows the start screen
so it can still be installed. A `CretLink-Support*` copy, for customers, never
installs or uninstalls the technician side; the commands it accepts are
`recover [--remove]` and its own `uninstall` (see the next section).

The window of `cretlink` is a webview, which keeps a profile (cache and local
storage, no session secret) in a folder:

- A copy that is not installed, and Get help wherever it is started from,
  uses a new folder of its own, `CretLink-webview-<random>`, in the temporary
  folder, and removes it when its last window has closed. If the program is
  killed, or the webview still holds files a few seconds after closing, the
  folder stays until the next start of a CretLink window, which removes the
  folders of copies that no longer run. `cretlink recover` lists them and
  `cretlink recover --remove` removes them.
- The installed copy keeps its profile next to the audit trail
  (`%LOCALAPPDATA%\CretLink\webview`, `~/.local/share/CretLink/webview`).
  Uninstall removes it.
- A window with administrator rights (Get help after the Windows prompt)
  makes that folder in `webview` in `%ProgramFiles%\CretLink Data`, where
  only administrators can write, and looks for leftovers only there: it
  removes nothing in the temporary folder or the local application data that
  its environment names. Before its window opens it drops the `WEBVIEW2_*`
  variables from its environment.
- Releases up to 0.24.1 kept the profile of both roles in `dev.cretlink.app`
  in the per-user data folder (`%LOCALAPPDATA%`, `~/.local/share`) and left it
  there. Uninstall and `cretlink recover --remove` remove that folder. Run
  with administrator rights, `recover` does not look at the account's own
  folders and says so: run `recover` there again without administrator
  rights.

Known gap: on macOS the webview is WebKit, which keeps this data in its own
folders under `~/Library`, where CretLink can neither direct nor remove it. It
stays after a copy that runs once has closed and after an uninstall. Only the
technician role exists on macOS.

## A customer keeps the support program

A customer who gets help more than once can keep the support program on the
computer, and start it from the Start menu next time. It is the customer's
own choice, made in the Get help window: **Keep CretLink Support on this
computer** is offered before a session and on the end report, never during a
session, and only in a window that Windows gave administrator rights. The
screen says what is added before anything is added:

- the program, in `%ProgramFiles%\CretLink Support\CretLink-Support.exe`,
  where only an administrator can change it;
- a Start-menu entry **CretLink Support** for everyone who uses the computer,
  and a shortcut on the desktop when the customer asks for one;
- an entry in Installed apps, with the version;
- the relay settings of the copy that was kept (`cretlink.toml`, and the
  certificates of its certificate file), when it had any. The screen names
  that relay first. On the computer the settings can then be read by every
  account, with the admission key of the relay in them.

Nothing else is added: no service, no scheduled task, nothing that starts
with Windows, no listener and no stored permission. Between sessions no
CretLink process runs. A session from the kept program takes every step of a
session from a downloaded copy: the customer starts it, Windows asks for
administrator rights, the technician is chosen or the code typed, the
approval code is compared and full control is allowed, each time.

- **Update.** The kept program does not update itself and contacts nobody by
  itself. A newer copy that the customer starts offers to update it and names
  both versions; an older copy never replaces a newer one. Relay settings
  next to the newer copy replace the kept ones, and the screen says so.
- **Remove.** The kept program offers **Remove from this computer**; so does
  its entry in Installed apps, and `CretLink-Support.exe uninstall`. Each
  shows what it removes, asks, and asks Windows for administrator rights when
  it has none. The program, its folder, the shortcuts and the entry go. A
  session report that the customer saved stays where it was saved. Removing
  always asks: `uninstall --yes` is refused.
- **No other way.** No command-line option, no environment variable and no
  remote operation keeps the program: only the customer's click does.

`cretlink recover` names a kept program as kept on purpose and never removes
it, and the end report of a session says that it is installed because the
customer chose that. A keep that was interrupted is no kept program:
`recover` names what it left, with the command that removes it
(`CretLink-Support.exe uninstall`), and does not call the computer clean.

## Platforms

- **Technician side** (Give help, the MCP bridge `cretlink mcp`, and the
  experimental terminal client `cretlink-cli shell`): Windows and macOS.
- **Customer side** (Get help): Windows. The customer role exists only in the
  Windows build of `cretlink`.
- **Linux** builds exist for operating the relay server and for development.

Each release has one archive per operating system, for Windows x86_64, macOS
aarch64, and Linux x86_64. It holds the application `cretlink` and its console
build `cretlink-cli`, which runs the same subcommands without a window (on
Windows, use it for `shell`). The relay server is attached separately.
`cretlink-console` and `cretlink-support-agent`, the headless technician and
customer programs in the source tree, are development and test tools and are
not released; non-Windows builds of the support agent offer file operations
and piped commands but no interactive terminal, desktop control,
reboot-resume, or privilege reporting.
The Linux binaries run on glibc 2.35 or newer (Ubuntu 22.04 LTS, Debian 12 and
later); the application `cretlink` also needs the WebKitGTK 4.1 webview
(`libwebkit2gtk-4.1-0` on Debian and Ubuntu).

## Current status

CretLink is under active development. Versions are `0.x` pre-releases;
`1.0.0` is reserved for the first usable attended-LAN release.

- **Works today:** the attended session on the same network (discovery or a
  connection code, the code comparison, full-control consent, stop from either
  side, cleanup), remote commands and jobs, file transfer, the MCP tools for
  an AI agent, session history with an end report, and offline recovery of
  leftover CretLink resources on Windows (`cretlink recover [--remove]`).
- **Checked on real computers:** sessions between two Windows 11 computers,
  driven by Claude Code and by the Code tab of Claude Desktop, last on
  2026-11-10. The run log in [Manual tests](docs/manual-tests.md) names what
  ran and what is still pending: among others macOS as the technician's
  computer, Codex CLI as the second client, and the
  [release sign-off](docs/release-signoff.md) on the official files.
- **Experimental:** the screenshot, input and clipboard tools, and the
  technician's interactive terminal.
- **In progress:** sessions over the internet, through a relay that you host
  yourself and that forwards only end-to-end encrypted traffic. The relay
  server is released and [documented](docs/relay-server.md). A copy of
  CretLink is told which relay to use in a file `cretlink.toml` next to the
  program or in **Settings → Relay** of the installed copy, and can test it
  ([how](docs/relay-server.md#3-tell-a-copy-of-cretlink-which-relay-to-use)).
  For a customer, **Settings → Relay → Make a customer copy…** writes one
  folder to hand over: the support program and a `cretlink.toml` with your
  relay and a key of that customer's own
  ([how](docs/relay-server.md#make-a-customer-copy)).
  With that, the technician chooses **Use relay** on the waiting screen and
  reads out a relay code, the customer types it, and from there the session
  is the one of one network: the same approval code on both screens, the
  customer's permission, and a stop on either side
  ([how](docs/relay-server.md#a-session-through-the-relay)). This is covered
  by automated tests against a relay inside the test; it has not been run
  between real computers on two networks yet
  ([MT-15](docs/manual-tests.md)). Attended reboot-resume has
  its building blocks (a customer-armed, single-use, expiring ticket that
  never restores control without fresh consent) but is not wired into an
  actual reboot.

Official releases are built by the release workflow and ship with SHA-256
checksums, a dependency inventory, and CycloneDX SBOMs.

## Documentation

- The [documentation site](https://yveslaurentcreton.github.io/CretLink/): the
  short introduction. Its source is in [`website/`](website/).
- [MCP client setup](docs/mcp-clients.md)
- [Verifying release artifacts](docs/verifying-artifacts.md)
- [Relay server operation](docs/relay-server.md)
- [Threat model](docs/threat-model.md) and [security policy](SECURITY.md)
- [Manual tests](docs/manual-tests.md) and
  [release sign-off](docs/release-signoff.md)
- [Release policy](docs/releasing.md)

Development setup, build commands, and the dependency policy are documented in
[CONTRIBUTING.md](CONTRIBUTING.md).

The requirements, milestones, stories, and architecture decisions are
maintained in a separate specification repository. Code, commits, and pull
requests refer to them by ID (`REQ-*`, `M###`, `ST-M###-###`, `ADR-####`).
The project follows a strict spec-first workflow:

```text
requirements → milestones → stories → implementation → verification
```

Milestones describe product outcomes. semantic-release independently derives
versions from Conventional Commits. Development uses GitHub Flow: short-lived
story branches, pull requests, required verification, and squash merges into a
releasable `main`.

## License

Copyright © 2026 Yves-Laurent Creton.

Licensed under the [MIT License](LICENSE).
