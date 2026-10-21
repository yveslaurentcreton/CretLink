# MCP client setup and validation

**Status:** Living document (ST-M004-005, REQ-F-014, REQ-F-015, REQ-N-001)

CretLink exposes its remote-computer tools to an MCP-capable AI client through a
thin local bridge, `cretlink-mcp`, that the client spawns over stdio (ADR-0004).
This page is the authoritative, per-client setup and validation record for the
clients in the ADR-0002 compatibility matrix.

The tool surface is deliberately generic and client-neutral: it contains no
client-, provider-, or model-specific behaviour, and an automated conformance
test (`apps/mcp-bridge/tests/stdio_conformance.rs`) proves that any independent
JSON-RPC client discovers the **same exact tool set** with schemas that validate
as JSON Schema. That set is the **fifteen** generic exec, file, and transfer
tools defined by M004 plus the **nine** desktop and clipboard tools added by
ST-M005-004 — **twenty-four** in all. The desktop and clipboard tools are
experimental: M005 is a draft milestone, and a target serves them only when it
advertises the `desktop.v1` capability (currently Windows agents only). The
count is not a free parameter: the harness pins the exact names, so a surface
change fails the test until the tool table below and the assertion move
together. Two
*independently implemented* clients — Claude Code and Codex CLI — are validated
per release to satisfy REQ-F-014's two-client evidence.

## Where things run (and where they never run)

- The **console**, the **`cretlink-mcp` bridge**, and the **MCP client** all run
  on the **technician** computer.
- **AI credentials, provider tokens, and account/login state stay on the
  technician host.** They are never copied to, and never reach, the customer
  (target) computer. The bridge holds no session authority and forwards only
  tool calls to the console over same-user local IPC; it never reads provider
  credentials from its own environment into a tool call.
- The customer computer needs **no** Codex CLI, Claude client, AI account, or API
  key — only the CretLink Support agent.

## Prerequisites

1. Get the bridge binary on the technician machine, either way:

   - **From an official release.** Download `cretlink-mcp-<target>` (with
     `.exe` on Windows) for your platform and verify it against the release's
     checksums per [`verifying-artifacts.md`](verifying-artifacts.md).
   - **From source.**

     ```sh
     cargo build --release -p cretlink-mcp-bridge
     ```

     The executable is `target/release/cretlink-mcp` (`cretlink-mcp.exe` on
     Windows).

   Use the binary's absolute path in the client configuration below — some
   clients do not resolve `PATH` when spawning stdio servers on Windows.

2. Run the **CretLink Console** as the **same OS user** that runs the MCP
   client. The bridge connects to the console over a per-user endpoint (a
   same-user Windows named pipe or a `0700`-directory Unix socket); a different
   user cannot reach it.

3. Connect and consent to a target session in the console. Until an attended,
   consented session exists, every tool call fails closed with an explicit
   `no_consented_session` / `disconnected` error and nothing runs (REQ-S-009).

By default the bridge uses the current user's console endpoint. The optional
`CRETLINK_IPC_ENDPOINT` environment variable selects an explicitly named
same-user endpoint instead; it never changes the transport or crosses a user or
host boundary. Leave it unset for normal single-console use.

## Per-client setup

### Claude Code (>= 2.1)

Register the bridge as a stdio server, either with the CLI:

```sh
claude mcp add cretlink -- /absolute/path/to/cretlink-mcp
```

or by adding it to `.mcp.json` (project scope) / the user config:

```json
{
  "mcpServers": {
    "cretlink": {
      "type": "stdio",
      "command": "C:\\absolute\\path\\to\\cretlink-mcp.exe",
      "args": []
    }
  }
}
```

Then list tools (`/mcp`) and confirm the twenty-four `remote_*` tools appear.

### Codex CLI (>= 0.146)

Add a stdio command entry to `~/.codex/config.toml` (the stable path; the
`codex mcp` management subcommands are still experimental, per ADR-0004):

```toml
[mcp_servers.cretlink]
command = "C:\\absolute\\path\\to\\cretlink-mcp.exe"
args = []
```

