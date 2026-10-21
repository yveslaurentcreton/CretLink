# Release security review and sign-off

**Status:** Living record (ST-M006-006; REQ-S-001 through REQ-S-022, REQ-N-001
through REQ-N-020)

This is the per-release evidence record that an official CretLink release
completes the attended-LAN workflow on real hardware and documents its remaining
risks honestly. It is the record the M006 exit criterion and REQ-S-021's security
acceptance evidence point at: the threat model states the design commitments, and
this page states what was actually verified for a given release, on which
platform classes, against which artifacts.

A release is signed off only when every checklist item below is either satisfied
with cited evidence or recorded as a named, accepted residual risk. Nothing here
may claim a check that was not performed; an unperformed check is left explicitly
pending (following the honesty rule in
[`threat-model.md`](threat-model.md#residual-risks-and-explicit-non-goals)).

## What this record covers

- **End-to-end acceptance** of the full attended workflow — launch, UAC,
  discovery and the manual fallback, pairing, code comparison, consent, MCP
  control, execution, transfer, stop, and cleanup — run start-to-finish from a
  fresh environment on each supported platform class. The procedure is
  [`manual-tests.md` › MT-E2E](manual-tests.md#mt-e2e--full-attended-lan-acceptance-walk-through-st-m006-006).
- **Compatibility matrix executed against official artifacts** — the ADR-0002
  platform/client matrix, run with the downloaded, integrity-verified release
  binaries rather than local `cargo build` output, and recorded in the
  [`mcp-clients.md` validation record](mcp-clients.md#validation-record).
- **Artifact integrity** — checksums (and Authenticode signature when a signing
  identity is available) verified per
  [`verifying-artifacts.md`](verifying-artifacts.md).
- **Threat model and SECURITY.md currency** — both reflect the released system,
  and the known limitations and residual risks are published.

## Release security checklist

Run this checklist for every release before sign-off. Each item names the
evidence that satisfies it.

### Artifacts and provenance

- [ ] Official artifacts were built by `release.yml` from the tagged commit, not
      hand-built — the five binaries, `CHECKSUMS-<target>.txt`,
      `dependency-inventory-<target>.txt`, and the per-binary SBOMs
      (`sbom-<binary>-<target>.cdx.json`) when available
      (REQ-N-016, REQ-N-018; ST-M006-004).
- [ ] Checksums verify on every platform class against the downloaded binaries
      (REQ-S-019; [`verifying-artifacts.md`](verifying-artifacts.md)).
- [ ] Windows Authenticode signature verifies as `Valid` with the expected
      trusted, timestamped signer **when** a signing identity was configured;
      otherwise the release is recorded as unsigned development-grade and the
      checksums stand as the baseline (REQ-N-017, REQ-S-019; ST-M006-005).
- [ ] Release automation ran only on the protected push-to-`main` path in the
      canonical repository; no signing secret was exposed to a pull-request job
      (REQ-S-019, REQ-S-020).

### Secure session and consent

- [ ] MT-E2E steps 3–5 pass: discovery carries no authority, the manual fallback
      uses the identical pairing path, the SAC matches on both endpoints, and
      code comparison and consent are distinct steps (REQ-S-001, REQ-S-006,
      REQ-F-004, REQ-F-005, REQ-F-006).
- [ ] Consent discloses the real authority granted and the AI-service data flow
      before `Active` (REQ-S-003, REQ-S-016, REQ-N-013).
- [ ] Fail-closed behaviour confirmed: an authentication, integrity, version, or
      authorisation failure denies control with a visible error, and
      reconnection alone never restores control (REQ-S-008, REQ-S-009).

### Execution, transfer, attribution

- [ ] `remote_system_info` reports the target's true identity and elevation, and
      every result carries the target and routed `session_id` (REQ-S-011,
      REQ-F-016, REQ-N-001).
- [ ] Transfers verify length and SHA-256 in both directions (REQ-N-007).
- [ ] Outcomes distinguish completed / failed / cancelled / timed-out / unknown;
      nothing interrupted is reported as success (REQ-N-008).
- [ ] Actions are visible in the activity view and audited with session identity,
      actor/origin, and the routed `call_id` (REQ-F-017, REQ-S-017, REQ-S-023).

### Stop, revocation, cleanup

- [ ] Stop and revoke work from either endpoint without the other's cooperation,
      reject new tool calls, and invalidate session secrets (REQ-S-004,
      REQ-F-018, REQ-S-007).
- [ ] Cleanup removes temporary services, scheduled tasks, secrets, and staged
      files in the normal path and in representative failure paths; a
      hard-kill of the agent leaves no orphaned elevated job tree (REQ-F-019,
      REQ-S-012, REQ-S-014; ST-M006-007). Recovery tooling reports residual
      access rather than hiding it (ST-M006-003, `recover`).
- [ ] No provider token, API key, or login state reached any CretLink-managed
      path on the target (REQ-S-015, REQ-F-015).

### Documentation and disclosure

- [ ] Threat model reflects the released trust boundaries; every milestone that
      changed a boundary updated its section (REQ-S-021).
- [ ] SECURITY.md reflects the released system and the coordinated-disclosure
      path is current (REQ-S-022).
- [ ] Known limitations and residual risks are published and current
      ([`threat-model.md` › Residual risks](threat-model.md#residual-risks-and-explicit-non-goals)).
- [ ] Release notes state the validated MCP client versions for this cycle
      (REQ-N-001; [`releasing.md`](releasing.md#per-release-mcp-client-validation)).

## End-to-end acceptance record

One row per supported platform class per release, filled from MT-E2E on official
artifacts from a fresh environment. An empty cell means the run has not been
performed — never infer a pass.

| Release | Platform class | Artifact provenance (verified) | MT-E2E result | Fresh env? | Date | Operator | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| _pending first signed-off release_ | Windows x86_64 | | | | | | |
| _pending first signed-off release_ | (other supported class) | | | | | | |

## Compatibility-matrix execution record

The ADR-0002 matrix executed **against the official artifacts** for this release.
The authoritative per-client rows live in the
[`mcp-clients.md` validation record](mcp-clients.md#validation-record); this table
records that the matrix was run against downloaded, integrity-verified binaries
(not a local build) and points at that record.

| Release | Ran against official artifacts? | Clients validated (two independent) | Console/target platforms | Date | Notes |
| --- | --- | --- | --- | --- | --- |
| _pending first signed-off release_ | | Claude Code + Codex CLI (REQ-F-014) | | | prior matrix runs used local builds; execution against official artifacts is required for sign-off |

## Sign-off

| Release (tag) | Checklist complete | E2E all classes | Matrix on official artifacts | Threat model + SECURITY.md current | Signed off by | Date |
| --- | --- | --- | --- | --- | --- | --- |
| _pending first signed-off release_ | | | | | | |

## Current status

The checklist, MT-E2E procedure, sign-off tables, and residual-risk register are
in place and ready to be executed. Official artifacts are published by the
release workflow with SHA-256 checksums, a dependency inventory, and CycloneDX
SBOMs; they are not code-signed yet, because no signing identity is configured.
The rows above are empty because no release has yet completed MT-E2E or the
compatibility matrix against its official artifacts. Signing off a release
requires, on real hardware:

- running MT-E2E from a fresh environment on each supported platform class
  against the downloaded, integrity-verified official artifacts, and recording
  each row above;
- executing the ADR-0002 compatibility matrix against those same official
  artifacts (two independent clients), reconciling the `mcp-clients.md`
  validation record, and signing off.

Until then, this release-security review is **prepared and honest, not
completed**: every design mitigation is specified and traceable, and the evidence
that official artifacts complete the workflow is pending. This matches the
implementation-status honesty the threat model already states.
