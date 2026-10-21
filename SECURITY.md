# Security policy

CretLink is a personal, MIT-licensed open-source project that intentionally
enables administrator-level remote execution during attended support sessions.
Security reports are taken seriously and handled with care, within the limits
of a single-maintainer project.

The project's threat model — including what CretLink defends against, what it
deliberately does not, and the current implementation status of each
mitigation — is published in
[`docs/threat-model.md`](docs/threat-model.md).

## Supported versions

CretLink is pre-1.0. Official releases are `0.x` versions published by the
release workflow, each with SHA-256 checksums, a dependency inventory, and
CycloneDX SBOMs.

- Only the **latest official release** (and the current `main` branch) receive
  security fixes. Older releases are not patched; please upgrade.

Milestone identifiers never correspond to version numbers.

Before running a downloaded artifact, confirm it is genuine by following
[`docs/verifying-artifacts.md`](docs/verifying-artifacts.md). Releases are
currently unsigned, so the SHA-256 checksums are the assurance; once a signing
identity is configured, Windows builds also carry an Authenticode signature to
verify.

Each release records its security review — the checklist, the fresh-environment
end-to-end acceptance run, the compatibility-matrix execution against official
artifacts, and the sign-off — in
[`docs/release-signoff.md`](docs/release-signoff.md). The published threat model
and its consolidated residual risks live in
[`docs/threat-model.md`](docs/threat-model.md).

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** using GitHub's private
vulnerability reporting (Security Advisories) for this repository:

- Go to the **Security** tab of
  `github.com/yveslaurentcreton/CretLink` and choose **Report a
  vulnerability** (or use
  `https://github.com/yveslaurentcreton/CretLink/security/advisories/new`).

(GitHub private vulnerability reporting becomes available once the repository
is public; the maintainer enables and verifies this channel at public
release. Until then, use the fallback below.)

**Please do not open a public issue, discussion, or pull request containing
exploitable details.** CretLink's own requirements commit the project to a
private reporting path precisely so that reporters are never forced to publish
exploitable details publicly (REQ-S-022). If you are unsure whether something
is sensitive, err on the side of the private channel.

If GitHub private reporting is unavailable to you for some reason, open a
public issue that says only that you have a security report and how you can be
contacted — without any technical detail — and a private channel will be
arranged.

### What to include

The more of the following you can provide, the faster triage will be:

- the component or boundary affected (for example: pairing, discovery, secure
  channel, execution broker, MCP bridge, file transfer, cleanup, release or
  build pipeline);
- the commit hash or release version you examined;
- a description of the issue and its security impact — which guarantee from
  the threat model is broken (a REQ-S-### reference is welcome but not
  required);
- steps to reproduce, a proof of concept, or the reasoning that leads you to
  believe the issue is real;
- any suggested fix or mitigation, if you have one;
- how you would like to be credited, or whether you prefer to remain
  anonymous.

Reports about the design itself (a requirement that seems insufficient, a
residual risk that seems understated) are also welcome through the same
channel or, if they contain nothing exploitable, as ordinary issues.

## What to expect

This is a personal project maintained on a best-effort basis, not a company
with a security team. With that understood:

- **First response:** target within **14 days** of your report, usually
  sooner.
- **Triage:** an assessment of whether the report is accepted, its severity,
  and the intended fix path, communicated in the advisory thread.
- **Fixing:** accepted issues are fixed as quickly as the maintainer's time
  allows, prioritised by severity. Because the project is pre-release, some
  reports may be resolved by changing the specification and design before the
  affected code ever ships.
- **Disclosure:** coordinated. You will be kept informed, and public
  disclosure (including a published advisory and credit, if you wish) happens
  once a fix or mitigation is available, or at an agreed date. If you need a
  disclosure deadline, propose one in your report — reasonable deadlines will
  be respected, and silence will never be used to sit on a report
  indefinitely.

## Safe harbour for good-faith research

Security research on CretLink conducted in good faith is welcome. If you:

- make a genuine effort to avoid harming others — testing only against
  systems, devices, and accounts you own or are explicitly authorised to use,
  and never against a third party's customer sessions or data;
- avoid privacy violations, data destruction, and service degradation beyond
  what is strictly necessary to demonstrate the issue;
- report through the private channel above and give the project a reasonable
  opportunity to fix the issue before public disclosure; and
- do not use a finding for extortion or to access data beyond what is needed
  as proof;

then the maintainer considers your research **authorised and welcome**, will
not pursue or support legal action against you for it, and will say so to
third parties if asked. This commitment is made by the maintainer of this
repository and cannot bind other parties (for example, operators of
third-party relay deployments or owners of machines you did not have
permission to test).

Note that CretLink's threat model explicitly excludes some findings from
scope — for example, denial of service on a hostile LAN, a fully compromised
technician computer, or abuse of authority the customer genuinely granted.
These are documented as accepted residual risks in
[`docs/threat-model.md`](docs/threat-model.md). Reports in those areas are
still read, but they will usually be answered by pointing at the documented
trade-off rather than a fix.

## Scope reminders for reporters

Particularly valuable reports include anything that would:

- grant session or command authority without visible local initiation,
  code comparison, and consent (REQ-S-001, REQ-F-005, REQ-F-006);
- allow stealth, hidden persistence, or survival of authority past stop,
  revocation, or cleanup (REQ-S-002, REQ-S-007, REQ-S-012, REQ-F-019);
- expose session plaintext or secrets to the network (REQ-S-005..008);
- place AI or provider credentials on the customer computer (REQ-S-015);
- create an internet-reachable listener in the default configuration
  (REQ-S-010); or
- compromise the build, dependency, or release pipeline (REQ-S-019).

Thank you for helping keep an intentionally powerful tool honest and safe.
