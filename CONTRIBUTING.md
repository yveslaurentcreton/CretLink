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
  cli/              The subcommands of `cretlink` and their console build
                    `cretlink-cli`: the stdio MCP bridge (`mcp`), AI-client
                    registration, the experimental terminal client (`shell`),
                    version and uninstall; the MCP conformance harness
  console/          Technician role library (session state, audit, MCP host)
                    and the `cretlink-console` development tool
  relay-server/     Operator-hosted relay server (`cretlink-relay-server`)
  support-agent/    Customer role library (Windows) and the headless
                    `cretlink-support-agent` development tool
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
scripts/            Release artifact generation, Windows signing helpers and
                    the private-data check
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
node scripts/check-private-data.mjs                    # no private data (see "Sample data")
```

Only `cretlink` (from `apps/app`), `cretlink-cli` and the relay server are
released. `cretlink-console` and `cretlink-support-agent` are headless
development and test tools: they drive the same libraries as the application,
and the console's relay options (`--relay`, `--ticket`, `--token`,
`--relay-tls`, `--relay-ca`) stay available to developers until the relay
comes to the application (M007).

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
- CI must be green before squash-merging, and the pull request must be up to
  date with `main`, so that the tree CI tested is the tree that is merged.
  A ruleset on `main` enforces this: it accepts changes only through a pull
  request, merged as a squash commit, and requires two checks, `CI result`
  and `Secret scan and private-data check`.
- CI runs on pull requests and again on `main` after the merge. A newer push
  cancels the run in progress for a pull request. A pull request that changes
  only documentation or the documentation site skips the build and test
  matrix; `CI result` still reports, and the secret scan and the private-data
  check always run.

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
and every pull request runs a secret scan and the private-data check described
below.

## Sample data

The repository and everything published with it hold no private data
(REQ-N-022): no name of a real computer, user account, network, domain, person
or customer, no real IP or hardware address, no path from a real machine, and
nothing copied from a real support session. This holds for code, tests,
fixtures, snapshots, documentation, screenshots, commit messages and pull
request texts. The author's name as copyright holder and publisher is the only
exception. Evidence of a hardware run names the kind of computer and its
operating-system version ("a Windows 11 Pro laptop"), never its name.

Use these sample values:

| For | Sample values |
|---|---|
| Computers | `TECH-LAPTOP` (the technician's), `CUSTOMER-PC`, `ACME-PC-07`, `FRONTDESK-2`, `MOM-LAPTOP` |
| People | `Sam` (the technician) and `Ann` |
| Accounts | `sam`, `ann`, `jdoe`, `tech`, `customer`, as in `C:\Users\ann` and `/home/ann` |
| Addresses | `127.0.0.1`, `10.0.0.42`, `192.168.1.20`, `172.20.0.1` for a virtual adapter, and `192.0.2.1` for a peer on the internet |
| Domains | names under `example.org`, such as `host-01.example.org` and `ann@example.org` |

[`scripts/private-data-samples.json`](scripts/private-data-samples.json) lists
every allowed account, address and e-mail domain with its reason.
`scripts/check-private-data.mjs` checks the tracked files and the commit
messages of every pull request against that list: it fails on any other IPv4
address, user-profile path or e-mail address, and on a computer name as
Windows generates it (`DESKTOP-` or `LAPTOP-` and seven or eight characters).
Run it before pushing; it reads the tracked files, so `git add` new files
first:

```sh
node scripts/check-private-data.mjs --commits origin/main..HEAD
```

A maintainer adds `--terms <file>` with a private list of real names, one per
line, kept outside the repository; a pull request run has no such list. The
check reports the place and the number of a term, never the term itself.

Replace a reported value by a sample value. A new sample value goes into
`scripts/private-data-samples.json`, with its reason, and into the table above
in the same change. The tests of the check run with
`node --test "scripts/*.test.mjs"`.

The check cannot read images. A screenshot shows sample data only: render it
from the fixtures of the application's screens (`apps/app/dev/gallery.html`),
never from a real session.

## Security

Read [`SECURITY.md`](SECURITY.md) for the private vulnerability-reporting
path. Never demonstrate an exploit against systems you do not own, and never
add stealth, consent-bypass, credential-capture, or hidden-persistence
functionality (REQ-S-002, REQ-S-013).
