# Contributing to CretLink

CretLink is a personal MIT-licensed project. Contributions follow the
spec-first workflow described in [`AGENTS.md`](AGENTS.md): requirements →
milestones → stories → implementation → verification. The specification is
maintained in a separate repository; implementation changes must reference at
least one Ready story (`ST-M###-###`). Without access to it, describe the
intended behaviour in the issue or pull request first.

## Repository layout

```text
apps/
  app/              The CretLink application `cretlink`: Give help and Get help
                    (Tauri v2, detached package)
  cli/              The console build `cretlink-cli` of the subcommands (MCP
                    bridge, AI-client registration, terminal, version)
  console/          CretLink Console core and CLI (session state, audit, MCP host)
  mcp-bridge/       Builds the `cretlink-mcp` stdio MCP bridge binary
  relay-server/     Operator-hosted relay server (`cretlink-relay-server`)
  shell-cli/        Experimental technician terminal client (`cretlink-shell`)
  support-agent/    CretLink Support agent (customer side, Windows)
crates/
  discovery/        LAN mDNS discovery and the manual connection-code fallback
  exec/             Execution broker, interactive terminal, files, desktop backend
  mcp/              MCP library: the `remote_*` tool surface used by the bridge
  protocol/         Versioned protocol schemas
  relay/            Relay core: opaque forwarding, rendezvous, admission controls
  session/          Attended session runtime: consent, revocation, cleanup
  shared/           Cross-cutting utilities (ids, redaction, audit primitives)
  simulator/        Non-privileged simulated target for tests and CI
  transport/        Pairing, secure channel, framing
  wss/              TLS/WebSocket transport envelope for the relay
docs/               Threat model, release policy, artifact verification, relay
                    operation, MCP clients, manual tests, release sign-off
scripts/            Release artifact generation and Windows signing helpers
```

## Prerequisites

- A stable Rust toolchain via [rustup](https://rustup.rs) (`rustfmt` and
  `clippy` components install automatically through `rust-toolchain.toml`).
- A C compiler. The relay transport's TLS uses rustls with the aws-lc-rs crypto
  provider (ADR-0017), which compiles AWS-LC's C and assembly sources; the
  session runtime depends on it, so every workspace build needs one (MSVC Build
  Tools on Windows, Xcode Command Line Tools on macOS, `gcc` or `clang` on
  Linux).
- Windows, macOS, or Linux. The release workflow builds on all three; the
  customer-side agent targets Windows, and non-Windows agent builds are for
  development and testing.

## Commands

```sh
cargo fmt --all                                        # format
cargo fmt --all --check                                # verify formatting
cargo clippy --workspace --all-targets -- -D warnings  # lint
cargo test --workspace                                 # run all tests
cargo build --workspace                                # build everything
```

All routine tests run without administrator rights against the simulated
target (`crates/simulator`). Privileged Windows tests are opt-in and never run
automatically for untrusted contributions.

### The application (Tauri)

The application (`apps/app`) is a **detached** Cargo package with its own
`Cargo.lock` (ADR-0011): Tauri's dependency tree and the Linux webview system
libraries are deliberately kept out of the workspace commands above. Its
cross-platform-verified state models live in `apps/console` and
`apps/support-agent` and are covered by the workspace tests. Building it needs
Node.js 20+ and npm in addition to the Rust toolchain, and on Linux the webview
libraries (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `librsvg2-dev`):

```sh
cd apps/app
npm install                 # first time only
npm run build               # type-check + bundle the frontend (Vite → dist/)
npm run tauri dev           # run it in development
```

See [`apps/app/README.md`](apps/app/README.md) for the roles, the release build
and the remaining manual accessibility verification.

## Branches, commits, and releases

Follow [`docs/releasing.md`](docs/releasing.md):

- GitHub Flow: short-lived branch from the latest `main`, pull request,
  verification, squash-merge. Never push feature work directly to `main`.
- Branch names carry the story: `feat/ST-M004-004-target-binding`.
- [Conventional Commits](https://www.conventionalcommits.org): `fix` → patch,
  `feat` → minor, `!`/`BREAKING CHANGE:` → major. `docs`, `test`, `build`,
  `ci`, `refactor`, and `chore` do not release.
- Include the story ID in the subject, body, or footer.
- semantic-release owns versions, tags, and changelogs; never edit them by
  hand. The only manual tags are the `v0.0.0` bootstrap seed and the
  `v0.21.1` snapshot seed (see `docs/releasing.md`).
- CI must be green before squash-merging. Branch protection with required
  checks is enabled once the repository becomes public; on the current
  private free-plan repository this rule is procedural.

## Dependency and license policy

CretLink is MIT-licensed (REQ-N-016). Dependencies must carry MIT-compatible
licenses; the allowlist and policy checks live in [`deny.toml`](deny.toml) and
run in CI via `cargo deny`:

- Allowed: `MIT`, `MIT-0`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`,
  `BSD-2-Clause`, `BSD-3-Clause`, `ISC`, `Zlib`, `Unicode-3.0`.
- Any other license, including `MPL-2.0`, needs an explicit, commented
  exception in `deny.toml` recording the review outcome. GPL/AGPL/LGPL and other
  strong-copyleft licenses and source-unavailable licenses are not accepted
  without an explicit maintainer decision.
- Security-relevant dependencies (crypto, transport, elevation) prefer widely
  used, actively maintained, audited crates (REQ-S-005). Do not add a crypto
  crate without an accepted ADR.

Retain third-party license notices. Never commit credentials, tokens, signing
keys, or customer data (REQ-N-017); `.gitignore` excludes common local state,
and CI runs a secret scan.

## Security

Read [`SECURITY.md`](SECURITY.md) for the private vulnerability-reporting
path. Never demonstrate an exploit against systems you do not own, and never
add stealth, consent-bypass, credential-capture, or hidden-persistence
functionality (REQ-S-002, REQ-S-013).
