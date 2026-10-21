# CretLink

CretLink is a personal open-source project for secure, attended AI-assisted
remote support.

It lets an MCP-capable AI agent running on a technician's own Windows or macOS
computer operate a temporary elevated agent on a customer's Windows computer.
The customer computer does not need Codex CLI, Claude CLI, an AI account, or an
AI API key.

## Intended workflow

1. Start CretLink Console on the technician computer.
2. Launch CretLink Support from removable media on the customer computer.
3. Approve UAC, compare the session code, and grant full control for the
   attended session.
4. Use Codex, Claude, or another MCP client through the local CretLink bridge.
5. Let the AI inspect the target, run arbitrary PowerShell or Command Prompt
   work, control jobs, and transfer files.
6. Stop the session from either endpoint and remove temporary access.

The primary path is a direct authenticated and encrypted LAN connection.
On Windows, offline recovery of leftover CretLink resources is implemented, as
are the building blocks of attended reboot-resume: a customer-armed,
single-use, expiring continuity ticket that never restores control without
fresh consent. Wiring reboot-resume into an actual reboot (automatic restart of
the agent and the reconnect) is not finished yet. An optional internet
relay, which forwards only end-to-end encrypted traffic, is in progress.
Interactive desktop control follows once the command, file, consent,
revocation, and cleanup foundations are reliable.

## Platforms

- **Technician side** (console, MCP bridge, and the experimental
  `cretlink-shell` terminal client): Windows and macOS.
- **Customer side** (support agent): Windows. Non-Windows builds of the support
  agent are for development and testing only; they offer file operations and
  piped commands but no interactive terminal, desktop control, reboot-resume,
  or privilege reporting.
- **Linux** builds exist for operating the relay server and for development.

Release binaries are built for Windows x86_64, macOS aarch64, and Linux x86_64.

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
