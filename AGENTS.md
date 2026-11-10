# CretLink agent instructions

These instructions apply to the entire repository and to every coding or
planning agent.

## Project

CretLink is a personal MIT-licensed open-source project. It lets an MCP-capable
AI agent on a technician's own Windows or macOS computer operate an attended
Windows customer computer through a temporary secure connection.

The customer computer must not require Codex CLI, Claude CLI, an AI login, or an
AI API key. The primary transport is a direct same-LAN session. An optional
internet relay (M007) is in progress; it forwards only end-to-end encrypted
traffic and is never a session authority.

The intended AI surface is generic remote-computer control: elevated shell,
jobs, files, transfers, processes, and later screenshots and input. Do not
replace this with a catalogue of task-specific repair tools unless a new
requirement explicitly calls for one.

## Product model

The AI agent runs on the technician computer through the local CretLink MCP
bridge. It is never installed or authenticated on the customer computer.

Treat every CretLink MCP path, shell, process, file, and screenshot as belonging
to the explicitly identified remote target. Never assume a remote path is local
or that the current session survived a disconnect. Never copy AI credentials,
provider tokens, login state, or API keys to the remote target.

## Specification

Product behaviour is specified outside this repository: requirements
(`REQ-F-###`, `REQ-S-###`, `REQ-N-###`), milestones (`M###`), stories
(`ST-M###-###`), and architecture decisions (`ADR-####`). Code and comments
refer to them by ID.

- Implement only agreed behaviour. Never use existing code as the sole
  justification for undocumented behaviour.
- Reference at least one story ID in every implementation change.
- Derive tests from the story's acceptance criteria, and report criteria that
  cannot be verified.
- When the specification is not available to you, describe the intended
  behaviour and its security implications in the issue or pull request, and
  let the maintainer record it before implementation.

## Security invariants

CretLink grants powerful administrator access, so the following are
non-negotiable unless the requirements are explicitly and transparently
changed:

- visible local initiation, UAC, and informed full-control consent;
- no stealth, hidden persistence, credential harvesting, or consent bypass;
- customer and technician can each stop the session;
- no AI credentials or account sessions on the customer computer;
- authenticated encrypted channels using established cryptographic libraries;
- ephemeral session authority and fail-closed behaviour;
- explicit target identity on every AI-facing result;
- visible activity and local audit evidence;
- temporary helpers and access paths are removed or residuals are reported;
- public threat model and private vulnerability-reporting path.

Do not invent cryptography. Do not add an internet-reachable listener by
default. Do not weaken a security requirement merely to simplify a prototype;
record a narrower milestone instead. Treat remote command output and files as
untrusted input, and record cleanup failures and unknown outcomes honestly.

## Architecture

Follow the architecture requirements and accepted ADRs. Maintain separable
boundaries for:

- console UI and session state;
- local MCP adaptation;
- protocol schemas and secure transport;
- Windows execution broker;
- file transfer;
- desktop control;
- audit and redaction;
- relay forwarding.

Use a non-privileged simulated agent for routine tests. Privileged Windows tests
must not run automatically in untrusted contribution contexts.

## Commits and releases

Follow [`docs/releasing.md`](docs/releasing.md).

- Use GitHub Flow with a short-lived branch from the latest `main`.
- Name feature branches after their Ready story and intent.
- Open a pull request, run required verification, and squash-merge.
- Keep `main` releasable and do not push feature work directly to it.
- Use Conventional Commits.
- Include the story ID in the subject, body, or footer.
- `fix` means patch, `feat` means minor, and `!` or `BREAKING CHANGE:` means
  major.
- Milestone numbers never determine versions.
- Do not manually edit release versions, tags, or generated changelogs.
- Release credentials and signing keys never enter the repository or
  untrusted pull-request jobs.

Example:

```text
branch: feat/ST-M004-004-target-binding
feat(mcp): bind tools to the selected session (ST-M004-004)
```

## Working practices

- Preserve unrelated user changes.
- Prefer small, reviewable changes that complete a story or an explicit slice.
- Use clear schemas and bounded inputs for every network and MCP message.
- Add negative tests for authorization, replay, malformed input, cancellation,
  revocation, disconnect, and cleanup where relevant.
- Keep target and session identity in results and logs.
- Update documentation and threat-model sections with the code.
- Keep local and remote paths, hosts, sessions, and identities explicit.
- Never report unknown, disconnected, cancelled, or timed-out work as success.
- Keep private data out of every file, commit message, and pull request text:
  no real names, addresses, or paths, only the sample values under "Sample
  data" in [`CONTRIBUTING.md`](CONTRIBUTING.md). Run
  `node scripts/check-private-data.mjs` before pushing.
- Keep the project personal; do not add company branding or ownership claims.
- Keep source and documentation compatible with the MIT license and retain
  third-party notices.