Start Codex and confirm it lists the `remote_*` tools.

### Claude Desktop (documented; smoke run optional)

Claude Desktop accepts only stdio server entries in its configuration file. Add
the bridge to `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "cretlink": {
      "command": "C:\\absolute\\path\\to\\cretlink-mcp.exe",
      "args": []
    }
  }
}
```

Claude Desktop publishes no semantic version; record the build string reported by
the installed app when validating.

## Smoke test (per client)

Run this once per client after configuration. It is the manual, hardware part
of the release validation.

1. Start the CretLink Console; connect the CretLink Support agent on the target;
   compare the session code and grant full control.
2. Start the MCP client so it spawns `cretlink-mcp`.
3. **List tools.** Confirm exactly the twenty-four `remote_*` tools are present.
4. **`remote_system_info`.** Confirm it returns the *target's* identity
   (hostname, OS, arch, elevation) and that the result embeds the routed
   `session_id` and target — not the technician machine.
5. **Representative workflow.** Run a portable command (for example
   `remote_exec` `echo`), then a file round-trip (`remote_upload` then
   `remote_download`) and confirm the SHA-256 digests match in both directions.
6. **Fail-closed check.** Stop the session (or revoke on the customer side) and
   confirm the next tool call fails closed and nothing runs.
7. **Credential check.** Confirm no provider token, API key, or login state
   appears in any CretLink-managed path on the target (agent staging, audit, or
   memory). None should — the bridge never sends them.
8. Record the client name, version/build, platform, and date in the validation
   record below and in the release notes.

## Capabilities: the twenty-four generic tools

The first fifteen rows are the generic exec, file, and transfer tools defined by
M004. The nine desktop and clipboard tools below them were added by ST-M005-004
and are asserted by the same conformance harness. They are experimental (M005 is
a draft milestone) and fail closed with an explicit error unless the target
advertises `desktop.v1`.

| Group | Tool | Purpose |
| --- | --- | --- |
| System | `remote_system_info` | Target identity and reboot-pending state |
| Execution | `remote_exec` | Run a shell command; collect stdout/stderr, exit, outcome |
| Execution | `remote_write_stdin` | Feed base64 bytes to a running job's stdin |
| Execution | `remote_cancel` | Request cancellation of a running job |
| Jobs | `remote_list_jobs` | List tracked jobs and running state |
| Files | `remote_stat` | Stat a path (lstat; never returns content) |
| Files | `remote_list_dir` | List one page of a directory |
| Files | `remote_make_dir` | Create a directory |
| Files | `remote_move` | Move/rename within a volume |
| Files | `remote_delete` | Delete a file or reparse point |
| Files | `remote_remove_dir` | Remove an empty directory |
| Transfer | `remote_upload` | Upload a local file into target staging, verified |
| Transfer | `remote_download` | Download a target file, verified by SHA-256 |
| Transfer | `remote_list_staged` | List tracked staged artifacts |
| Transfer | `remote_cleanup` | Remove staged artifacts by cleanup trigger |
| Desktop | `remote_capture_desktop` | Capture a screenshot of the target desktop |
| Desktop | `remote_desktop_pointer` | Move/click/drag the target pointer |
| Desktop | `remote_desktop_scroll` | Scroll the target pointer wheel |
| Desktop | `remote_desktop_key` | Press or release a single key |
| Desktop | `remote_desktop_type` | Type a Unicode string on the target |
| Desktop | `remote_desktop_hotkey` | Press a chord (modifiers + key) |
| Desktop | `remote_desktop_wait` | Wait a bounded interval between actions |
| Clipboard | `remote_clipboard_get` | Read the target clipboard text |
| Clipboard | `remote_clipboard_set` | Set the target clipboard text |

## Client-specific limitations

These are recorded here and stay out of the remote protocol, which carries no
client identifiers (the IPC wire is versioned and client-neutral).

