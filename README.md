# CretLink

CretLink is a personal open-source project for secure, attended AI-assisted
remote support.

It lets an MCP-capable AI agent running on a technician's own Windows or macOS
computer operate a temporary elevated agent on a customer's Windows computer.
The customer computer does not need Codex CLI, Claude CLI, an AI account, or an
AI API key.

## Intended workflow

CretLink is one application, `cretlink` (`cretlink.exe` on Windows), with two
roles: **Give help** for the technician and, on Windows, **Get help** for the
customer.

1. On the technician computer, unpack the release archive, start `cretlink`
   and choose **Give help** (run once or install, see below).
2. Connect your AI clients once with `cretlink mcp install`, or from the AI
   assistants screen.
3. On the customer's Windows computer, start the same `cretlink.exe`, for
   example from removable media, and choose **Get help**. A copy renamed to
   `CretLink-Support.exe` opens Get help straight away.
4. Approve UAC, compare the session code, and grant full control for the
   attended session.
5. Use Codex, Claude, or another MCP client; it reaches the session through
   the local bridge, `cretlink mcp`.
6. Let the AI inspect the target, run arbitrary PowerShell or Command Prompt
   work, control jobs, and transfer files.
7. Stop the session from either endpoint and remove temporary access.

The primary path is a direct authenticated and encrypted LAN connection.
On Windows, offline recovery of leftover CretLink resources is implemented
(`cretlink recover [--remove]`; Get help also removes what an earlier session
left before it connects, and says so when it cannot), as
are the building blocks of attended reboot-resume: a customer-armed,
single-use, expiring continuity ticket that never restores control without
fresh consent. Wiring reboot-resume into an actual reboot (automatic restart of
the agent and the reconnect) is not finished yet. An optional internet
relay, which forwards only end-to-end encrypted traffic, is in progress.
Interactive desktop control follows once the command, file, consent,
revocation, and cleanup foundations are reliable.

## Run once or install

The technician side is one program, `cretlink`, that runs from wherever it was
unpacked. Its start screen offers, under **Give help**:

- **Run once without installing.** The copy writes nothing but its audit trail
  (in the per-user data folder). AI clients you connect from it point at this
  copy and break when it is moved or deleted; the AI assistants screen says so.
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
installed files, the shortcut or desktop entry, the uninstall entry and the AI
client registrations the install made, but only while they still point at the
installed copy. The audit trail stays. On macOS and Linux an installed copy
opens Give help directly; a copy that is not installed shows the start screen
so it can still be installed. A `CretLink-Support*` copy, for customers, never
installs or uninstalls; the one command it accepts is `recover [--remove]`.

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

CretLink is under active development and not yet ready for production support
work. The attended same-LAN session — discovery or manual pairing, the
approval-code comparison, full-control consent, revocation from either side,
and cleanup — is implemented (M001, M002). Remote execution and file transfer
(M003) and MCP control (M004) are implemented, with hardware validation
outstanding. Resilience and release hardening (M006) and the optional internet
relay (M007) are in progress. Desktop control (M005) and the technician
terminal (M008) are draft milestones with experimental code only.

Versions are `0.x` pre-releases; `1.0.0` is reserved for the first usable
attended-LAN release. Official releases are built by the release workflow and
ship with SHA-256 checksums, a dependency inventory, and CycloneDX SBOMs. They
are not code-signed yet; see
[Verifying release artifacts](docs/verifying-artifacts.md).

Start with:

- [Release policy](docs/releasing.md)
- [Verifying release artifacts](docs/verifying-artifacts.md)
- [MCP client setup](docs/mcp-clients.md)
- [Relay server operation](docs/relay-server.md)
- [Threat model](docs/threat-model.md)
- [Manual tests](docs/manual-tests.md)
- [Release sign-off](docs/release-signoff.md)
- [Security policy](SECURITY.md)

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
