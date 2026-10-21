# Release policy

CretLink uses semantic-release and Conventional Commits. Milestones remain a
separate planning layer.

## Why milestones and semantic-release are compatible

Milestones answer:

- Which coherent product outcome are we working toward?
- Which requirements and stories define that outcome?
- What evidence is needed before that outcome is Done?

semantic-release answers:

- Does the commit history require a release?
- Is that release a patch, minor, or major?
- What tag, changelog, and release notes should be generated?

One milestone can produce multiple releases, and one release can contain Done
stories from more than one milestone. No milestone is named after a semantic
version.

## Release branch

The initial release branch is `main`. Only protected release automation may
publish from it.

After bootstrap, versions and tags are never chosen manually.

### Bootstrap (ST-M001-005)

- CretLink is in `0.x`. A manually created `v0.0.0` seed tag marked the
  pre-release baseline.
- The repository started from a snapshot commit carrying a manual seed tag,
  `v0.21.1`; earlier history is not part of this repository. The `v0.0.0`
  bootstrap seed and the `v0.21.1` snapshot seed are the only manual tags;
  semantic-release computes every later version.
- From that baseline semantic-release computes versions: `fix`, `perf`, and
  `security` → patch, `feat` → minor. A breaking change (`!` or
  `BREAKING CHANGE:`) produces `1.0.0`; it is reserved for the first usable
  attended-LAN release and must follow the breaking-change rules below.
- The changelog is the generated GitHub release notes; no `CHANGELOG.md` file
  is committed. `docs`, `test`, `refactor`, `build`, `ci`, and `chore` commits
  are hidden from the notes and publish nothing.
- Publication runs only in `.github/workflows/release.yml` on push to `main`
  with the repository-scoped `GITHUB_TOKEN`; pull requests never run release
  automation or receive write-scoped tokens. Release tooling versions are
  pinned in `package.json`.

### Release artifacts (ST-M006-004)

Every release attaches verifiable platform artifacts, built by the `build`
matrix in `release.yml` from the exact commit semantic-release tags and gathered
into the GitHub release by `@semantic-release/github`:

- the five shippable binaries for each target triple (Windows x86_64, macOS
  aarch64, Linux x86_64) — `cretlink-console`, `cretlink-mcp`, and
  `cretlink-shell` (technician computer),
  `cretlink-support-agent` (the only binary placed on the customer computer),
  and `cretlink-relay-server` (operator-hosted public infrastructure, on
  neither endpoint; see [`docs/relay-server.md`](relay-server.md));
- `CHECKSUMS-<target>.txt` — SHA-256 over every binary for that target;
- `dependency-inventory-<target>.txt` — the resolved normal-dependency graph and
  pinned versions from the locked workspace (`cargo tree --locked`);
- `sbom-<binary>-<target>.cdx.json` — one CycloneDX SBOM per shipped binary
  when `cargo-cyclonedx` is available; its absence is a warning, never a silent
  gap, and the dependency inventory is the portable fallback.

`scripts/gen-artifacts.sh` is the single portable producer, runnable both in CI
and locally in any bash (Linux, macOS, or Git Bash on Windows) so a maintainer
reproduces the same artifact metadata from the same commit. Binary filenames
carry the target, not the version: the tag and GitHub release carry the version,
so the artifact producer never chooses one.

### Code signing (ST-M006-005)

Windows artifacts are Authenticode-signed when — and only when — a signing
identity is configured for the release:

- `scripts/sign-windows.ps1` signs every `dist\cretlink-*.exe` with a timestamp,
  verifies each signature, then regenerates `CHECKSUMS-<target>.txt` so the
  published checksums cover the *signed* bytes.
- Signing is gated on `CRETLINK_SIGN_PFX_BASE64` (plus optional
  `CRETLINK_SIGN_PFX_PASSWORD` and `CRETLINK_SIGN_TIMESTAMP_URL`). With no
  identity present the helper is a clean no-op and the binaries stay unsigned, so
  a development build is always distinguishable from an official signed build
  (REQ-N-017).
- The signing secrets live only on the protected push-to-`main` release path in
  the canonical repository; pull-request jobs never run release automation and
  never receive the secrets, so keys cannot leak through an untrusted PR
  (REQ-S-019, REQ-S-020). The decoded PFX is written to a temporary file and
  removed whether signing succeeds or fails.

Users verify the checksums and, on signed Windows builds, the Authenticode
signature by following [`docs/verifying-artifacts.md`](verifying-artifacts.md).

### Security-relevant changes in release notes

Release notes surface the two change classes a user must not miss:

- **Breaking changes** — a `!` marker or `BREAKING CHANGE:` footer renders in the
  generated notes' breaking-changes section automatically.