- **Structured vs. text results.** Every tool result is returned both as a
  pretty-printed JSON text block and as MCP `structuredContent` (no
  `outputSchema` is advertised). Clients that read only the text block work; a
  client that ignores the text and requires an `outputSchema` before reading
  `structuredContent` would see less. Prefer clients that read either.
- **Codex CLI.** Use the `config.toml` stdio entry; the `codex mcp` management
  subcommands are experimental.
- **Claude Desktop.** stdio-only configuration; no semantic version (record the
  build string). Optional MCPB extension packaging is not provided.
- **Windows spawn.** Some clients do not resolve `PATH` for stdio servers on
  Windows; always configure the absolute path to `cretlink-mcp.exe`.
- **A single linear caller cannot drive mid-job `remote_write_stdin` /
  `remote_cancel`.** `remote_exec` collects a job to completion before it
  returns, and an agent that awaits each tool result before issuing the next
  (observed with Claude Code 2.1.228: tool calls sent together are dispatched
  sequentially) therefore cannot feed stdin to, or cancel, a job it just
  launched — the follow-up call reaches the agent after the job has ended and
  returns a fail-closed `unknown_job`. Bound long jobs with `timeout_ms`
  instead; the timeout arm reaps the whole process tree exactly as
  `remote_cancel` would (verified: a timed-out `ping` left zero survivors).
  Drive live stdin or cancellation from a concurrent controller — the console,
  or a client that issues concurrent JSON-RPC. This is a caller-interaction
  property, not a wire-protocol limitation, and the tools still fail closed
  honestly when targeted at a job that is no longer running.
- **Piped `remote_exec` is non-interactive.** The piped execution backend
  launches the shell non-interactively, so an interactive prompt such as
  PowerShell `Read-Host` errors immediately (`NonInteractive mode`) rather than
  blocking for input. Pipe input only to programs that read stdin
  non-interactively; prompt-driven interactive sessions belong to the terminal
  backend (ST-M003-009), not the piped `remote_exec` surface.

## Validation record

Filled in per release from the smoke test above. Each row must name the client
and its exact version/build so it counts as evidence for that client (a
Claude-driven run cannot itself serve as the independent second-client evidence
for REQ-F-014; Codex CLI is the independent client).

| Client | Version / build | Console platform | Target OS | tools/list | `remote_system_info` | Representative workflow | Date | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Claude Code | 2.1.228 | Windows 11 Pro (10.0.26200) | Windows 11 Home 22H2 (22621.900), elevated, non-simulated | yes — all 15 `remote_*` exercised live (pre-ST-M005-004 binary; 15-tool surface) | yes — target was a Windows 11 Home test laptop; result carried the routed `session_id`/target | `remote_exec` + `remote_upload`→`remote_list_staged`→`remote_download` with SHA-256 matched in both directions (local `cmp` identical); full `make_dir`/`move`/`list_dir`/`stat`/`delete`/`remove_dir` chain; `remote_cleanup` honest on a dangling record; `timeout_ms` kill reaped the whole tree (0 survivors) | 2026-08-12 | Pass; `remote_write_stdin`/`remote_cancel` mid-job use needs a concurrent controller (see limitations) |
| Codex CLI | _pending_ | | | | | | | |
| Claude Desktop | _optional_ | | | | | | | |

> This row predates the desktop and clipboard tools (ST-M005-004) and observed
> 15 tools; current builds expose 24. Codex CLI validation is pending.

## Automated evidence

The client-neutral conformance harness
(`apps/mcp-bridge/tests/stdio_conformance.rs`) runs on every CI platform and
spawns the shipped binary with a hand-rolled JSON-RPC client (no rmcp client
types). It asserts: `initialize` across two protocol versions; `tools/list`
equals exactly the twenty-four tools above; each input schema compiles as JSON
Schema and carries the shared `limits` bounds; stdout emits nothing but JSON-RPC
frames (the ADR-0004 stdout-purity consequence); a tool call without a console
fails closed; and the full `client -> binary -> IPC -> console -> broker` path
returns the routed target identity and runs a portable command.