- **Security-relevant changes** — use a `security:` commit type. It cuts at least
  a patch release and renders under a dedicated **Security** section, so a
  security fix is never hidden among ordinary `fix:` commits.

## GitHub Flow

CretLink uses GitHub Flow:

1. Start a short-lived branch from the latest `main`.
2. Name the branch after the Ready story and intent, for example
   `feat/ST-M003-001-command-broker`.
3. Keep the branch focused on the documented story or an explicitly reviewable
   slice of it.
4. Open a pull request and reference the requirement, milestone, and story.
5. Run all required CI and record acceptance evidence.
6. Resolve review findings without expanding scope silently.
7. Squash-merge using a valid Conventional Commit title.
8. Delete the merged branch.
9. Let semantic-release evaluate the resulting commit on `main`.

`main` must remain releasable. Direct feature commits and manually created
release tags are not part of the normal workflow.

## Conventional Commits

Examples:

```text
feat(agent): add elevated command broker (ST-M003-001)
fix(protocol): reject replayed pairing messages (ST-M002-004)
docs(mcp): clarify target selection (ST-M004-004)
refactor(console): isolate audit writer (ST-M003-006)
feat(protocol)!: replace incompatible job message schema
```

Version impact:

| Commit | Default release effect |
|---|---|
| `fix` | patch |
| `feat` | minor |
| `type!` or `BREAKING CHANGE:` footer | major |
| `perf` | patch |
| `security` | patch |
| `revert` | determined by semantic-release |
| `docs`, `test`, `refactor`, `build`, `ci`, `chore` | no release by default |

Scopes should name a stable component such as `agent`, `console`, `mcp`,
`protocol`, `release`, `security`, or `docs`.

Story identifiers belong at the end of the subject or in the body/footer. They
provide traceability but do not affect version calculation.

## Pull-request and merge policy

The GitHub Flow merge policy is squash merge:

- Pull-request title follows Conventional Commits.
- The title or body references the Ready story.
- Acceptance evidence is present before merge.
- The squash commit is the release input.

If the project later preserves individual commits, every release-relevant
commit must remain valid and the merge strategy must be documented here.

## Breaking changes

A breaking change requires:

- an updated requirement;
- an updated milestone and Ready story;
- protocol compatibility and migration consequences;
- a `!` marker or `BREAKING CHANGE:` footer;
- release notes that explain user action.

Breaking changes are not hidden in milestone completion or inferred from a
large code diff.

## Per-release MCP client validation

Each release must confirm or update the validated MCP clients (REQ-N-001,
ADR-0002 client matrix). The per-release record — not the ADR snapshot — is the
authoritative statement of which clients were validated, because client versions
churn quickly.

Before publishing a release:

1. Run the smoke test in [`docs/mcp-clients.md`](mcp-clients.md) for the
   two independently implemented clients, Claude Code and Codex CLI (Claude
   Desktop optional), and record each client's exact version or build string,
   the console platform, and the date in that file's validation record.
2. Reconcile the recorded versions with the "Validated at" column of the
   ADR-0002 client matrix; update the matrix if a newer version was validated.
3. State the validated client versions in the release notes.

A release must not claim MCP client support it did not validate this cycle.

## Per-release security sign-off (ST-M006-006)

Every release records a security review before publication in
[`docs/release-signoff.md`](release-signoff.md). The sign-off is complete
only when each of these is satisfied with cited evidence or recorded as a named,
accepted residual risk:

- the release security checklist (artifacts and provenance; secure session and
  consent; execution, transfer, and attribution; stop, revocation, and cleanup;
  documentation and disclosure);
- the fresh-environment end-to-end acceptance run
  ([`docs/manual-tests.md` › MT-E2E](manual-tests.md)) on every supported
  platform class, against downloaded, integrity-verified official artifacts;
- the ADR-0002 compatibility matrix executed against those same official
  artifacts, reconciled with the `mcp-clients.md` validation record;
- confirmation that the threat model and `SECURITY.md` reflect the released
  system and that known limitations and residual risks are published.

A release must not be signed off on evidence from local builds where the criteria
call for official artifacts, and an unperformed check is left explicitly pending
rather than assumed to pass.

## Target pipeline

The mature release pipeline publishes:

- semantic version and Git tag — done;
- generated release notes — done;
- platform-specific artifacts for every shipped binary — done;
- checksums — done;
- dependency inventory and SBOM — done;
- signatures, gated on a configured signing identity — signing is implemented,
  but no signing identity is configured yet, so releases are unsigned;
- build provenance attestation — not yet implemented.

Release secrets and signing keys never run in untrusted pull-request jobs.
