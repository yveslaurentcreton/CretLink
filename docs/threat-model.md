# CretLink threat model

## Purpose and scope

CretLink lets an MCP-capable AI agent on a technician's own computer operate an
attended customer Windows computer through a temporary, authenticated,
encrypted LAN session. Because the product intentionally enables
administrator-level remote execution, its security must be a product property,
not an afterthought. This document is the public threat model required by
REQ-S-021. It covers pairing, LAN attackers, malicious peers, compromised AI
clients, command execution, file transfer, temporary persistence, relay
operation, and supply-chain risks.

This is a defensive document for a legitimate attended-support tool. It
describes abuse cases and attacks so they can be mitigated and detected, not as
a how-to. It contains no exploit code and no operational attack detail beyond
what is needed to justify a mitigation.

**Implementation-status honesty.** Milestones M001 (project foundation) and
M002 (pairing and the secure channel) are Done. Remote execution and files
(M003) and MCP control (M004) are implemented: the execution core
(ST-M003-001, ST-M003-002), the file-operation and transfer subsystem
(ST-M003-004, ST-M003-005), and the MCP bridge session control and file tools
(ST-M004-001 to ST-M004-004) have automated tests, a simulated-agent
end-to-end run in CI, and real-elevated-agent runs on maintainer hardware; the
milestones stay in progress because hardware validation is outstanding,
including the maintainer-run half ([`manual-tests.md`](manual-tests.md)) of
the privileged adversarial suite (ST-M003-007), whose automated half is green
and cited by test name below. Resilience and release hardening (M006) is in
progress: the reboot-resume ticket, proof and fail-closed checks (ADR-0014),
the residual-access inventory and `cretlink recover`, and
kill-on-close process-tree containment are implemented; wiring reboot-resume
into an actual reboot (the auto-start entry and the post-reboot reconnect) is
not, so hardware scenario MT-8 cannot pass yet. Official releases are
published with checksums, a dependency inventory, and SBOMs, though not yet
code-signed. The internet relay
(M007) is in progress: the opaque relay core, `cretlink-relay-server`, the
endpoint dialer, and the `wss`/TLS transport exist. Desktop control (M005) and
the technician terminal (M008) are draft milestones with experimental code
only. A mitigation described for an unshipped area is therefore a **design
commitment bound to a requirement**, not a claim about shipped code, unless the
section says otherwise. Each boundary section below states its own status, and
this document must be revisited whenever a milestone changes a trust boundary
(see [Maintaining this document](#maintaining-this-document)).

Requirement identifiers cited throughout refer to the security (REQ-S-###),
functional (REQ-F-###), and non-functional (REQ-N-###) requirements in the
project's specification.

## Assets

The things worth attacking, and therefore worth protecting:

- **Administrator authority over the customer computer.** The active,
  consented session itself — the highest-value asset. Whoever holds it can run
  arbitrary elevated commands (REQ-F-007).
- **Customer data.** Files, command output, system metadata, and later
  screenshots, all of which may flow to the technician computer and onward to
  the technician's AI service (REQ-S-016).
- **Session secrets.** Session keys and bearer capabilities (REQ-S-007), the
  Session Approval Code (SAC, bound to the cryptographic session by
  REQ-S-006), and the reboot-resume continuity ticket stored across a restart
  when the customer explicitly arms it (REQ-F-021, ADR-0014).
- **Technician-side secrets.** AI credentials, provider tokens, and account
  sessions. These must never reach the customer computer (REQ-S-015,
  REQ-F-015).
- **Consent integrity.** The accuracy of what the customer believed they
  approved (REQ-S-001, REQ-S-003) and the customer's ability to trust that
  "support has ended" means what it says (REQ-F-019).
- **Identity attribution.** The binding of every result to the correct target,
  session, and elevation state (REQ-F-016, REQ-S-011).
- **The audit trail.** The local append-oriented record of what happened
  (REQ-S-017).
- **Release artifacts and provenance.** Official binaries, checksums, the
  build pipeline, and repository and CI credentials (REQ-S-019, REQ-N-017).

## Threat actors

Actors recur across boundaries; each boundary section names the ones relevant
to it.

- **Passive LAN observer.** Records discovery and session traffic on a shared
  or hostile network (shop Wi-Fi, customer premises with unknown devices).
- **Active LAN attacker.** Injects, modifies, replays, or races traffic; spoofs
  ARP or mDNS; floods the network; may attempt to interpose during pairing.
- **Rogue endpoint / malicious peer.** A device speaking the CretLink protocol:
  a rogue console seeking control of a customer device, a tampered or
  deliberately dishonest support agent feeding false results to the technician,
  or a post-disconnect impersonator trying to inherit a session.
- **Compromised or misbehaving AI client.** An AI model or MCP client on the
  technician computer emitting harmful, deceptive, or malformed tool calls.
- **Adversary with content on the customer computer.** Resident malware, a
  booby-trapped file, or hostile text emitted by a program the AI runs — able
  to plant instructions in data the session reflects back to the AI (indirect
  prompt injection), or to attack the session components locally.
- **Malicious technician.** Someone who obtains genuine consent and abuses the
  granted authority.
- **Local opportunist after the session.** A later local user or malware that
  discovers leftover components or reconnect material.
- **Malicious relay operator (M007).** Modelled as fully hostile.
- **Supply-chain attackers.** A compromised dependency maintainer, an attacker
  with repository or workflow access, an artifact tamperer or impersonator, or
  a careless insider leaking secrets.
- **Inattentive humans.** Not malicious, but a customer or technician who
  confirms codes and consent screens without reading them is the enabling
  condition for several attacks.

A **fully compromised technician computer** is deliberately outside the
defended model: it holds the AI credentials, target selection, and audit log,
and no customer-side control can make it harmless. This is stated as a
trust-boundary fact throughout, not hidden (see
[Residual risks and explicit non-goals](#residual-risks-and-explicit-non-goals)).

## Trust-boundary overview

This mirrors the trust boundaries defined in the architecture requirements.

- **Technician boundary.** The technician computer is trusted to hold AI
  credentials, select the correct target, store the local audit log, and invoke
  tools. A compromised technician computer is therefore a critical threat that
  the customer agent cannot neutralise.
- **Customer boundary.** The customer computer trusts only the active,
  cryptographically paired session after local UAC elevation and full-control
  consent. It does not trust LAN discovery messages, an unconfirmed console, or
  a previous session.
- **LAN boundary.** The LAN is untrusted for confidentiality and authenticity.
  Discovery may be unauthenticated, but it cannot grant control. Pairing and
  all control traffic must tolerate observation, injection, replay, and
  competing peers.
- **AI boundary.** AI output is untrusted input to the execution system even
  after the customer grants full control. Tool arguments must be schema-valid
  and bound to the selected session. The product provides visibility,
  cancellation, audit, and honest outcomes; it does not attempt to semantically
  allowlist every administrator command.
- **Open-source supply-chain boundary.** Repository source, dependencies, CI,
  build artifacts, release metadata, and signing identities have different
  trust levels. Official artifacts must have verifiable provenance independent
  of the fact that the source is public.

The session state model is
`Idle → Discovered → Pairing → Code confirmation → Awaiting full-control
consent → Active → Revoking → Cleaning up → Closed`. Any authentication,
integrity, incompatible-version, or consent failure moves to a non-active
failure state, and network reconnection alone can never skip pairing or
consent.

The sections that follow analyse threats by attack surface across these
boundaries: pairing and session establishment, the local network, malicious
peers, the AI client, command execution, file transfer and staging, temporary
persistence and cleanup, the internet relay, and the supply chain.

### Pairing and session establishment

This surface covers everything between an idle customer device and an active,
consented session: LAN discovery, the manual-address fallback, authenticated
key agreement, Session Approval Code comparison, and the transition to
full-control consent.

**Status.** Implemented (M002, Done). Pairing is a Noise `XX` handshake
(`Noise_XX_25519_ChaChaPoly_SHA256` via `snow`) with fresh per-session static
keys, a prologue that commits both peers to the protocol version and capability
set, and a commit-reveal round inside the encrypted channel that binds the
handshake hash and two fresh nonces into a six-digit Session Approval Code
(ADR-0003). LAN discovery uses mDNS/DNS-SD scoped to private interfaces, with a
manual connection-code fallback (ADR-0007). Negative tests cover a
machine-in-the-middle producing different codes, capability and version
downgrade, replayed and tampered ciphertext, and oversized frames
(`crates/transport/tests/pairing.rs`).

**Entry points.**

- LAN discovery traffic (broadcast or multicast, mechanism to be decided):
  unauthenticated by design; anyone on the local network can observe, answer,
  or forge it.
- Manual address entry (REQ-F-004): a typographic error or a socially supplied
  wrong address leads to the wrong peer.
- The pairing and key-agreement exchange itself: the first messages either
  endpoint accepts from an unauthenticated network peer.
- Version and capability negotiation: pre-authentication protocol metadata an
  attacker may try to influence.
- The human comparison step: two people reading and confirming the SAC on the
  console and support UI.

**Abuse cases and mitigations.**

*Discovery spoofing and rogue endpoints.* An attacker answers discovery probes
or advertises a fake endpoint so the technician pairs with an attacker device,
or the customer device connects to an attacker console. Mitigations: discovery
conveys zero authority — the customer boundary trusts only the
cryptographically paired, code-confirmed, consented session, never a discovery
message or an unconfirmed console (REQ-S-001; architecture LAN boundary);
pairing authenticates the negotiated endpoints and binds the SAC to the
cryptographic session (REQ-S-006), so a rogue endpoint cannot survive code
comparison against the genuine peer; discovery and control traffic are limited
to the intended local interfaces with no internet-reachable listener by
default (REQ-S-010); before any tool becomes callable, the console displays the
target hostname, Windows version, architecture, session identifier, and
elevation state (REQ-F-016), giving the technician a second chance to notice a
wrong target; and the manual-address fallback (REQ-F-004) follows the identical
pairing and SAC path — entering an address never bypasses authentication.

*Machine-in-the-middle during key agreement.* An active attacker relays and
modifies the key agreement so each genuine endpoint unknowingly establishes
keys with the attacker. Mitigations: key agreement must use an established,
audited, authenticated design and maintained cryptographic libraries — CretLink
must not invent its own scheme (REQ-S-005); the SAC is derived from and bound
to the full protocol transcript (REQ-S-006; architecture transport
constraints), so an interposed attacker produces different transcripts on each
side and the two endpoints display different codes; a mismatched or unconfirmed
code cannot transition to full control (REQ-F-005), and authentication or
integrity failure denies control with a visible error rather than degrading
silently (REQ-S-009).

*Session Approval Code weaknesses.* A human-comparable code is necessarily
short, so an attacker who can force many pairing attempts could try to get
lucky. Mitigations: the SAC is bound to the protocol transcript (REQ-S-006)
and derived from fresh random nonces in a commit-reveal round scoped to exactly
one session attempt (ADR-0003), so each protocol run gives at most one
implicit guess; a failed run fails closed and terminates rather than allowing
retries against the same code (REQ-S-008, REQ-S-009); and because that round
runs inside the encrypted channel, an observer of the exchange gains no offline
advantage in predicting or brute-forcing the code. The stronger practical
attack is not guessing but *blind confirmation* — a customer or technician
confirming without comparing. Mitigations: both
endpoints must display the same code before full-control consent is possible
(REQ-F-005); consent screens must use plain language, accurately describe the
authority granted, and disclose what data may flow to the technician's AI
service (REQ-S-003, REQ-S-016, REQ-F-006, REQ-N-013); critical confirmation
controls should be keyboard accessible and not rely on colour alone
(REQ-N-014, SHOULD); and code comparison and full-control consent are
deliberately separate steps in the architecture's session state model, so one
hasty click cannot cover both.

*Replay of pairing messages.* An attacker records a successful exchange and
replays it later to resurrect a session, skip pairing on reconnect, or splice
old messages into a new exchange. Mitigations: the protocol must reject
replayed authentication messages (REQ-S-008); session keys and bearer
capabilities are random, short-lived, scoped to one session, and invalidated on
stop or expiry (REQ-S-007), so replayed material references authority that no
longer exists; reconnection must not restore full control without a valid
current session state (REQ-S-009), and the state model forbids re-entering
`Active` without passing pairing, code confirmation, and consent again, with
explicit lifecycle states (REQ-N-004).

*Downgrade attacks.* An attacker tampers with version or capability
negotiation so the endpoints settle on weaker cryptography, an older protocol
revision, or a mode that skips a required protection. Mitigations: a peer must
not be able to silently downgrade required security properties (REQ-S-008); the
negotiation outcome must be covered by the authenticated transcript the SAC
binds (REQ-S-006); protocol messages carry explicit schemas, bounded inputs,
and explicit version negotiation (REQ-N-012), and an incompatible or
manipulated negotiation is a fail-closed, visibly reported error, not a quiet
fallback (REQ-S-009, REQ-N-015). The initial product supports no legacy
insecure mode, so there is no weaker configuration to downgrade into.

*Competing simultaneous pairing attempts.* An attacker races a legitimate
pairing — initiating toward the same support agent or console while the
genuine attempt is in flight, hoping the humans confirm the wrong session — or
floods attempts to confuse or exhaust an endpoint. Mitigations: the session
state model admits one pairing flow at a time per endpoint with explicit,
predictable states (REQ-N-004), so a competing attempt cannot silently attach
to an in-progress flow; each attempt has its own transcript-bound SAC
(REQ-S-006; per-attempt scope per ADR-0003), so if the two humans are looking
at different sessions
the codes do not match and full control is unreachable (REQ-F-005); pairing
requires visible local initiation on the customer device (REQ-S-001) and all
connection state is shown, never hidden (REQ-S-002), so an unexpected
concurrent attempt is observable; pairing attempts and failures are recorded in
the local audit trail with session identity and timestamps (REQ-S-017); and
bounded inputs and resource limits (REQ-N-012, REQ-N-006) constrain flooding —
a flood degrades availability, not authentication.

**Residual risks.**

- Humans can mis-compare or blindly confirm. UI design reduces but cannot
  eliminate a customer approving a code that does not match, especially under
  social pressure from a caller claiming to be the technician. CretLink cannot
  verify real-world authorisation; it can only make the grant explicit,
  visible, and revocable (REQ-S-004, REQ-F-018).
- A short SAC is a deliberate usability trade-off. The design limits an active
  attacker to roughly one online guess per visible, failed attempt; it does not
  make guessing impossible, only unattractive and detectable.
- Discovery denial of service is accepted. Unauthenticated discovery can be
  jammed or polluted; the fallback is manual addressing (REQ-F-004).
  Availability of pairing on a hostile LAN is not guaranteed — only that a
  hostile LAN cannot obtain control.
- A compromised endpoint before pairing is out of scope. Malware already
  running on the customer computer, or a fully compromised technician computer,
  can subvert its own side of pairing and consent; see the malicious-peer
  section and the consolidated residual risks.
- Client-isolated or multi-segment LANs may prevent discovery or direct
  connection entirely; this is an assumption of the initial product, not a
  mitigated threat.

### LAN attackers

This surface covers adversaries with a presence on the same local network as an
active or forming session, but with no software on either endpoint. A LAN
attacker is assumed able to observe all broadcast traffic, inject arbitrary
packets, spoof ARP and mDNS, race legitimate peers, and flood the network — but
*not* to control either endpoint (those cases belong to the malicious-peer and
technician boundaries).

**Status.** Implemented (M002, Done): mDNS/DNS-SD discovery scoped to private
interfaces with a manual connection-code fallback (ADR-0007), and the Noise
`XX` secure channel (ADR-0003) described in the pairing section.

**Entry points.**

- Discovery traffic: broadcast or multicast announcements that let the console
  and support agent find each other without typing an address (REQ-F-004) — the
  only surface deliberately reachable by unauthenticated LAN peers.
- The pairing and control listener that accepts the handshake and, after
  pairing, carries the encrypted session. The architecture prefers the
  customer agent to initiate the control channel toward the technician console
  where practical, so the long-lived listener sits on the technician side. It
  is bound to one private address of the technician's computer, never to a
  public or wildcard address (REQ-S-010, ST-M009-016).
- In-flight session traffic: encrypted frames carrying commands, output
  streams, and file transfers.
- The network fabric itself: ARP, DHCP, mDNS/DNS, and switching behaviour that
  a LAN attacker can influence to redirect, observe, or suppress traffic.

**Abuse cases and mitigations.**

*Passive observation.* An attacker records discovery announcements and session
traffic. If session traffic were plaintext, this would expose commands, output,
transferred files, and any secrets they contain — administrator-level material
by design. Mitigation: session confidentiality and integrity use audited,
maintained cryptographic protocols and libraries; CretLink will not invent an
algorithm or use unauthenticated encryption (REQ-S-005).

*Active injection and manipulation.* An attacker injects, modifies, reorders,
or replays frames, attempting to insert commands, corrupt a transfer, alter
results shown to the technician, or resurrect authority from a recorded
handshake. Mitigations: authenticated channel integrity (REQ-S-005); rejection
of replayed authentication messages and prevention of silent downgrade
(REQ-S-008); ephemeral, session-scoped keys and capabilities (REQ-S-007) so
recorded traffic has no value for a later session; and reconnection cannot
restore control without valid session state (REQ-S-009). File transfers
additionally verify length and a cryptographic digest before being reported
complete (REQ-N-007), so injected or truncated data is detected rather than
silently accepted.

*Machine-in-the-middle via ARP or mDNS spoofing.* An attacker poisons ARP or
answers discovery queries first, placing itself between console and agent
during pairing. The mitigation chain is the one described in the pairing
section: endpoint authentication with the SAC bound to the transcript
(REQ-S-006, REQ-F-005) means an interposed attacker produces mismatched codes,
so ARP or mDNS spoofing can redirect packets but cannot yield an approved
session.

*A session offered on the wrong network (ST-M009-016).* A technician's
computer is often on several private networks at once: its local network, a
VPN, the virtual switch of a hypervisor. The listener is bound to exactly one
of their addresses, and discovery announces that address alone, on its adapter
alone, so a waiting session is not offered on a VPN or a virtual network that
the technician did not choose, and the customer's list never holds an address
that nobody listens on. The waiting screen names the adapter, and the
technician can move the session to another network; the address asked for
must be one this computer itself listed, so the window cannot make the
listener bind anything else. Without a choice the application waits on the
address the computer uses to reach other networks, unless that adapter is a
point-to-point link. To learn that address it connects a datagram socket to
an address reserved for documentation and reads the socket's local address:
nothing is sent, nothing listens, and the socket is closed at once —
*implemented (`crates/discovery/src/listen.rs`,
`apps/app/src-tauri/src/technician/network.rs`)*.

*Malicious discovery announcements.* An attacker advertises a fake console to
the agent, or a fake agent to the console, floods the discovery view with
plausible-looking entries so the technician selects the wrong target, or
advertises misleading names. Mitigation: discovery output is an untrusted hint
used only to propose a pairing attempt. Control requires the full chain —
visible local initiation and consent on the customer computer (REQ-S-001),
authenticated pairing bound to a human-compared SAC (REQ-S-006, REQ-F-005), and
a single clearly worded full-control consent (REQ-F-006). A malicious
announcement can at worst start a pairing attempt that fails code comparison;
it cannot mint authority.

*A borrowed technician name (ST-M009-011).* The console advertises its
technician's display name (the account's full name, else the user name) and
sends it again, as an optional field of its pairing `Nonce` (protocol minor 10),
so the customer window can say who the technician's computer claims to be. Any
LAN peer can advertise, and any console can send, whatever name it likes.
Mitigation: the name is validated to one bounded line without control or
text-direction characters, it is not part of the Session Approval Code
derivation and grants nothing, and the customer surface labels it as what the
technician's computer says, never as a verified identity. Trust still comes
only from comparing the code with the person on the phone (REQ-S-006). Residual
risk: a customer who reads only the name, not the code, can be misled; the
consent flow keeps the code comparison mandatory for that reason.

*The customer's account in the target identity (ST-M009-008).* Since protocol
minor 11 the agent's `TargetIdentity` (hostname, Windows version, architecture,
elevation, agent version) also carries, as an optional `account`, the account
its process runs as (`DOMAIN\user` from `GetUserNameExW(NameSamCompatible)`,
the machine name as the domain for a local account, or the bare user name
when only that can be read). It is the customer's own account name,
shown to the technician in the live identity line, and, like the hostname, part
of the target identity in AI-facing results, so it reaches the technician's AI
client; it is not a credential and grants nothing. It is validated on decode
to one bounded line without control or text-direction characters, omitted when
the account could not be read, and not part of the reboot-resume target
binding, whose pre-image is frozen (ADR-0014). Like every identity field it is
what the agent reports, not a verified claim.

*Denial of service.* An attacker suppresses or floods discovery, exhausts the
listener with connection attempts or oversized messages, or disrupts an active
session by cutting traffic, forcing resets, or jamming Wi-Fi. Subtler goals are
making a disconnect look like success, or making repeated failures pressure
users into unsafe workarounds. Mitigations: blocked discovery falls back to
manual address entry over direct IP connectivity (REQ-F-004); session lifecycle
states are explicit and idempotent where practical (REQ-N-004); interruption is
never reported as success — completed, failed, cancelled, timed-out, and
unknown outcomes are distinguished (REQ-N-008); resource limits and
backpressure bound the cost of floods (REQ-N-006); and stop and revocation work
from either endpoint without the other's cooperation (REQ-S-004, REQ-F-018), so
an attacker who can disrupt traffic cannot prevent revocation from taking
effect locally. Denial of service degrades availability, never safety.

*Hostile guest or isolated network conditions.* Client-isolated guest networks
silently drop peer-to-peer traffic; discovery appears dead or connections
half-work, which can push users toward disabling protections or trusting
whatever endpoint does respond. Mitigations: manual addressing serves networks
where multicast is filtered but unicast works (REQ-F-004); errors identify the
failed stage and a safe next action (REQ-N-015); and all initial session
functions work with no internet access on the customer computer (REQ-N-003), so
an attacker who black-holes internet egress on a guest network gains no
leverage over the session itself.

*Port scanning of the listener.* A scanner enumerates open ports, fingerprints
the listener, and probes it with malformed or hostile input, looking for
pre-authentication parsing flaws, resource exhaustion, or information leakage.
Mitigations: discovery and control traffic are limited to intended local
interfaces and no internet-reachable listener exists by default (REQ-S-010);
the pre-authentication surface is deliberately small — schemas with explicit
length limits, bounded inputs, and version handling on every network message
(REQ-N-012), bounded buffers and backpressure (REQ-N-006), and errors that
identify the failed stage without exposing credentials or cryptographic secrets
(REQ-N-015). There are no provider or AI secrets on the customer device for a
scanner to reach (REQ-S-015).

*Detection and evidence.* Explicit target identity on every result (REQ-F-016)
and the local append-oriented audit trail with session identity, timestamps,
and outcomes (REQ-S-017) give the technician a way to notice anomalies such as
repeated pairing failures, unexpected disconnects, or duplicate discovery
entries.

**Residual risks.**

- Availability on a hostile LAN is not guaranteed. An attacker with LAN access
  can jam, poison, or flood until no session can form or survive. CretLink
  commits to failing closed and reporting honestly, not to completing a session
  under active denial of service.
- Metadata is observable. Even with an encrypted channel, a passive observer
  learns that a CretLink session exists, the endpoint addresses, approximate
  traffic volume, and timing. Traffic-analysis resistance is out of scope.
- The SAC depends on people actually comparing it (see the pairing section);
  ADR-0003 fixes it at six digits from a commit-reveal round, and the human
  step remains a real weakness.
- Discovery flooding and name spoofing cannot be fully prevented.
  Unauthenticated discovery means an attacker can always populate the target
  list with noise or misleading names. The defence is that selection leads only
  to pairing and code comparison, not to control; the confusion itself is
  accepted as residual.
- Pre-authentication parser risk is reduced, not eliminated. Bounded schemas
  shrink the surface, but any implementation flaw in handshake parsing would be
  reachable by a LAN scanner. This is mitigated over time by negative and
  malformed-input tests required for stories touching this boundary, and by the
  coordinated vulnerability-reporting path (REQ-S-022).
- Client-isolated networks are an assumed-away condition; on isolated networks
  the session simply fails. The risk that frustrated users route around
  isolation insecurely is addressed only by clear, actionable errors
  (REQ-N-015), not by a technical control.
- Nothing here defends the endpoints; a LAN attacker who also compromises an
  endpoint moves into a different boundary where these mitigations do not
  apply.

### Malicious peers

This surface covers adversaries who participate in, or pretend to participate
in, the CretLink protocol itself: a rogue console that tries to control a
support agent without consent, a malicious or modified support agent that lies
to the console, and impersonation of either endpoint after a disconnect. Unlike
the LAN attacker, a malicious peer speaks the protocol and may hold one side of
a session.

**Status.** Implemented. Pairing, the secure channel (M002), the
remote-execution core, and reconnection handling (M003) are implemented and
have been run on maintainer hardware; the automated half of the privileged adversarial and negative
suite (ST-M003-007) that hardens this surface has landed —
`simultaneous_bilateral_revoke_terminates_both_without_hang`,
`console_revoke_tears_down_both_fail_closed` (which also proves a closed session
is not re-joinable), and `agent_revoke_tears_down_both_fail_closed` in
`crates/session/tests/e2e.rs` — while its maintainer-run hardware half
([`manual-tests.md`](manual-tests.md)) is still outstanding.

**Entry points.**

- The pairing and discovery surface of the support agent, reachable by any
  device on the same LAN before a session exists.
- The authenticated control channel of an active session, from the perspective
  of the peer at the other end.
- The reconnection path after a network interruption, when either endpoint may
  try to resume or re-establish a session.
- The result path: command output, file contents, digests, and self-reported
  identity and status returned by the support agent to the console, and from
  there to the technician's AI client.

**Abuse cases and mitigations.**

*A rogue console attempts to control an agent without consent.* A rogue
console may answer discovery, initiate pairing repeatedly, race a legitimate
console, or try to negotiate weaker security properties. Success would mean
arbitrary administrator-level execution on the customer computer — the
highest-impact case in this boundary. Mitigations: discovery grants nothing,
traffic is limited to local interfaces, and no internet-reachable listener
exists by default (REQ-S-010); control requires a chain a rogue console cannot
complete remotely — visible local initiation and consent on the customer
computer (REQ-S-001), visible UAC elevation (REQ-F-002), and a SAC displayed on
both endpoints and confirmed by the people present (REQ-F-005), bound
cryptographically to the negotiated session (REQ-S-006) so a raced or relayed
pairing produces a mismatched code; the protocol uses established, audited
cryptography (REQ-S-005), authenticates both endpoints (REQ-S-006), rejects
replays, and prevents silent downgrade (REQ-S-008); any authentication,
integrity, version-negotiation, or authorisation failure denies control with a
visible error (REQ-S-009), and malformed or oversized pairing traffic is
rejected rather than interpreted (REQ-N-012); consent language states the real
authority being granted (REQ-S-003, REQ-F-006, REQ-N-013), so a rogue console
cannot rely on the customer believing they approved something harmless; and the
customer can revoke at any time without the console's cooperation (REQ-S-004,
REQ-F-018), with the agent hiding nothing about its connection state
(REQ-S-002), so an unexpected session is visible and stoppable at the machine
it targets.

*A malicious or modified support agent lies to the console.* A tampered agent
controls everything it sends: it can claim a false hostname or Windows
identity, claim an action ran elevated when it did not, fabricate exit codes
and output, attribute output to the wrong job, report cleanup that never
happened, present a digest computed over different content than it claims, or
craft output designed to manipulate the technician's AI client. CretLink's
requirements on the agent (privilege truthfulness, REQ-S-011; honest unknown
outcomes, REQ-N-008; process-tree and job-identifier correctness, REQ-S-014)
constrain the official product, not an attacker's binary. The design therefore
does not assume agent honesty; it limits what a lying agent can achieve:

- Blast radius is capped by construction: no AI credentials, provider tokens,
  or technician account secrets ever reach the customer computer (REQ-S-015,
  REQ-F-015), and the MCP bridge is local to the technician computer
  (REQ-F-013). A malicious agent gains no credential to steal and no network
  authority over the technician machine; its weapon is deception, not access.
- Agent output is untrusted input. The architecture treats all remote command
  output and file content as untrusted data even inside a consented session.
  The console and MCP bridge attach session and target identity to every result
  (REQ-F-016) so the AI and technician can always distinguish the remote host
  from the local one (REQ-F-011) and cannot silently act on data from an
  unselected or stale session.
- Claims are recorded, not believed. The console keeps its own append-oriented
  audit trail with session identity, timestamps, actions, result status, and
  digests (REQ-S-017), and shows live activity in near real time (REQ-F-017).
  An agent can lie, but it must lie consistently, on the record, to an
  attending technician who can stop the session at once (REQ-F-018).
- Transfer digests bound what tampering can hide. Length and digest
  verification (REQ-N-007) guarantee that the console received exactly the
  bytes the agent sent, defeating in-path modification. They deliberately do
  not prove the agent is honest about where those bytes came from; that limit
  is recorded under residual risks.
- Tampered binaries are detectable before use. Official releases are built by
  documented automation from tagged commits and ship with SHA-256 checksums
  (REQ-S-019), so a tampered binary is detectable and a technician can direct
  the customer to a verifiable artifact. Authenticode signing is implemented
  but no signing identity is configured, so current releases are unsigned.

*Impersonation of either endpoint after disconnect.* After a Wi-Fi drop or a
deliberately induced disconnect, an attacker may take over the agent's address
or discovery name and wait for the console to reconnect, replay captured
handshake material, or present themselves to the agent as the returning console
in the hope of inheriting full control without consent. Mitigations:
reconnection alone restores nothing — reconnecting must not restore full
control without valid session state (REQ-S-009), and the state model makes
every failure transition to a non-active state from which pairing and consent
cannot be skipped (REQ-N-004); session authority is ephemeral — keys and bearer
capabilities are random, short-lived, session-scoped, and invalidated on stop
or expiry (REQ-S-007), so a stopped or expired session leaves nothing to
inherit, and stop is available to both endpoints and fails closed (REQ-S-004,
REQ-F-018); replayed authentication messages are rejected (REQ-S-008); a fresh
session started by an impersonator is just a new pairing, producing a new SAC
that the people at both real endpoints will not have confirmed (REQ-F-005,
REQ-S-006) and requiring new visible consent on the customer computer
(REQ-S-001); cleanup removes temporary credentials, services, and access paths
after normal shutdown (REQ-F-019, REQ-S-012), and errors during resumption name
the failed stage without leaking secrets (REQ-N-015), so a failed impersonation
surfaces as a visible pairing failure rather than a silent retry loop.
Reboot-resume (ADR-0014) is explicitly armed by the customer, single-use,
expiring, and target-bound (REQ-F-021); proving continuity after a reboot never
restores control — attended consent runs again on a fresh handshake — so the
ticket is not a standing reconnection credential an impersonator could exploit
(verified headlessly; hardware scenario MT-8).

**Residual risks.**

- Cryptography authenticates the channel, not the peer's honesty. The SAC
  proves both people confirmed the same negotiated session; it cannot make a
  malicious agent binary tell the truth about its hostname, elevation, results,
  or cleanup. CretLink does not attempt remote attestation of the peer binary
  and does not claim to. The defences are attended visibility, provenance
  checking of the official artifact, and treating all agent output as untrusted
  claims.
- A digest verifies transfer, not provenance. A malicious agent can compute a
  correct digest over dishonest content; REQ-N-007 protects against corruption
  and in-path tampering only.
- Poisoned output can still influence the AI. Even with results marked
  untrusted and identity-stamped, crafted command output may steer an AI
  client's reasoning. CretLink mitigates with technician visibility,
  cancellation, and audit (REQ-F-017, REQ-F-018, REQ-S-017) rather than
  claiming to sanitise arbitrary administrator output; this overlaps with the
  AI-client boundary.
- Social engineering of the attending customer. If an attacker persuades the
  customer to launch an agent, read out or confirm the attacker's SAC, and
  grant consent, the protocol has worked as designed for the wrong technician.
  Clear risk language (REQ-N-013, REQ-S-003) reduces but cannot eliminate
  this.
- A compromised host defeats its own endpoint. CretLink does not defend
  against a customer computer whose operating system is already compromised and
  tampers with the running agent, nor against a fully compromised technician
  computer, where a rogue console is indistinguishable from the legitimate one
  because it holds the same trust.
- Code signing is a SHOULD under REQ-S-019: it is implemented but gated, and
  no signing identity is configured, so current releases are unsigned. The
  relay (REQ-F-022, M007) and reboot-resume (REQ-F-021) re-open parts of this
  boundary; they are analysed in their own sections (REQ-S-021).

### The AI client: compromised AI and prompt injection

CretLink treats the AI as untrusted input to the execution system at all
times, including after the customer grants full control. Consent authorises the
*session*, not any particular AI output. The AI client runs only on the
technician computer, holds no network route or credential to the customer
device, and can act on the target solely through the local MCP bridge and the
one explicitly selected, consented session (REQ-F-013, REQ-F-015, REQ-S-015).

**Status.** Implemented. Pairing (M002) and the remote-execution core (M003)
are implemented and have been run on maintainer hardware. The MCP bridge
(M004) is implemented: session control (ST-M004-001, ST-M004-002,
ST-M004-004) and the generic file and transfer tools (ST-M004-003) have
automated and simulated-agent CI coverage plus a real-elevated-agent
end-to-end run on maintainer hardware recorded on 2026-08-10.

**Entry points.**

- Tool-call arguments from the MCP client into the bridge: `remote_exec`
  command lines, stdin data, working directories and the constrained
  environment-override set (REQ-F-009), file paths and contents, transfer
  sources and destinations, job identifiers, cancellation and timeout
  parameters, and the stated `purpose` every tool requires (REQ-F-026,
  ST-M009-007), a short text the attended customer reads.
- Reflected content returned to the AI: command stdout and stderr, exit codes,
  file contents, directory listings, filenames, and system metadata from the
  customer computer — the channel through which hostile content on the target
  can reach the AI's context.
- Desktop observation widens the reflection surface: M005 is a draft; an
  experimental op-set exists behind `desktop.v1`, and its screenshots expose
  on-screen text — including web pages, chat windows, and malware UI — to the
  AI as untrusted input.

**Threat actors.** A misbehaving or compromised AI model or MCP client on the
technician computer; an adversary with content on the customer computer who
cannot talk to the AI directly but can plant instructions in the data the
session reflects back to it (indirect prompt injection); and a remote adversary
reachable from the customer computer, if that machine has internet access, who
benefits when an injected or compromised AI is steered into sending data
outward or fetching hostile payloads.

**Abuse cases.**

- *Prompt-injected instructions reflected into tool calls.* Command output or
  file contents contain text instructing further commands, weakened defences,
  persistence, or uploads; the AI incorporates the text into its reasoning and
  issues matching tool calls. Because the session legitimately holds
  administrator authority, the injected actions are schema-valid and cannot be
  distinguished mechanically from intended repair work.
- *Exfiltration attempts.* The AI is induced (or a compromised client is
  instructed) to read sensitive customer files and move them off the device —
  via `remote_download` to the technician side, or via arbitrary commands that
  transmit data from the customer computer directly.
- *Credential planting or capture.* Attempts to copy technician AI
  credentials, tokens, or account state to the target, or to invoke
  credential-harvesting behaviour on the target.
- *Hostile or degenerate arguments.* Schema-invalid messages, oversized
  command lines or file payloads intended to exhaust buffers, ambiguous or
  forged job identifiers aimed at cancelling or hijacking the wrong job, and
  attempts to smuggle protocol-level effects through argument fields.
- *Wrong-target confusion.* The AI treats a remote path, process, or shell as
  local (or the reverse), acts on a stale or revoked session as if it were
  live, or is confused into addressing an unselected target.
- *Outcome masking.* A hostile client reports or assumes success for work that
  failed, was cancelled, or never reached the target, so the technician's
  picture of the customer machine diverges from reality.
- *Misleading stated purpose.* An injected or compromised AI states a harmless
  reason ("Checking free disk space") for a harmful action, or crafts the text
  to look like a system message, to talk the customer out of pressing Stop —
  social engineering through the attended surface itself.

**Mitigations.**

- *Structural containment.* AI credentials, provider tokens, and account
  sessions never reach the customer computer (REQ-F-015, REQ-S-015), the MCP
  transport is local to the technician computer (REQ-F-013), and no
  internet-reachable listener exists by default (REQ-S-010). A compromised AI
  client gains no direct route to the target; everything funnels through the
  bridge, where it can be validated, attributed, audited, and stopped.
- *Schema validation and bounded inputs.* Every MCP and network message has an
  explicit schema, version handling, and bounded inputs (REQ-N-012); buffers,
  logs, and transfers have documented limits and backpressure (REQ-N-006).
  Schema-invalid or oversized tool calls are rejected, and validation or
  authorisation failure denies the action rather than degrading it
  (REQ-S-009).
- *Session and target binding.* Tool calls are routed only to the explicitly
  selected, consented session (REQ-F-013); switching targets requires a
  deliberate console action, and every tool result carries the target
  hostname, session identifier, and elevation state (REQ-F-016) so the AI,
  technician, and audit trail agree on which machine acted. Job identifiers
  are tracked per process tree with confusion prevented, so cancellation hits
  the intended job (REQ-S-014, REQ-F-008).
- *Reflected content treated as data.* Remote command output and file contents
  are transported, displayed, and logged as untrusted data; the bridge and
  console never interpret them as control messages, and the control plane is
  kept structurally separate from remote data by the architecture's
  AI-boundary rule and the explicit message schemas (REQ-N-012). CretLink
  cannot stop the AI *model* from being persuaded by hostile text, but it
  ensures such text has no mechanical authority over the session.
- *Human oversight of an untrusted AI.* The console shows session state,
  running commands, transfers, and failures in near real time, and each visible
  action is attributable to a specific MCP tool call (REQ-F-017, ST-M004-006):
  every routed tool result carries the same console-minted `call_id` the audit
  record was written under, so the AI transcript maps one-to-one onto the audit
  trail (result `call_id` ↔ audit `call_id` ↔ job or transfer id). The
  console distinguishes active AI control from an idle bridge connection through
  an in-flight count of routed calls, surfaced to the technician CLI; a richer
  graphical indicator on the same signal is not yet implemented. Every wire
  request carries an `origin` (`console`, `mcp`, or `terminal`), so the
  customer-side agent can label AI-driven operations distinctly from
  technician-driven ones and hides nothing (REQ-S-002).
  `origin` is **attribution only**: it grants no new authority and changes no
  routing, consent, or fail-closed behaviour — a forged or defaulted origin
  cannot widen what a session may do.
- *Stated purpose as an unverified claim (REQ-F-026, ST-M009-007).* Every AI
  tool call carries a required, plain-language `purpose`. The bridge rejects a
  missing or malformed one before anything is sent; it is bounded to one line
  of at most 200 characters and refused if it contains control, line-break,
  or text-direction (bidirectional override) characters, so it cannot spill
  over or visually reorder the line beside it. It travels to the agent as an
  optional request field (protocol minor 9), is recorded verbatim in the
  console audit records of the call, and both surfaces show it **labelled as
  the AI's own unverified reason, next to the real command, path, or action,
  its source, and its result — never instead of them**. Like `origin`, it grants
  no authority and is never interpreted: it is display and audit text. A
  technician's own actions carry no purpose and are labelled by their source.
- *One activity item on both surfaces (ST-M009-011).* The technician's console
  and the customer window build the same activity item for every action: its
  source, the AI's stated purpose, a plain description, its kind, the real
  operation, path and byte count, the honest state, the start time and the
  duration. The console builds it from its own capture-time audit records of
  every actor (the AI, the technician terminal, manual console actions),
  including read-only AI calls; the customer side builds it from what its agent
  actually did. A file leaving the customer computer is reported when sending
  starts and again when it ends, with its byte count (REQ-S-016). Every
  request the agent refuses — one bound to another session, or a refused
  transfer chunk (once per path until that path's next reported request) —
  is a failed item on the customer side too, with the reason in plain words
  and no file content. A request the technician's computer refuses before
  sending it (a path that fails the wire rules, such as a UNC or traversal
  path) never reaches the customer computer, so only the technician side
  shows it. Events shed
  under pressure on either side are counted and shown as "N items were not
  shown" where they were dropped, never as a silent gap (REQ-N-008); the
  durable audit log is never lossy. An item still running when the session ends
  is shown as ended with the session, never as done.
- *The AI client's name stays on the technician computer (ST-M009-011).* The
  bridge passes the MCP client's self-reported `clientInfo.name` to the console
  over local IPC (IPC minor 10), bounded to one line; the console records it in
  its audit records and labels the activity "Claude Code", "Codex CLI",
  "Gemini CLI" or "AI assistant". It is the client's own claim and grants
  nothing, and the console never puts it on the session protocol, so the
  customer computer only ever sees "AI assistant".
- *Audit trail.* The append-oriented audit trail records
  session identity, actor or source, action, arguments, and results
  (REQ-S-017), so injected or malicious actions are observable during the
  session and reconstructable afterwards.
- *End records and the session history (ST-M009-012).* At consent the console
  records the identity the consent bound (`target_identity`); at the end,
  before the file is sealed, it records who ended the session
  (`session_ended`: technician, customer, lost connection or failure) and the
  cleanup report the agent sent with every note (`cleanup_report`). The agent
  sends that report over the encrypted channel at the end of an active
  session (`CleanupReported`, protocol minor 11), before it closes after the
  technician's stop and before its own `Revoke` when the customer stops. The
  report is a claim of the customer computer, like every agent result: it is
  bounded (at most 32 notes of 512 bytes, one line each, validated on
  decode), recorded as the agent's, and grants nothing. Without one — a lost
  connection, an interrupted session — the record says so and the cleanup is
  shown as unknown, never as success (REQ-N-015, REQ-F-019); a session that
  never reached full control needed none. The history reads these records
  back and verifies each file's chain and seal first: a broken file is
  summarized only from its verified prefix and shown as damaged, an unsealed
  one as unfinished, and neither is skipped. The records hold no session
  secret (REQ-S-017). `open_audit` opens only the audit folder or a file the
  history listed inside it, with the platform opener and no shell.
- *The customer keeps no record unless asked (ST-M009-012, REQ-F-027).* The
  customer window builds its end report in memory from what it showed and
  the agent's own teardown report, and writes nothing. Only **Save a copy**
  writes a file — after a save dialog, into a folder of the customer's
  choosing (`Documents\CretLink\` by default, created only then) — as one
  HTML file with inline CSS, no script and no external resource, in which
  every string from the session (operations, paths, purposes, notes, the
  technician's name) is escaped, so a hostile string cannot run or reach the
  network when the file is opened (REQ-S-016). The report holds what the
  customer saw and no session secret: no approval code, no key, no
  connection code. **Open** opens only a file this process saved, with the
  platform opener started by its absolute path (`%SystemRoot%\explorer.exe`,
  `/usr/bin/open`): Windows looks for a bare program name in the running
  program's own folder first, often `Downloads` for the customer, where a
  planted `explorer.exe` would otherwise run with the session's
  administrator rights. On Linux (technician only) `xdg-open` is found on the
  user's own `PATH`. The technician's host name is read without starting a
  program by name either (`COMPUTERNAME`, the kernel's host name, or
  `/bin/hostname` on macOS).
- *The technician interface (ST-M009-008).* The technician window renders
  every string that comes from the customer computer or an AI (operations,
  paths, hostnames, notes, stated purposes, codes) as text nodes only: its DOM
  builder has no markup path and refuses inline event-handler attributes, and
  a rendering test checks that markup in an activity item stays text
  (REQ-S-016, REQ-S-018). A stated purpose is labelled as such, above the real
  operation. Items shed under load are shown as "N items were not shown" where
  they were dropped, never silently. The end report takes the cleanup result
  from the sealed audit file and says "Cleanup unknown", never "Cleaned up",
  when no cleanup report arrived or the file cannot be read; a broken or
  unsealed audit file is shown as such (REQ-N-015). The Live tab **Audit**
  shows this session's audit records from its own audit file, only the ones
  the hash chain verified, each as one bounded line built from what the
  record already holds — records carry no secret by construction
  (REQ-S-017), redactions read `[REDACTED]` and bytes only as a digest — and
  never a payload or a chain field; a file name that is not a plain name in
  the audit folder is not read. Connecting an AI client
  shows the exact commands before anything changes, and a run-once copy warns
  that its registration breaks when the copy moves (REQ-F-025).
- *Immediate interruption and ephemeral authority.* Customer and technician
  each hold an always-available stop control; stop rejects new tool calls,
  terminates or honestly reports running work, and invalidates session secrets
  (REQ-F-018, REQ-S-004). Session authority is short-lived and scoped to one
  session (REQ-S-007), reconnection cannot restore control without valid
  session state (REQ-S-009), and cleanup removes temporary access paths
  (REQ-F-019, REQ-S-012). A hostile AI cannot convert one session into durable
  access.
- *No high-value primitives to inject into.* The tool surface is generic and
  contains no credential harvesting, password extraction, keylogging, or
  authentication-bypass functions for an injected instruction to invoke
  (REQ-S-013). An attack must express itself as ordinary, visible, audited
  commands.
- *Honest outcomes.* The execution layer reports its true identity and
  elevation (REQ-S-011) and distinguishes completed, failed, cancelled,
  timed-out, and unknown outcomes rather than ever silently reporting success
  (REQ-N-008), limiting a hostile client's ability to mask failure or
  fabricate state.
- *Informed exposure.* Before consent, the customer is told that command
  output and selected files may be sent to the technician computer and onward
  to the technician's AI service (REQ-S-016), the consent wording states the
  real authority granted (REQ-S-003, REQ-F-006), and the console warns that
  arbitrary administrator commands can reveal sensitive data, with redaction
  markers and configurable retention planned (REQ-S-018).

**Residual risks.**

- No semantic allowlisting of administrator commands. Within a consented
  full-control session, CretLink deliberately does not attempt to judge
  whether each schema-valid command is benign. A prompt-injected or
  compromised AI can therefore execute harmful commands indistinguishable in
  form from legitimate repair work. The defence is visibility, attribution,
  audit, and immediate stop — detection and interruption, not prevention.
- Prompt-injection resistance of the model is outside CretLink's control.
  CretLink cannot sanitise the semantic content of remote output before the AI
  reads it, and it depends on the technician actually watching the activity
  view. Model-level safeguards and technician vigilance are assumed, not
  enforced.
- A misleading stated purpose is not detectable. CretLink cannot check that
  the AI's reason matches what it does; a prompt-injected or compromised AI can
  state a reassuring purpose for a harmful action. The mitigation is that the
  real operation is always shown beside the purpose and the purpose is
  labelled as the AI's claim, so the customer and technician can see the
  mismatch and stop the session. One gap remains: both application backends
  now emit the same activity item, with purpose, for every action
  (ST-M009-011), but the new interfaces that render it are separate stories
  (ST-M009-008, ST-M009-009); until they land, the windows render less than
  the backends emit. A console older than IPC minor 9 receives calls without
  the purpose, so they are shown and audited without one. In the other
  direction, the console sends the customer's `account` in a routed target
  identity only to a client whose `Hello` reported IPC minor 11 or later, so an
  older bridge, which refuses unknown identity fields, never fails closed on
  every answer (REQ-N-012).
- Exfiltration by command is not blockable. An AI with consented administrator
  control can move customer data using ordinary commands, including directly
  off the customer computer when that machine has internet access. CretLink
  makes such actions visible and audited; it cannot make them impossible
  without abandoning the general-purpose surface the product exists to
  provide.
- Deferred surface: output redaction is a SHOULD (REQ-S-018) and not yet
  designed in detail. Desktop control adds an on-screen prompt-injection
  channel analysed in
  [Desktop observation and control (M005)](#desktop-observation-and-control-m005);
  M005 is a draft and an experimental op-set exists behind `desktop.v1`, so
  those mitigations rest on the design and automated tests rather than
  hardware evidence.
- Audit is local and technician-held: it evidences what happened to an honest
  technician and is not tamper-proof against a dishonest one.
- Bridge-to-console IPC lacks full Windows DACL hardening. The local IPC that
  carries tool calls from the MCP bridge to the console is same-user by
  construction — a per-user named pipe with `reject_remote_clients(true)` and
  `first_pipe_instance(true)` on Windows, a `0600` socket in a `0700` per-user
  directory on Unix (ADR-0004). An explicit same-user DACL on the pipe and a
  connect-side verification of the pipe server's owner SID both require raw
  Win32 calls and therefore `unsafe`, which the workspace denies, so they are
  not yet applied (see `crates/mcp/src/ipc/transport.rs`). This is an accepted
  residual risk: it must fail closed if it cannot be applied, and it remains the
  ADR-0004 threat-model item to close.
- A fully compromised technician computer defeats this boundary entirely; see
  the consolidated residual risks.
- MCP client registration (`cretlink-cli mcp install`, ST-M009-003) runs the
  client command lines it finds on `PATH` (`claude`, `codex`, `gemini`) as the
  current user, and only adds or removes a `cretlink` entry that starts the
  running program with the argument `mcp`. It writes no environment variable,
  credential or session secret, and reads the clients' configuration files only
  to compare. A hostile program named like a client earlier on `PATH` would run,
  exactly as when the technician types that name; `cretlink-cli mcp status` prints
  the resolved path of each client so this stays visible. A registration that
  points at a location other users can write lets them replace the program that
  every AI client session starts, so register a copy in a per-user location.

### The terminal IPC client: a second local consumer of the consented session

The interactive terminal (ST-M003-009) introduces a **second class of local IPC
client** beside the MCP bridge. A terminal consumer — the console terminal panel
or the command-line terminal client — connects
to the same same-user IPC endpoint (ADR-0004) and reaches the same one
explicitly selected, consented session, but it is a distinct actor with its own
attribution and its own bounded message set. It is not the AI: terminal activity
is never counted as AI in-flight control and is attributed separately.

**Status.** The wire and same-user IPC surface are implemented: the terminal
attach/stream/detach message set, the console terminal registry and
multi-consumer fan-out, fail-closed routing, and the adversarial suite. M008 is
a draft milestone, and its front ends are experimental. The command-line
client `cretlink-cli shell` (ST-M008-002) is a same-user IPC consumer
beside the MCP bridge: it holds no session authority and adds no new listener,
endpoint, protocol variant, or trust anchor (REQ-N-012, REQ-S-023). The console
GUI's terminal panel (ST-M008-001) attaches to the hosted session in-process
rather than over IPC, fails closed outside a consented session, and writes the
untrusted VT stream to an xterm.js terminal. Its opening and closing are
recorded in the session's audit log as the `terminal` actor, like an IPC
terminal client's, and appear in the technician's activity feed
(ST-M009-011, REQ-S-023). Verified by the console IPC tests
(`apps/console/tests/terminal_ipc_e2e.rs`), the terminal registry unit tests
(`apps/console/src/service.rs`), a `cfg(windows)` real-ConPTY-over-IPC
end-to-end, and the ST-M008-003 adversarial consolidation suite for the operator
terminal surface (see below).

**Entry points.**

- Terminal messages from a local client into the console over the same-user IPC:
  `TermOpen` (shell, dimensions, working directory, environment), `TermAttach`,
  and — once attached, in the bounded duplex stream mode — `TermInput` bytes,
  `TermResize` dimensions, `TermDetach`, and `TermClose`.
- Reflected content pushed to attached consumers: the terminal's merged VT
  output (raw pass-through, ADR-0010), its honest exit, and fail-closed teardown
  frames — the channel through which hostile on-target terminal output reaches
  the local consumer, treated as untrusted data exactly like piped output.

**Threat actors.** A misbehaving or compromised local terminal client on the
technician computer (same-user, so it already shares the technician's authority);
an adversary with content on the customer computer who plants escape sequences or
instructions in terminal output; and a slow or wedged consumer whose backpressure
could otherwise stall the shared stream.

**Abuse cases.**

- *Authority escalation by attaching.* A local client attaches to a terminal
  hoping the attach itself widens what it may do, or reaches a session it was not
  granted.
- *Cross-consumer leakage.* One consumer's raw input is delivered to another
  consumer, or a consumer observes or influences a session that is not the one
  consented target.
- *Stale/forged handles.* Attach or input aimed at an unknown or already-exited
  terminal, or at a revoked session, hoping for a panic, a hang, or a silent
  success.
- *Backpressure denial of service.* A deliberately slow consumer refuses to read
  its stream, aiming to stall the shared VT stream and starve the other
  consumers, or to exhaust console memory with unbounded buffering.
- *Outcome masking on a lost link.* A disconnect or revoke is presented to a
  consumer as a clean terminal close rather than the honest, non-success outcome.

**Mitigations.**

- *Attach grants no new authority.* Attaching is a purely local registry
  operation on the already-consented session; it mints no session authority and
  never reaches the target. Input, resize, and close still route through the same
  fail-closed session surface as any other action, stamped `origin = terminal`
  (attribution only, no routing or consent effect).
- *Distinct attribution.* Terminal actions are audited as `Actor::Terminal`,
  frozen into the actor vocabulary by ST-M003-006 and kept distinct from the MCP
  bridge's `Actor::Mcp` (REQ-S-023), so the audit trail separates a technician
  terminal from AI-driven work. Terminal messages are excluded from the AI
  in-flight indicator.
- *Fail-closed routing.* With no consented active session — or after a revoke or
  stop — `TermOpen`/`TermAttach` fail closed with the explicit no-consented-session
  error, mirroring the bridge (ST-M004-004, REQ-S-009). A revoke while consumers
  are attached **actively** tears every attached stream down with an honest
  `Revoked` frame rather than leaving it to hang; a lost link yields an honest
  `Unknown` exit, never a fabricated clean close (REQ-N-008).
- *Multi-consumer without cross-talk.* Input from every consumer is muxed to the
  single pseudo-console; the shared merged output (including the pty echo) is
  fanned out to all consumers, but a raw input frame is never delivered to
  another consumer. There is no wire frame that carries one consumer's input to
  another.
- *Bounded everywhere, slow consumer disconnected.* At most `MAX_TERMINALS`
  concurrent terminals (enforced fail-closed by a reservation before the host
  round-trip) and `MAX_TERM_CONSUMERS` consumers per terminal. The VT stream is
  fanned out over **per-consumer bounded channels**, not a shared lossy
  broadcast: a consumer that overruns its window is disconnected with an honest
  `Dropped` frame, so it can never corrupt the shared stream by dropping bytes
  for the others (REQ-N-006). Every frame and input chunk stays within the
  existing IPC frame and chunk bounds.
- *Bounded duplex extension, same framing.* After a successful `TermAttach` the
  connection enters a duplex stream mode — a deliberate, well-scoped extension of
  ADR-0004's request/response turn-taking — layered on the **same** length-prefix
  framing. The non-cancel-safe read loop runs alone on the connection's read half
  so a frame is never torn by cancellation; the writer runs on the write half.
  The bridge's request/response connections are unaffected: only a `TermAttach`
  puts a connection into duplex mode.

**Operator-surface validation (ST-M008-003).** The operator terminal surface —
the `cretlink-cli shell` client and the console panel, both non-AI
technician front ends — is exercised by an adversarial consolidation
suite so its safety properties are pinned, not assumed:

- *Attach and open races.* Concurrent opens each receive a distinct terminal
  handle (no id collision that could cross-wire two operators), and concurrent
  attaches to one terminal all register as distinct consumers with none silently
  lost — every racing consumer still receives the shared stream.
- *Reattachment prevented after stop/revoke.* A revoke or stop tears the live
  stream down with an honest `Revoked` frame, and a fresh connection may **not**
  reattach to the same handle: it fails closed with the explicit
  no-consented-session error — not a stale-handle leak, not a silent success.
- *Stale handles refused.* Reattaching to a terminal that was valid but has
  since exited is refused with an honest `UnknownJob`, never a panic, hang, or
  fabricated success — even while the session stays consented.
- *Honest unknown on a lost link.* A link lost mid-terminal ends the consumer's
  stream with an honest `Unknown` outcome and no exit code, never a fabricated
  clean close (REQ-N-008).
- *Owning-session attribution.* Every terminal audit record carries the
  `Actor::Terminal` actor and the owning session id, so terminal activity is
  always attributable to the one consented session and never merged with AI/MCP
  work (REQ-S-017, REQ-F-016).

**Residual risks.**

- The terminal IPC transport shares the bridge's same-user IPC and therefore the
  same accepted Windows DACL-hardening residual risk (ADR-0004; see the AI-client
  section above). A same-user local client already shares the technician's
  authority by construction.
- Operator-facing rendering of the VT stream relies on the terminal emulator in
  each front end (xterm.js in the console panel, the host terminal for
  `cretlink-cli shell`); the contract passes raw VT bytes through as untrusted data,
  and escape-sequence handling is only as robust as that emulator.

### Command execution

CretLink's core capability is arbitrary elevated PowerShell and Command Prompt
execution on the customer computer (REQ-F-007), granted through one attended
full-control consent (REQ-F-006, REQ-S-001). This is a deliberate design
choice: the product does not attempt to semantically allowlist administrator
commands, because no such filter can reliably distinguish a legitimate repair
action from a harmful one when both use the same operating-system facilities.

The security boundary is therefore **not** "which commands may run". It is:

- **when** commands may run — only inside an active, consented,
  cryptographically paired session (REQ-S-001, REQ-S-007, REQ-S-009);
- **as whom** — with truthfully reported Windows identity and elevation
  (REQ-S-011);
- **under whose observation** — visible to the customer and technician at all
  times (REQ-S-002, REQ-F-017);
- **with what containment** — tracked job identifiers and process trees,
  bounded output, enforced timeouts, and cancellation (REQ-S-014, REQ-F-008,
  REQ-N-006);
- **until when** — revocable by either endpoint at any moment, with session
  authority invalidated on stop (REQ-S-004, REQ-F-018, REQ-S-007).

**Status.** Implemented (M003). The execution broker, the MCP execution tools,
and pairing are in place with the negative tests listed under verification
evidence below (REQ-N-010, REQ-N-011); the maintainer-run hardware scenarios in
[`manual-tests.md`](manual-tests.md) are partly recorded.

**Entry points.**

- The MCP bridge execution tools on the technician computer: `remote_exec`,
  `remote_write_stdin`, `remote_cancel`, `remote_list_jobs`.
- The execution-context arguments carried with each command — the explicit
  working directory and the constrained set of environment overrides
  (REQ-F-009) — AI-supplied values that shape where and how a job runs, and
  untrusted input like every other tool argument.
- The execution broker on the customer computer, which receives job requests
  over the authenticated encrypted channel and starts processes with the
  granted Windows authority.
- The returning stream of stdout, stderr, exit codes, and job metadata — an
  entry point in its own right, because remote output flows into the
  technician's AI context and must be treated as untrusted input.
- Later, an optional session-scoped SYSTEM helper (separately specified;
  REQ-S-012) — out of scope for the initial milestone.

**Threat actors.** A manipulated or compromised AI client (see the AI-client
section); a malicious technician who obtains genuine consent and then abuses
the granted authority; hostile code on the customer computer — processes
started by a job, or malware already present — that may try to confuse job
identifiers, forge or pollute output, or escape the tracked process tree; and a
network attacker, whose injection or replay of execution messages is mitigated
at the channel boundary (REQ-S-005, REQ-S-006, REQ-S-008) — the execution layer
assumes an authenticated channel and rejects anything else (REQ-S-009).

**Abuse cases and mitigations.**

*Execution outside the consent window.* A command runs before full-control
consent, after stop or revocation, or through a reconnect that silently
resumes a dead session. Mitigations: no privileged command may run before
consent (REQ-F-006, REQ-S-001); stop and revocation reject new tool calls and
invalidate session secrets (REQ-F-018, REQ-S-004, REQ-S-007); reconnection
cannot restore control without a valid session state, and all authorisation
failures fail closed with a visible error (REQ-S-009); the session state model
makes these transitions explicit and idempotent (REQ-N-004).

*Execution-identity lies.* The broker claims a command ran elevated when it
did not, or results are attributed to the wrong host or session — leading the
technician or AI to act on false premises, or masking a privilege failure as
success. Mitigations: the agent must report its effective Windows identity and
elevation state and must never claim elevation it does not have (REQ-S-011);
UAC elevation is requested visibly and the UI reflects the true broker state
(REQ-F-002); every result carries exit code, output, duration, and execution
identity (REQ-F-007); every tool result identifies the target and session, so
remote results cannot be mistaken for local ones (REQ-F-016).

*Process-tree escape.* A started command spawns work that detaches from the
tracked tree — for example by re-parenting or by launching processes through
operating-system facilities that create them outside the job — so that
cancellation, session stop, or cleanup misses them. In the worst case this
becomes unintended persistence beyond the session. Mitigations: the broker
must track the process tree created for each job and make a best effort to
terminate the correct tree on cancellation or revocation (REQ-S-014); it must
report whether a process continues after a client disconnect rather than
implying it stopped (REQ-F-008); active CretLink-owned processes and jobs are
identifiable through inspection (REQ-F-011); cleanup removes temporary
services and auto-start mechanisms, and cleanup failures are shown prominently
and recorded (REQ-F-019); the agent hides nothing from the customer
(REQ-S-002). Escapes that survive despite this are a residual risk and must be
reported as such, never as success (REQ-N-008).

*Job identifier confusion.* A cancel, stdin write, or stream request is
applied to the wrong job — through a stale, reused, or forged identifier —
terminating an unrelated process or crossing output between jobs. Mitigations:
the broker must prevent job identifier confusion (REQ-S-014); all execution
messages have explicit schemas, bounded inputs, and identifiers with version
handling (REQ-N-012); tool calls are routed only through the selected,
consented session (REQ-F-013, REQ-F-016).

*Hostile or ambiguous execution-context arguments.* A manipulated AI supplies
a misleading working directory or hostile environment overrides so that a
command acts somewhere, or on something, other than what the humans believe.
Mitigations: environment overrides are restricted to a constrained set rather
than arbitrary process-environment control (REQ-F-009); execution-context
arguments are schema-bounded like every other tool argument (REQ-N-012); and
each result reports the effective working directory (REQ-F-009 acceptance
evidence), so ambiguity is visible rather than silent.

*Output flooding.* A command emits unbounded output to exhaust memory or disk
on either endpoint, push earlier evidence out of the console's view, degrade
the session, or saturate the AI context with attacker-chosen text.
Mitigations: command buffers have documented resource limits and backpressure
behaviour (REQ-N-006); output is streamed incrementally with chunking, flow
control, and upper bounds rather than accumulated (REQ-F-008, REQ-N-005); the
audit trail records actions and digests independently of the live view
(REQ-S-017); the product warns that arbitrary administrator output can reveal
sensitive data, with redaction and retention controls planned (REQ-S-018,
SHOULD). Remote output is treated as untrusted input to the AI by
architectural rule.

*Timeout abuse.* An effectively unlimited timeout keeps a process running past
anyone's attention; or a process ignores termination at timeout, so "timed
out" silently means "still running". Mitigations: timeouts are configurable
and enforced (REQ-F-008); the console distinguishes completed, failed,
cancelled, timed-out, and unknown outcomes and never reports them as success
(REQ-N-008); long-running work remains visible in the live activity view
(REQ-F-017) and stoppable at any time from either endpoint (REQ-F-018,
REQ-S-004); whether a process survived is reported honestly (REQ-F-008).

*Cancellation failure.* Cancellation or emergency stop appears to succeed
while the process tree survives — the most dangerous failure mode, because
both humans believe the work has ended. Mitigations: termination is
best-effort but reporting is not — cancellation either terminates the intended
tree or states why it cannot (REQ-S-014, REQ-F-008); stop fails closed at the
authority level regardless: new tool calls are rejected and session secrets
are invalidated even when an existing process could not be killed (REQ-F-018,
REQ-S-007, REQ-S-009); surviving work is surfaced through inspection and
cleanup reporting (REQ-F-011, REQ-F-019, REQ-N-008).

*Authorised power used abusively.* Arbitrary elevated execution can
technically be turned toward credential theft, surveillance, or sabotage —
this is the line between authorised power and abuse. Mitigations: CretLink
ships no credential harvesting, keylogging, browser-session extraction, or
OS-authentication bypass features, and will not add them (REQ-S-013); consent
honestly describes the authority granted — install and remove software, change
system settings, access files, restart — and never implies read-only access
(REQ-S-003, REQ-F-006, REQ-N-013); the customer is told before consent that
commands, output, and files may reach the technician's AI service
(REQ-S-016); every action is visible to the customer (REQ-S-002), observable
in near real time by the technician (REQ-F-017), and recorded with session
identity, actor, arguments, and result in the local audit trail (REQ-S-017);
and the customer can end the session unilaterally at any moment (REQ-S-004,
REQ-F-018). The defence against abuse is attendance, visibility, attribution,
and revocation — not command filtering.

*Prompt injection through remote output.* Text produced on the customer
computer — file contents, process names, error messages — is crafted to
instruct the technician's AI to run harmful follow-up commands. The mechanics
and primary mitigations are covered in the AI-client section; at this boundary
the human technician remains in the loop with a live view of every action
(REQ-F-017) and an immediate stop (REQ-F-018), and the audit trail preserves
what was actually executed (REQ-S-017). CretLink does not claim to filter
injected instructions semantically; this is a shared residual risk with the
AI-client boundary.

*Privileged broker outliving the session.* The elevated broker, or a future
SYSTEM helper, persists past the session and becomes a standing privileged
service. In M003 there is no separate SYSTEM helper process: the "privileged
helper" is the **in-process elevated broker of the single-session agent**, so
its lifetime is the agent process's lifetime. Mitigations: any privileged broker
is scoped to the current session and removed or disabled during cleanup
(REQ-S-012); normal shutdown removes temporary credentials, services, and
auto-start mechanisms, and a post-session check must find no active access path
(REQ-F-019, REQ-F-001); session authority is ephemeral and invalidated on stop
or expiry (REQ-S-007). The reuse-after-closure claim decomposes into three
sub-claims verified concretely: a closed session is **not re-joinable** (the
fresh-join rejection in `console_revoke_tears_down_both_fail_closed`), a fresh
session requires **full pairing plus consent** — no resume path, per
`no_control_before_consent` and `happy_path_same_code_consent_and_heartbeats` —
and the **agent process ends after its session** (the single-session `run()`
returns and the broker is dropped — proven automatically by
`agent_run_loop_ends_after_a_single_session`, which drains the agent handle to
its close, the exact condition on which the binary `run()` returns and the
process exits, and confirms the console accepts no second connection; exercised
on hardware in [`manual-tests.md`](manual-tests.md) MT-3/MT-4). See also the
temporary-persistence section.

**Verification commitments.** Each milestone that delivers execution
capability must land with negative authorisation tests, cancellation and
revocation tests, cleanup tests, and an updated version of this section, as
required by the security acceptance evidence in the security requirements.
Routine testing uses a non-privileged simulated agent so containment and
cancellation logic is exercised without granting administrator control to CI
(REQ-N-011, REQ-N-010).

**Verification evidence (ST-M003-007).** The automated adversarial suite for
this boundary is green on every CI platform: revocation races are covered by
`revoke_mid_job_reports_an_honest_outcome_never_success`,
`revoke_mid_upload_is_honest_and_sweeps_staging`, and
`simultaneous_bilateral_revoke_terminates_both_without_hang`
(`crates/session/tests/e2e.rs`); cancellation reaping the tracked process tree
with zero survivors is covered by `exec_cancel_terminates_the_job_as_cancelled`
and the ConPTY containment tests (ST-M003-008); tree containment surviving a
hard `TerminateProcess` of the agent is covered by
`windows_hard_kill_reaps_the_whole_tree_via_kill_on_close` (ST-M006-007);
fail-closed teardown and a
non-re-joinable closed session by `console_revoke_tears_down_both_fail_closed`
and `agent_revoke_tears_down_both_fail_closed`; the agent run loop ending after
its one session (and no second connection accepted) by
`agent_run_loop_ends_after_a_single_session`; and honest disconnect by
`disconnect_mid_session_is_honest_never_success`, with its mid-transfer sibling
`disconnect_mid_upload_is_honest_never_success` proving a severed transport
mid-upload is `Revoked { Error }` and never a completed transfer. The
running-job policy on stop
(revoke → `cancel_all` plus a bounded drain; an unkillable process → `Unknown`;
both a graceful agent exit and a hard `TerminateProcess` reap the Job Object
tree — the latter via `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` since ST-M006-007; a
job never outlives its session) and the maintainer-run hardware scenarios are
recorded in [`manual-tests.md`](manual-tests.md).

**No credential-capture primitive (REQ-S-013).** The generic tool surface —
`remote_exec`, `remote_write_stdin`, `remote_cancel`, `remote_list_jobs`, the
file and transfer tools, and `remote_system_info` — exposes ordinary
computer-control operations only. It ships no keylogger, no password or
browser-session extractor, and no OS-authentication-bypass primitive, and none
will be added (REQ-S-013). Any credential theft an authorised, prompt-injected,
or malicious operator attempts must therefore express itself as ordinary,
visible, audited commands, not as a hidden capability of the product. This is a
statement backed by review of the tool surface, not a runtime test.

**Residual risks.**

- Containment is best-effort. An elevated process has the full power of the
  operating system and can create work outside any tracked tree. CretLink
  guarantees honest reporting of escapes and survivors (REQ-S-014, REQ-N-008),
  not their impossibility.
- No semantic command filtering. Within an authorised session, harmful
  commands are technically executable. The defence is attended consent,
  visibility, audit, and revocation — accepted by design.
- A malicious technician within a consented session. The customer's
  protections are attendance, the visible activity indication, and the stop
  control; CretLink cannot prevent misuse of authority the customer genuinely
  granted.
- The audit trail is held by the technician: it supports honest technicians
  and after-the-fact review, and is not tamper-proof against the person who
  controls the console (REQ-S-017).
- Executed effects cannot be revoked. Stop fails closed for future actions but
  cannot undo system changes already made, and an elevated command can in
  principle degrade or terminate CretLink itself on the customer computer.
- Further surfaces: the optional SYSTEM helper is not implemented (REQ-S-012);
  desktop control is experimental (M005 draft, REQ-F-020); reboot persistence
  is implemented as the single-use reboot-resume ticket (ADR-0014, REQ-F-021)
  and analysed under temporary persistence.

### File transfer and staging

File transfer moves bytes between the two computers: uploading
technician-provided scripts, utilities, installers, and drivers to the
customer computer; downloading files, logs, and results back to the technician
computer; and the general remote file operations (stat, directory listing,
directory create and remove, move, and delete) exposed to the AI. Reading and
writing file *content* is deliberately not a standalone AI tool: bytes cross
the boundary only through the bounded upload and download transfer engine,
never as tool-call payloads.

**Status.** Implemented on `main`. The transfer and file-operation subsystem
(ST-M003-004, ST-M003-005) and its AI-facing MCP tools (ST-M004-003) have
landed with automated tests, a simulated-agent end-to-end run in CI (upload,
execute, download, and cleanup), and a real-elevated-agent-on-hardware
end-to-end run on maintainer hardware recorded on 2026-08-10, so all three
stories are Done. The automated half of the privileged adversarial and negative
suite (ST-M003-007) that hardens this surface with revocation, race, and
hostile-input tests has landed — see the per-abuse-case evidence below
(`revoke_mid_upload_is_honest_and_sweeps_staging`,
`cleanup_failure_is_reported_then_retried_at_next_start`,
`staging_path_traversal_is_rejected_at_the_mcp_surface`) — while its
maintainer-run hardware half ([`manual-tests.md`](manual-tests.md)) is still
outstanding.

The operations the technician's file explorer adds (ST-M010-001, REQ-F-028) —
the list of drives, the removal of a folder with its content, and a file sent
into a folder on any drive — have landed with automated tests on every CI
platform. What only Windows has (junctions and other reparse points, a path
that leads through one, a folder that is held while it is removed, an open
and a rename inside an opened folder, a file held open by another program, a
second drive) is covered by tests that run on the Windows CI runner only; the
run on real hardware is scenario MT-14 in
[`manual-tests.md`](manual-tests.md), not yet recorded. The technician reaches
these operations through the console library. The AI tool surface does not
contain them.

The file explorer in the Files tab of the technician's window (ST-M010-002)
has landed in both of its parts. It browses — the list of drives and the
listing of a folder (FT-12) — and it changes: files and folders are sent to
the open folder, the selection is copied to the technician computer, and an
entry is renamed or deleted (FT-13). Both have automated tests on every CI
platform. A folder is found by its path only where the wire accepts such a
path, so the tests that open a real folder by a typed path, and those that
send, copy, rename and delete in one, run on the Windows CI runner only. On
every platform the technician's calls run through a real session in the
staging area, and the transfer queue and the changes of the window are
tested against a customer computer held in memory. Its run on real hardware
is scenario MT-14, not yet recorded.

The four operations that change one entry of a folder — delete a file, remove
an empty folder, create a folder, move or rename (M003; the AI tools
`remote_delete`, `remote_remove_dir`, `remote_make_dir` and `remote_move`) —
take up the folder that holds the entry as a verified folder on Windows
(ST-M010-004, FT-11). Their tests with junctions and with a folder that is
swapped run on the Windows CI runner only; the run on real hardware is steps
15 to 17 of scenario MT-14, not yet recorded.

**Entry points.**

- The MCP bridge file and transfer tools on the technician computer
  (`remote_stat`, `remote_list_dir`, `remote_make_dir`, `remote_remove_dir`,
  `remote_move`, `remote_delete`, `remote_upload`, `remote_download`,
  `remote_list_staged`, and `remote_cleanup`), driven by an AI client whose
  output is untrusted input. There is deliberately no file-content read or
  write tool; content crosses only through `remote_upload` and
  `remote_download`.
- The technician's own file actions in the console library
  (`TechnicianFiles`, ST-M010-001, ST-M010-002): the list of drives, the
  listing of a folder, the removal of a folder with everything in it, a file
  sent into a folder on any drive, a file copied to the technician computer,
  and the changes to one entry (a new folder, a move or rename, a delete, a
  look-up). Their requests carry the origin `console`; they are not MCP
  tools. The drive list and the removal are wire operations of their own
  (`fs_drives`, `remove_tree`), which an agent serves only when it advertises
  the capability `fs.v2`; the others use the operations the AI tools use,
  under the technician's name.
- The commands of the technician's window that drive those actions
  (`explorer_*` in `apps/app`, FT-12 and FT-13). They take text from the
  window — a folder and names on the customer computer — and write files on
  the technician computer under names that the customer computer gives.
- The file explorer in the Files tab of the technician's window
  (ST-M010-002): the commands `explorer_drives` and `explorer_open` of the
  application's backend, registered in a technician process only. They call
  `TechnicianFiles` for the list of drives and for the listing of a folder
  (`fs_list` with the origin `console`). `explorer_open` takes a path as text
  from the window; the window shows names, labels and paths that the customer
  computer supplied.
- The chunked transfer messages carried inside the authenticated encrypted
  session channel.
- The file and transfer functions of the elevated execution broker on the
  customer computer.
- Technician-side staging sources (local disk, removable media) and the
  technician-side download destination directory, which receives content
  originating from the untrusted customer machine.

**Threat actors.** A malicious or manipulated AI client issuing hostile paths,
oversized requests, or prompt-injection-induced requests; a LAN attacker
attempting to corrupt, truncate, reorder, or inject transfer chunks or
substitute a staged tool in transit; a malicious peer (rogue console or rogue
agent) lying about transfer outcomes, supplying hostile filenames and
metadata, or streaming unbounded data; a malicious operator of either endpoint
using the transfer surface to exfiltrate customer files or stage malware; and
the remote filesystem itself as untrusted input — downloaded content and
remote-supplied names, paths, and sizes may be crafted to confuse the console,
the audit log, or the technician.

**Abuse cases and mitigations.**

*FT-1 — Path traversal and path confusion.* Hostile relative paths, `..`
segments, drive-relative forms, NTFS alternate data streams, or Unicode tricks
could aim writes or deletes at unintended locations. Two distinct cases
matter. On the **customer** side, full-control consent deliberately authorises
access to arbitrary in-scope paths (REQ-F-006, REQ-S-003), so traversal there
is not a sandbox escape; the risks are ambiguity and misattribution, and where
a constrained staging area exists, traversal out of it must be rejected. On
the **technician** side, a remote-supplied filename or path escaping the
chosen download destination would let untrusted customer-machine content
overwrite technician files — the more direct traversal target. Mitigations:
bounded, schema-validated path fields with explicit local/remote semantics
(REQ-N-012; the architecture requires that transfer functions never pretend
remote paths are local); canonicalisation and rejection of paths that escape a
constrained staging or download area, proven end to end over the MCP surface by
`staging_path_traversal_is_rejected_at_the_mcp_surface`
(`apps/console/tests/mcp_bridge_e2e.rs`, rejected as `invalid_path`) and backed
by the protocol-level `validate_staging_name` and the store's own
`staging_reference_cannot_escape_root` guard (REQ-N-010); every result carries
session and target identity
so a path is never applied to the wrong machine (REQ-F-016); malformed or
ambiguous paths fail closed with an actionable error (REQ-S-009, REQ-N-015);
destructive operations record the affected remote path in the audit trail
(REQ-F-010, REQ-S-017).

*FT-2 — Tampered or substituted content and digest mismatches.* An attacker
who could modify chunks in transit, or a buggy or lying peer, could cause the
executed artifact to differ from the artifact the technician staged.
Mitigations: all transfer traffic runs inside the authenticated encrypted
channel built from established cryptographic protocols and libraries
(REQ-S-005), with endpoint authentication bound to the SAC (REQ-S-006) and
replay and downgrade resistance (REQ-S-008); independently of the channel,
each transfer verifies length and a cryptographic digest before it may be
reported complete (REQ-N-007), staged artifacts are verified by digest before
execution (REQ-F-012), and digests are recorded in the audit trail so what ran
can later be tied to what was staged (REQ-S-017). A digest mismatch is a hard
failure: the transfer is not reported complete and the error identifies the
failed stage (REQ-S-009, REQ-N-015).

*FT-3 — Partial or corrupted transfers reported as complete.* A dropped
connection, cancelled job, revoked session, or crash mid-transfer could leave
a truncated file that a later step treats as valid — particularly dangerous
when the file is an installer or driver about to be executed. Mitigations:
completion is only reported after both length and digest match (REQ-N-007;
ST-M003-005 acceptance criteria); network interruption or process failure must
never be reported as success, and outcomes distinguish completed, failed,
cancelled, timed out, and unknown (REQ-N-008); transfers support chunking,
progress, and cancellation with documented behaviour for interrupted transfers
(REQ-F-010, REQ-N-006); revocation and stop fail closed rather than leaving a
transfer in a claimed-successful state (REQ-S-004, REQ-S-009, REQ-F-018) —
`revoke_mid_upload_is_honest_and_sweeps_staging`
(`crates/session/tests/e2e.rs`) drives a revoke into a mid-flight upload and,
observing the in-process agent's real staging root, asserts that the interrupted
part is created mid-transfer, that the transfer never reports success and
publishes no final path, and that after the agent's autonomous session-end sweep
the orphaned part is gone and no committed final file exists (a commit is the
only route to a final name, so this is a filesystem-level "no torn file" check);
its hard-disconnect sibling `disconnect_mid_upload_is_honest_never_success`
proves the same honest outcome when the transport is *severed* mid-upload rather
than gracefully revoked — the console surfaces a terminal `Revoked { Error }`,
the upload never reads complete, and no committed final file is published;
an error on the technician's own file (a download's missing destination folder,
an upload's missing source, denied access, a full disk) ends that transfer as
failed in the stage `local`, with the local path and the reason, and is never
reported as a lost session, so the reported reason is the true one and the
technician can act on it (REQ-N-008, REQ-N-015, ST-M003-005) —
`download_into_a_missing_local_folder_is_failed_and_the_session_stays_usable`
and `upload_of_a_missing_local_file_is_failed_and_the_session_stays_usable`
prove it with a command that still runs afterwards, and on Linux
`download_onto_a_full_disk_is_failed_and_the_session_stays_usable` proves that
a write the disk refused fails the download instead of publishing a short file;
partial-write atomicity or recovery behaviour is documented per operation
(ST-M003-004).

*FT-4 — Staging of malicious tools.* The staging capability (REQ-F-012) is
genuinely dual-use: the same mechanism that delivers a legitimate driver can
deliver malware. CretLink does not try to classify staged binaries; it makes
staging attributable, visible, and reversible. Mitigations: staging is only
possible inside an attended session that the customer visibly initiated and
consented to, with consent language that explicitly covers installing and
removing software (REQ-S-001, REQ-S-003, REQ-F-006, REQ-N-013); the agent
hides nothing about its activity from the customer (REQ-S-002) and transfers
appear in the live activity view (REQ-F-017); the source, resulting remote
path, and digest of each staged artifact are audited (REQ-F-012, REQ-S-017);
staged files are associated with the session and covered by cleanup, with
cleanup failures reported rather than suppressed (REQ-F-019); CretLink adds no
hidden persistence and does not bypass or disable endpoint security, so staged
malware remains subject to the platform's own defences (product non-goals;
REQ-S-012).

*FT-5 — Exfiltration of sensitive files.* After consent, downloading
arbitrary readable files is an authorised capability, and downloaded content
may additionally be forwarded to the technician's AI provider. The threat is
abuse of that authority — bulk copying of documents, mail stores, or
credential databases — and the customer's inability to assess each individual
read. Mitigations: the pre-consent disclosure states that selected files may
be sent to the technician computer and on to the AI service (REQ-S-016);
CretLink ships no credential-harvesting, browser-session-extraction, or
OS-authentication bypass tooling, and ordinary file access remains visible and
audited rather than covert (REQ-S-013); transfers are shown in near real time
(REQ-F-017) and recorded with paths, sizes, and digests (REQ-S-017); the
customer can stop the session at any moment and stop fails closed (REQ-F-018,
REQ-S-004); session authority is ephemeral, so exfiltration cannot silently
continue after the session ends (REQ-S-007); the console warns that arbitrary
administrator access can reveal sensitive data, with redaction markers and
retention configuration planned for what it logs locally (REQ-S-018, SHOULD).

*FT-6 — Resource exhaustion via huge or unbounded transfers.* A hostile peer,
a runaway AI loop, or a malformed length field could exhaust memory through
oversized chunks, fill the disk of either endpoint, or starve the session
channel so that the stop control becomes sluggish. Mitigations: transfers
require chunking, flow control, cancellation, and documented upper bounds by
design (architecture transport constraints; REQ-N-006); all transfer messages
have explicit schemas with bounded fields and declared lengths, so an
oversized or inconsistent frame is rejected rather than buffered (REQ-N-012);
buffers are bounded with backpressure (REQ-N-006); transfers are cancellable
and the emergency stop remains immediately effective (REQ-F-010, REQ-F-018);
adversarial oversized-input tests cover this end to end
(`crates/mcp/tests/bridge_mock.rs`): `oversized_command_is_a_tool_error_not_a_panic`,
`oversized_stdin_is_rejected_before_decoding`,
`oversized_upload_source_label_is_rejected`, and
`oversized_absolute_path_is_rejected`, plus the protocol field-limit suite
(`crates/protocol/src/tests.rs`) — so an oversized or inconsistent frame is
rejected rather than buffered (REQ-N-010, ST-M003-007).

The size of one answer is bounded by the channel as well (ST-M010-005). The
secure channel sends a message in one record of `MAX_RECORD_BYTES` (65519)
bytes and splits nothing, so an answer whose encoding is longer cannot be
sent. A folder page was bounded by its number of entries alone
(`MAX_DIR_PAGE`, 256): a folder of long names — which any program on the
customer computer can make — gave a page that did not fit, and the agent took
the failed send for a lost connection and ended the session. Two things
changed:

- *A folder page fits.* The agent ends a page before the entry that would
  make its answer larger than one record, and says that more follows; the
  listing continues after the last name, as it always did. The size is the
  one the answer has when it is encoded, with the longest request identifier,
  so a name that the encoding writes longer (a quotation mark or a backslash
  as two bytes, a control character as six) is counted as it is written. The
  largest single entry the protocol carries is a small part of a record, so
  a page that says more follows always holds an entry and a listing always
  gets further.
- *An answer that cannot be sent fails its request, not the session.* An
  answer that is larger than one record, or that breaks a bound of the
  protocol, is refused before anything of it reaches the wire. The agent
  then fails that one request with the reason (`oversized_message`, or the
  category of the bound), in fixed words that hold nothing of the answer
  (REQ-N-015), and serves the next request. This holds for every kind of
  answer. Only a channel that is really gone ends the session. The customer
  sees such a request as failed.

Evidence: `the_counted_size_is_the_size_of_the_real_encoding`,
`the_real_answer_is_never_longer_than_counted`,
`a_page_ends_before_the_entry_that_would_not_fit`,
`the_largest_single_entry_always_fits` and
`no_character_is_written_longer_than_six_bytes_for_one`
(`crates/protocol/src/page/tests.rs`);
`a_folder_of_long_names_is_listed_in_pages_that_fit_one_record`,
`names_that_the_encoding_writes_longer_are_counted_as_they_are_written` and
`an_entry_that_fits_no_page_fails_the_listing_instead_of_ending_a_page_empty`
(`crates/exec/src/fs/tests.rs`);
`an_answer_larger_than_a_record_fails_its_request_and_the_session_goes_on`,
`an_answer_that_breaks_a_bound_fails_its_request_and_the_session_goes_on`
and `a_frame_refused_before_the_wire_is_told_apart_from_a_lost_channel`
(`crates/session/src/runtime.rs`); and, through a real session,
`a_folder_of_long_names_is_listed_through_a_session_every_name_exactly_once`
(600 files; names of 250 characters of three bytes each on Windows, and of 85
such characters, the longest a name can be, elsewhere) and
`an_answer_too_large_to_send_fails_its_request_and_the_session_goes_on`
(`crates/session/tests/e2e.rs`).

*FT-7 — Another program on the customer computer uses CretLink's own
folders against the elevated program (ST-M006-020).* The staging area is
where the elevated program acts on its own records: a cleanup removes the
files that a manifest names, the startup sweep does that for every session
folder it finds, and a staged tool is run. Up to release 0.24.0 the staging
area and the store of reboot tickets lay in `CretLink` under the machine's
application data (`ProgramData`), where every account can create files and
folders, and the program took the location of that folder from its
environment. A program of an account without administrator rights could put
a folder named like a session there, with a manifest that names any file,
and the next elevated start removed that file. It could also open a folder
there and keep it open: Windows checks rights when an entry is opened, so
closing the folder to others afterwards takes nothing away from a handle
that is already open. Mitigations:

- *A place only administrators can write, private from its first moment.* An
  elevated program keeps the staging area and the store of reboot tickets in
  `CretLink Data` in `Program Files` on the drive of the Windows folder. Only
  administrators can create or move anything into `Program Files`, so
  nothing can be waiting there. The program creates each of its folders with
  its owner and its permissions in one step: owned by the administrators,
  with permissions of its own that give full control to the administrators
  and the system and nothing to anyone else, for the folder and for
  everything made in it later. So nobody can hold such a folder open from
  before, and nothing in it was put there by anyone else. A folder that is
  already there is opened without following a link, and through that one
  handle its kind, its owner and its permissions are read: it must be such a
  private folder of the administrators already. Nothing is made private
  afterwards: whoever opened a folder while it was open keeps what that
  handle allows. Anything else under one of those names (a link, a file, a
  folder of another owner, a folder with other permissions) is an error: the
  program then keeps nothing, stages nothing, and says why. All of this
  happens before any staging operation, before a sweep reads, and before a
  ticket is written.
- *The place is not the starter's choice.* The drive is asked from the
  system. Nothing is read from the environment, which whoever starts the
  program can choose. `Program Files` itself must be a folder that Windows,
  the system or the administrators own.
- *What earlier releases left is never read.* The start that sweeps removes
  the old folder under the machine's application data whole, without reading
  anything in it and without following a link. `recover` names it and
  `recover --remove` removes it. That includes files which a session of an
  earlier release kept there on purpose (cleanup policy *keep*): a file in
  that folder can no longer be told from a planted one. An elevated start
  removes the folder unasked only when the administrators own it, as they do
  when an elevated program made it. A folder there that an account owns is
  named and left to `recover --remove`, or to a start from that account:
  every account can make that folder again and fill it. A program of release
  0.23.0 or 0.24.0 that still runs holds the lock of its staging area; the
  folder is then left until that program has ended, and the start says so.
  Older releases have no such lock: a session of such a release that runs at
  that moment loses what it staged.
- *What a manifest holds is still not trusted.* Whatever a manifest names, a
  cleanup removes a file only when the entry ends in the session's own folder
  and a file name, and addresses it through the staging folder as the program
  knows it, never through the path the manifest spells. Every other entry is
  reported and left alone, and the purge drops the entry. A manifest and a
  ticket store are read up to a bound, and their temporary files are made
  new, never written through what stands under their name.
- *Links are never followed.* A link (on Windows also a junction) under the
  name of a session folder, of its manifest or of its folder of parts is
  never read, entered or followed, also in the staging area of a program
  without administrator rights. Every start that sweeps names it, and so does
  `recover`; `recover --remove` removes the link and leaves what it points
  at.
- *An administrator, or nothing.* A process that is elevated without being a
  member of the administrators cannot make anything private. It keeps
  nothing and stages nothing.
- *Two keepers.* A program without administrator rights keeps its staging
  area and its tickets in `CretLink` in the account's own local application
  data, where no boundary is crossed. A program works only in the place of
  its own kind: an elevated program never reads the place in a profile, and a
  program without rights cannot read the other. `recover` covers the place
  of the kind of program that runs it. Run without administrator rights, it
  says that there is something it cannot see when the other place exists,
  and never calls the computer clean then; run with them, it says that the
  folders of the accounts are not covered. The notice at a start says
  whether the recovery command is to be run as administrator.
- *The shells are the ones of Windows.* A job starts `powershell.exe` and
  `cmd.exe` by their full path in the system folder, and the file explorer by
  its full path in the Windows folder; the system names both folders.
  Windows looks for a program that is named without its folder in the folder
  of the running program first. For a downloaded CretLink that is a folder
  its account can write, so a program of that name planted there would have
  been started with the rights of the session.
- *A job starts in the system folder.* A job or a terminal whose request
  names no working directory starts in the system folder, where only
  administrators can put anything, and the report of the job names that
  folder. It does not start in the working directory of the CretLink
  program, which whoever started the program chose: Command Prompt looks
  there first for a program that a command names without its folder. An
  elevated customer program also leaves the working directory it was started
  in before it sweeps and connects, so that Windows does not look there for a
  library and nothing it starts later begins there.
- *Nothing is moved in.* A move keeps a file as it is, with its permissions
  and with every handle a program opened on it before. An elevated program
  therefore refuses to move a file from elsewhere into its staging area,
  whether the request names the staging area by a staging name or by its
  full path: where the file would lie decides. A file gets there by an
  upload, which makes it new.
- *What is moved out takes the permissions of where it arrives.* For the same
  reason a file that an elevated program moves from its staging area to
  another place would stay closed to everybody but the administrators, the
  customer included. It is given the permissions that the folder it arrives
  in hands down, as if it had been made there, also when the request names
  the staged file by its full path; if that fails, the move is answered as
  failed and says so.
- *Said, not passed over.* A session whose cleanup failed, a staging area
  that could not be made private (with the reason), a folder of an earlier
  release that is still there, and a lock that another program holds for
  good are named by the start. The inventory calls what it could not read
  unknown, never empty, also the place of an earlier release when the system
  does not name it; the recovery counts as removed only what a second look
  no longer finds (REQ-N-008).

Evidence: `a_planted_manifest_removes_nothing_outside_the_staging_area`,
`a_cleanup_removes_only_what_lies_directly_in_its_staging_folder`,
`an_entry_stands_for_the_file_in_the_sessions_own_folder_however_it_is_spelled`,
`a_private_base_serves_a_session_from_its_first_file_to_its_cleanup`,
`a_link_in_the_place_of_a_session_folder_is_named_and_never_read`,
`a_manifest_that_is_a_link_is_not_read`,
`a_folder_of_parts_that_is_a_link_is_not_swept`,
`a_private_base_that_is_a_link_is_not_used`,
`a_private_base_behind_a_link_is_not_used`,
`the_temporary_file_of_a_manifest_is_never_written_through_a_link`,
`a_manifest_larger_than_a_manifest_can_be_is_not_read`,
`a_session_with_a_damaged_manifest_is_named_and_never_called_clean`,
`a_sweep_and_a_census_make_nothing_where_nothing_was_kept`,
`nothing_is_moved_into_a_private_staging_area_from_elsewhere` and, on Windows,
`a_file_moved_out_of_a_private_staging_area_takes_the_permissions_of_its_new_folder`
(`crates/exec/src/fs/tests.rs`); the rule about private permissions, the
chain of folders, and what happens when something else stands in the place
of a folder (`crates/exec/src/own/tests.rs`);
`a_shell_is_a_program_in_the_system_folder` and
`a_job_starts_in_the_folder_it_names_and_otherwise_in_the_system_folder`
(`crates/exec/src/system.rs`);
`default_working_dir_is_the_system_folder_on_windows`
(`crates/exec/src/broker/tests.rs`);
`the_folder_of_an_earlier_release_is_named_and_the_recovery_removes_it_whole`,
`links_in_the_folder_of_an_earlier_release_are_never_followed`,
`what_a_program_with_administrator_rights_kept_is_said_to_be_out_of_reach`,
`a_recovery_that_left_nothing_leaves_no_empty_folder_behind`,
`an_earlier_release_whose_place_cannot_be_told_is_unknown_not_absent`,
`what_is_out_of_reach_is_said_also_when_something_else_was_kept`,
`a_manifest_entry_outside_the_staging_area_is_reported_and_never_removed`,
`a_ticket_store_behind_a_link_is_not_read_and_nothing_behind_it_is_removed`
and `a_ticket_file_that_is_a_link_is_not_read_and_not_written_through`
(`apps/support-agent/src/inventory/tests.rs`);
`a_start_that_sweeps_removes_the_folder_of_an_earlier_release`,
`a_start_removes_unasked_only_what_no_program_uses_and_what_administrators_own`,
`work_on_a_thread_of_its_own_is_waited_for_only_so_long`,
`what_a_start_kept_is_removed_by_a_run_of_its_own_kind` and
`a_lock_held_for_good_does_not_keep_the_program_from_starting`
(`apps/support-agent/src/recovery.rs`). The owner, the permissions and
junctions exist on Windows only, and their tests run on the Windows CI
runner, which is an elevated administrator:
`a_folder_is_private_from_the_moment_it_is_made`,
`a_new_private_folder_belongs_to_the_administrators_and_is_closed_to_everyone_else`,
`a_folder_of_the_administrators_with_other_permissions_is_an_error_and_is_left_as_it_is`,
`whose_a_folder_is_is_read_from_its_owner_and_never_through_a_link`,
`a_folder_of_another_owner_is_an_error_and_is_left_as_it_is`,
`a_junction_in_the_place_of_a_private_folder_is_an_error_and_never_followed`,
`removing_whole_takes_a_junction_and_never_what_it_points_at`,
`a_held_lock_says_that_an_earlier_release_still_runs`,
`the_places_are_on_the_drive_of_the_windows_folder` and
`the_real_place_of_an_elevated_program_is_made_private_and_removed_again`
(`crates/exec/src/own/tests.rs`), with
`a_junction_in_the_place_of_a_session_folder_is_named_and_never_read`,
`a_folder_of_parts_that_is_a_junction_is_not_swept`,
`a_private_base_that_is_a_junction_is_not_used` and
`a_private_base_behind_a_junction_is_not_used`
(`crates/exec/src/fs/tests.rs`),
`the_shells_are_found_where_the_system_keeps_them`
(`crates/exec/src/system.rs`),
`a_job_is_a_shell_from_the_system_folder_and_starts_there`
(`crates/exec/src/platform.rs`),
`a_folder_that_an_earlier_release_still_uses_is_left_and_named`
(`apps/support-agent/src/recovery.rs`),
`an_elevated_windows_program_does_not_follow_its_environment` and
`in_verification_an_elevated_recovery_removes_the_folder_of_an_earlier_release`
(`apps/cli/tests/recover.rs`). No test acts as an account without
administrator rights: that, and the environment of an elevated start, is
the run on real hardware, scenario MT-4b in
[`manual-tests.md`](manual-tests.md), not yet recorded.

*FT-8 — Removal of a folder with its content (ST-M010-001).* One operation
removes a whole folder tree, and what it removes is gone: there is no recycle
bin. Beyond a wrong path, four things can go wrong. The removal can be led to
another place: the agent is elevated, the folder may belong to an account that
is not, and a program of that account can plant a junction or a symbolic link
in the folder, swap a folder for a link while the removal is on its way to it,
or swap the folder itself or a folder above it for a junction — before the
removal begins or between two of its steps — so that the removal reaches the
system folders or another user's files. The removal can hold the agent: the
agent serves one request at a time, so a removal that ran to its end inside
one request, or a step that a program keeps busy by creating and deleting
entries, would keep a stop by the customer waiting. A removal that left
something behind, or a folder that was only renamed away, can be reported as
done. And an answer that names every entry it could not remove can outgrow
what the channel carries, or carry a file name made to forge a line in the
activity view or the audit log. Mitigations:

- *Consent, attribution and visibility.* The operation exists only inside an
  attended session with full-control consent, whose disclosure names reading,
  modifying and deleting files (REQ-S-001, REQ-S-003). Every request carries
  its origin, and the customer's activity shows the removal as one item with
  its kind and its path: in progress from the first step that leaves more to
  do, then done, cancelled or failed (REQ-F-017).
- *Audit.* The agent records `fs_remove_tree` with the honest outcome when a
  removal ends, and `fs_remove_tree_start` when a removal that takes more
  than one step begins. The console records `console_remove_tree_start`
  before the first request and `console_remove_tree` after the last, with the
  path, the number of files and folders removed, the number of entries left
  behind and the first of them with its reason (REQ-S-017).
- *The folder is the one that lies at the path.* On Windows a step opens the
  folder once, without following a link in the last part of its path, reads
  from the opened handle where the folder really lies, and compares that with
  the path it was asked, without regard to letter case and with short names
  written out. A path that reaches the folder through a junction or a
  symbolic link does not compare equal and is refused as `invalid_path`
  before anything is removed; so is a path on a drive made with `subst` and
  one on a network drive, whose real location is not the path that was asked.
  The step then holds the folder: for as long as it runs, nobody else renames
  or removes it, nor a folder above it. It asks for the right to remove the
  folder with that. A folder that another program has open in a way that
  excludes its removal, its working folder for example, is held with the
  right to list it instead, and is opened once more at the end and removed
  only if it is still the folder that was held; a folder that can be held in
  neither way is not taken up. A link in the place of the folder itself is
  removed as a link, by the first step of a removal only. The root of a drive
  is refused, judged from the path and again from where the opened folder
  lies, and the path of the request passes the same rules as every remote
  path on both ends (FT-1).
- *One removal, one folder.* Between two steps the agent holds nothing, and
  the folder could be swapped for another. An answer that says more remains
  therefore names the folder by its identity — its volume and its file
  identifier, at most `MAX_FOLDER_ID_BYTES` (32) bytes — and the console gives
  that back with the next request. A step that finds a folder with another
  identity at the path, or a link, refuses with `invalid_path` before it
  removes anything. Both ends validate the rule: the identity is present
  exactly when more remains.
- *Never out of the folder.* An entry that stands for another place — a
  symbolic link, a junction, a volume mount point — is removed as that entry
  and never entered; what it points at is not looked at. On Windows nothing is
  removed by its name alone: every entry is opened without following a link,
  what it is and where it really lies are read from the opened handle, an
  entry whose real location is not inside the folder is refused as
  `invalid_path`, and the removal goes through that same handle. The check and
  the removal are about one object, so a swap between them changes nothing.
  The paths of the entries are built from the folder's verified location in a
  form that Windows does not rewrite, so a name that ends in a dot or a space,
  or the name of a device such as `nul`, names the entry of that very name.
  The folder itself is removed through the hold the step has on it, or,
  where it is held without the right to remove it, through a second open
  that is compared with the folder that was held.
- *Bounded steps.* One request meets at most `MAX_TREE_STEP_ENTRIES` (4096)
  entries — one it removes, one it cannot remove, one that is gone by the time
  it reaches for it, a folder it enters — and answers after
  `MAX_TREE_STEP_MS` (one second) once it has met an entry, whether or not it
  removed anything. A program that keeps creating and deleting entries in the
  folder therefore cannot keep a step going. The bounds are looked at between
  two entries: one request that the operating system keeps waiting is not
  ended by them (see the residual risks). A step that ends at a bound
  without having removed anything says so (`blocked`) and names what stood in
  its way. The console asks again while the answer says that more remains, at
  most a million times; then the removal ends as failed. The agent keeps
  nothing between two steps, so there is no state to tear down when the
  session ends. A folder nested deeper than `MAX_TREE_DEPTH` (256) is reported
  and left. The answer names at most `MAX_TREE_FAILURES` (16) entries, each
  path cut to `MAX_TREE_FAILURE_PATH_BYTES` and kept on one line, counts the
  others, and always fits one record of the secure channel (REQ-N-006).
- *Stop and cancel.* Between two steps the agent serves whatever arrives, so a
  stop or a revocation from either side takes effect within one step
  (REQ-S-004, REQ-F-018). A cancellation is the console no longer asking; it
  tells the agent that the removal ended, so the customer's item ends as
  cancelled and does not stay in progress.
- *Honest outcome.* Only a folder that the step itself removed is reported as
  removed; one that is gone from its path for another reason is reported as
  not found. A removal that left an entry behind is a failure that names it,
  a cancelled one says what it had removed, and one cut short by the end of
  the session is never recorded as done: the agent records its result as
  unknown. The console treats an answer that contradicts itself — more to do
  but nothing removed or no folder named, gone but with failures — as an
  error, never as a result (REQ-N-008).
- *Capability gate.* An agent that does not advertise `fs.v2` answers
  `unsupported` before it lists or removes anything (REQ-N-012).

Evidence: `links_in_the_folder_are_removed_and_never_followed`,
`a_link_in_the_place_of_the_folder_is_removed_as_a_link`,
`a_continuation_in_another_folder_is_refused_and_removes_nothing`,
`a_step_that_leaves_more_to_do_names_its_folder_and_no_other_step_does`,
`a_step_stops_at_its_entry_bound_and_the_next_one_gets_further`,
`a_step_counts_every_entry_it_meets_and_ends_at_its_bound`,
`entries_that_are_gone_when_the_walk_reaches_them_count_toward_the_bound`,
`a_step_ends_at_its_time_bound_although_it_removed_nothing`,
`a_folder_that_vanishes_during_the_step_is_never_reported_as_removed`,
`what_cannot_be_removed_is_named_and_the_rest_is_removed` and
`a_drive_is_not_a_folder` (`crates/exec/src/fs/tree/tests.rs`), with
`a_folder_reached_through_a_junction_is_refused_and_its_target_keeps_everything`,
`a_parent_swapped_for_a_junction_between_two_steps_is_refused`,
`the_folder_cannot_be_renamed_away_while_a_step_holds_it`,
`a_folder_another_program_holds_is_taken_up_and_reported_in_use`,
`a_folder_held_without_the_right_to_remove_it_is_not_renamed_away`,
`a_folder_above_a_held_folder_is_not_renamed_away`,
`names_that_win32_would_rewrite_are_removed_as_plain_names`,
`a_junction_in_the_folder_is_removed_and_never_followed`,
`a_junction_in_the_place_of_the_folder_is_removed_as_a_link` and
`a_file_held_open_by_another_program_is_named_as_in_use` on Windows;
`a_folder_reached_through_a_junction_is_refused` and
`another_spelling_of_the_real_location_is_accepted` on Windows
(`crates/exec/src/fs/anchor/tests.rs`);
`every_step_names_the_folder_the_step_before_answered_with` and
`a_removal_that_never_ends_is_given_up_after_a_bounded_number_of_steps`
(`crates/session/src/files/tests.rs`);
`a_folder_swapped_between_two_steps_is_not_removed_in_its_place`,
`a_cancelled_removal_stops_between_steps_and_says_so_on_both_sides`,
`a_revoke_during_a_removal_never_reads_as_a_success`,
`a_removal_that_leaves_something_behind_is_a_failure_with_its_reasons` and
`without_the_capability_the_explorer_operations_are_unsupported`
(`crates/session/tests/e2e.rs`);
`only_a_removal_that_continues_names_its_folder`,
`a_folder_id_is_bounded_and_never_empty` and
`the_largest_drives_and_tree_removal_answers_fit_one_secure_record`
(`crates/protocol/src/tests.rs`); and
`a_stop_ends_a_removal_and_later_actions_fail_closed`
(`apps/console/tests/technician_files_e2e.rs`).

*FT-9 — A file sent into any folder (ST-M010-001).* The wire and the agent's
file store could already commit a file at an absolute path, but nothing that
is shipped asked for it: the AI's upload tool takes a staging name only. The
technician's file actions are the first to send a file straight into a
folder. Such a file is the customer's to keep: it lies outside the staging
area, in no manifest, and no cleanup removes it. What can go wrong: the
unfinished part of a transfer that never completes stays in the customer's
folder; a program of an unprivileged account turns the elevated agent
elsewhere — it plants a link under the part's name, gives the part a second
name, or swaps the folder for a junction between two requests of the
transfer, so that a chunk, the verified file or the cleanup lands in another
directory; a file that was already there is replaced unasked; a torn or
unverified file appears under the final name; a full disk or a file in use is
reported as something else. Mitigations:

- *Verified before it has its name.* The bytes go to a part file beside the
  destination — on the same drive, so the commit is one rename on any drive,
  and created in that folder, so on Windows the file inherits the folder's
  permissions — and the file appears under its name only after its length and
  SHA-256 are verified (FT-2, FT-3, REQ-N-007).
- *Replaced only on request.* Without the request to replace, a file of that
  name fails the transfer with `already_exists` and stays as it is; the
  rename that publishes the file refuses a taken name in the same step. A
  file that another program holds open is not replaced either, and the answer
  says `file_in_use`.
- *Inside the folder that was opened.* On Windows every request of the
  transfer — each chunk, the commit, the discard and the cleanup — opens the
  folder once as a verified folder (FT-8): it must be a real folder that lies
  where the request says, so a junction or a symbolic link in its place or
  in a folder above it is refused as `invalid_path`, as is a `subst` drive or
  a network drive. Everything after that happens inside the opened folder.
  The part is opened by its one name relative to the folder's handle, where
  neither a path, nor a junction, nor a link in the object manager's
  namespace takes part. The commit reads the length and the digest through
  the handle it then publishes, which only readers share, and publishes with
  a rename inside the directory the part lies in. Nothing that happens to a
  path meanwhile changes where the bytes go.
- *The part is the transfer's own.* The part must be a regular file with
  exactly one name, and one that this session began. A link under its name, a
  folder, a second name (a hard link) of a file that lies elsewhere, and a
  file that lay there before the transfer wrote its first byte are refused:
  never written to, never published. Whoever put a file there would stay its
  owner and could change it after it got its name.
- *The part is cleaned up.* The agent remembers every unfinished part that
  lies outside the staging area (at most 1024 at a time; one more is refused)
  and removes them with every cleanup: the one the session runs by itself
  when it ends by a stop, a revocation or a lost connection, and one the
  console asks for. On Windows the part is removed through its handle inside
  the verified folder; a folder that is a link by then is not followed. A
  part it could not remove is reported like any other cleanup failure and
  tried again by the next cleanup (REQ-F-019). So is a part whose folder is
  no longer in its place: a folder that was moved took the part along, and
  the agent does not call gone what it cannot see. The parts are removed
  also when the staging area itself cannot be cleaned up. A commit of an
  empty file that is refused removes the empty part it made, so refused
  commits leave nothing to remember.
- *A name is one name.* The technician computer refuses, before anything is
  sent, a name that is a path and a folder that is not a rooted path on a
  drive; the wire validation applies the path rules again on both ends (FT-1).
- *Attributed, visible, recorded.* The transfer carries the origin `console`,
  the customer's activity shows a received file with its path and size, and
  the console records `console_send_start` and `console_send` with the path
  the agent committed — the folder's real location and the file's name — the
  length and the verified digest, and whether a replacement was asked for
  (REQ-F-017, REQ-S-017). A full disk is reported as `storage_exhausted`
  (REQ-N-015).

Evidence: `the_part_lies_beside_the_destination_until_the_verified_commit`,
`a_file_sent_to_a_folder_is_kept_and_not_staged`,
`the_part_of_an_unfinished_transfer_to_a_folder_is_removed_by_the_cleanup`,
`a_part_the_cleanup_cannot_remove_is_reported_and_tried_again`,
`a_part_whose_folder_is_no_longer_in_its_place_is_named_and_tried_again`,
`a_cleanup_that_cannot_use_the_staging_area_still_removes_unfinished_parts`,
`a_file_that_lay_under_the_name_of_a_part_before_the_transfer_is_not_taken_for_it`,
`a_path_from_a_planted_manifest_fits_the_answer_whatever_it_holds`,
`a_link_planted_under_the_parts_name_is_never_written_through`,
`a_folder_under_the_parts_name_is_refused`,
`a_refused_commit_of_an_empty_file_leaves_no_part_and_no_memory_of_one` and
`a_full_disk_is_reported_as_exhausted_storage`
(`crates/exec/src/fs/tests.rs`), with
`a_junction_in_the_place_of_the_folder_is_refused_by_every_step_of_a_transfer`,
`a_folder_swapped_for_a_junction_gets_nothing_published_or_removed_in_its_target`,
`a_junction_under_the_parts_name_is_never_written_through`,
`a_part_with_a_second_name_is_never_written_to_and_never_published`,
`a_destination_held_open_by_another_program_is_not_replaced`,
`a_file_is_committed_into_the_root_of_a_drive` and
`a_file_is_committed_into_a_folder_spelled_by_a_short_name_or_in_another_letter_case`
on Windows; `an_open_in_a_folder_stays_in_that_folder_whatever_its_path_leads_to`
and `a_rename_through_a_handle_stays_in_the_folder_of_the_file` on Windows
(`crates/exec/src/fs/anchor/tests.rs`); `a_name_that_is_a_path_is_refused`
(`apps/console/src/service.rs`); and, on Windows only, because the wire
accepts a path only when it is rooted on a drive,
`a_file_is_sent_to_a_folder_and_replaced_only_on_request`,
`a_revoke_during_a_transfer_to_a_folder_leaves_no_part_behind`
(`crates/session/tests/e2e.rs`),
`a_file_is_sent_into_a_folder_verified_and_replaced_only_on_request` and
`a_large_file_arrives_whole_and_a_cancelled_one_leaves_nothing`
(`apps/console/tests/technician_files_e2e.rs`).

*FT-10 — The list of drives (ST-M010-001).* The list reads facts and changes
nothing, but it carries text that anyone can choose — the label of a USB
stick — to the technician computer, and a careless query could open a system
dialog on the customer's screen or wait on a file server while the agent
serves nothing else. Mitigations: at most `MAX_DRIVES` (32)
entries, a root and a label bounded and kept on one line by the agent and
checked again by the wire validation on both ends; a drive without a medium
is listed with its kind and no size, and no dialog is shown for it; a drive on
the network is listed with its kind only, so the list never waits on the
network; a value the system cannot give is absent, never zero and never
guessed (REQ-N-006, REQ-N-008). Evidence:
`a_label_is_bounded_and_kept_on_one_line`,
`a_drive_without_a_usable_root_is_left_out` (`crates/exec/src/fs/drives.rs`),
`drive_texts_are_single_lines` and `too_many_drives_rejected`
(`crates/protocol/src/tests.rs`).

*FT-12 — Looking through the customer computer's files from the technician's
window (ST-M010-002).* The explorer lets the technician open drives and
folders of the customer computer and read their names, sizes and dates. It
reads and changes nothing. What it adds: a command of the application's
backend that takes a path from the window; text that anyone on the customer
computer can choose — the name of a file, the label of a drive — shown in the
technician's window; listings of any length; and many small requests that the
customer must still be able to follow. Mitigations:

- *No authority of its own.* Both commands exist in a technician process
  only. Outside a session under full control they refuse with
  `no_consented_session` before anything is asked, and so they do when the
  session's audit trail is not open: nothing the technician does through the
  explorer can miss the trail. Nothing is asked of the customer computer
  before the technician opens the Files tab. A stop or a revocation ends a
  listing with the session, between two of its requests, as an error and
  never as the whole folder; what was listed is dropped from the window when
  full control ends (REQ-S-023, REQ-S-004, REQ-N-008).
- *A path from the window.* The path of `explorer_open` is a path on the
  customer computer. It is never opened, resolved or written on the
  technician computer. It must be a rooted path on a drive
  (`validate_remote_absolute_path`: no `..`, no UNC or device namespace, no
  reserved device name, no alternate data stream, bounded in length and
  depth); any other text is refused, and recorded as refused, before
  anything is asked. The wire validation applies the same rules again on both
  ends (FT-1).
- *The place that is shown is the real one.* No answer of the agent holds a
  path, so the explorer names a place by what the customer computer answers:
  the root of a drive as the drive list gives it, and every further name as a
  listing of the folder above it gives it. A path that was typed is looked
  for name by name; in another letter case it is led to the listed spelling,
  and that is what the window shows, what later requests carry and what the
  audit record names. A name the folder above does not list is not found —
  a second spelling such as a short name included — and nothing is opened.
  The search for a name reads at most as many entries as a listing does; when
  it stops there, the refusal says so and does not claim that the name is
  absent (REQ-N-008).
- *A link is shown, not followed.* A listing describes a link and never
  follows it. The explorer does not open one, and refuses a typed path that
  ends in one or leads through one, before anything behind it is asked. So
  the explorer never stands in a place whose real location differs from the
  path it shows, which is the path a later change would name (FT-8, FT-11).
- *Text from the customer computer.* Names, labels and paths are written into
  the window as text, never as markup, and every string of the backend's
  events is checked against the contract before it is taken; an event with a
  malformed entry is dropped whole, so a damaged page cannot make a folder
  look complete (REQ-S-016, REQ-S-018). The type shown for a file is read
  from the end of its real name.
- *A folder of any size.* A page holds at most 256 entries and always fits one
  record of the secure channel (ST-M010-005). A listing stops at 100 000
  entries and says that the folder holds more; the window draws only the rows
  in view. A new listing, or the drive list, leaves the one that is still
  running, so one window never has two listings in flight. What the session
  remembers of the places the customer computer confirmed is bounded (the
  drive roots, 256 folders, 20 000 subfolder names of the folder listed last)
  and ends with the session (REQ-N-006).
- *What the customer sees.* Every request carries the origin `console`, so it
  is shown as the technician's. The requests of one look after another are
  one item in the customer's activity, *Looked through files and folders*,
  which names the folder that is looked at now; a change, a transfer, a
  command and an action of the AI stay items of their own, and a look that
  the customer computer refused is a failed item of its own that no later
  look replaces (REQ-F-017). A look of the AI is never folded into it: it
  keeps the purpose the AI stated (REQ-F-026).
- *What is recorded.* Each folder that is opened is one pair of audit
  records, `console_fs_list_start` and `console_fs_list`, with the path as it
  was asked for, then the folder as the customer computer names it, the
  number of entries, and an outcome that is `ok` only for a listing that
  reached the end of the folder or its bound (REQ-S-017).

Evidence: `a_typed_path_is_led_to_the_names_the_target_lists`,
`a_refused_spelling_reaches_nobody`,
`a_name_the_folder_does_not_list_is_not_found`,
`a_link_is_refused_and_nothing_behind_it_is_asked_for`,
`a_listing_stops_at_its_bound_and_says_that_more_exist`,
`a_listing_that_fails_on_its_way_is_an_error_with_what_arrived` and
`a_folder_that_was_forgotten_is_looked_up_again`
(`apps/console/src/explorer/tests.rs`);
`a_large_folder_is_listed_in_pages_and_is_one_item_on_both_sides`,
`looks_follow_along_and_a_change_is_its_own_item_on_both_sides`,
`a_folder_that_cannot_be_listed_is_refused_and_recorded`,
`a_stop_ends_a_listing_and_later_looks_fail_closed`,
`a_path_that_is_no_remote_path_is_refused_before_it_begins` and, on Windows,
`a_typed_path_opens_the_folder_under_its_real_name`
(`apps/console/tests/technician_files_e2e.rs`);
`a_look_that_was_refused_is_a_failed_item_of_its_own` and
`a_look_of_the_ai_stays_an_item_with_its_stated_purpose`
(`crates/session/src/activity/tests.rs`);
`without_a_session_both_commands_fail_closed`,
`without_an_audit_trail_both_commands_fail_closed`,
`a_place_that_is_refused_delivers_nothing_and_says_why` and
`after_a_stop_both_commands_fail_closed`
(`apps/app/src-tauri/src/technician/explorer/tests.rs`); and the frontend
tests `explorer.test.ts`, `explorer-views.test.ts` and
`explorer-controller.test.ts` in `apps/app/src/technician`. The run on real
hardware is the browsing steps of scenario MT-14 in
[`manual-tests.md`](manual-tests.md), not yet recorded.

*FT-13 — Sending, copying to the technician computer, renaming and deleting
from the technician's window (ST-M010-002).* The second part of the explorer
makes changes everyday actions: a drop on the window sends files, **Download**
writes files of the customer computer on the technician computer, **Delete**
removes a folder with everything in it, **Rename** moves an entry. What it
adds: commands of the application's backend that are driven by text from the
window; files written on the technician computer under names that the
customer computer chooses; a destructive action one key away; many requests
that run without the technician watching each; and transfers that are still
on their way when the session ends. Abuse: a customer computer — or whoever
answers in its place — names a file `..\..\Startup\run.bat`, `C:\x`, `NUL`
or `report.pdf.` so that a copy lands outside the folder the technician
chose, on a device, or over another file; a listing says that a link is a
folder so that a copy follows it; the content of the window, should it ever
be led astray, names a file of the technician computer to send or a place to
write to; a transfer that failed, was cancelled or was cut off by the end of
the session is shown as done; a delete removes more than was confirmed, or
reports as removed what is still there; a rename moves an entry to another
folder or replaces another file. Mitigations:

- *No authority of its own.* Every command exists in a technician process
  only. Those that ask something of the customer computer, or that prepare
  to, refuse with `no_consented_session` outside a session under full
  control with an open audit trail, before a dialog opens or anything is
  kept: nothing the technician changes can miss the trail. What a dialog
  gave is not kept either when the session ended while it was open. The five
  that only look at or cancel what the backend holds — the queue, the three
  cancels, the folder of a copy — reach nothing on the customer computer. A
  stop or a revocation cancels the transfer and the removal that run; the
  queue then ends the running transfer and every waiting one as stopped,
  never as done, and a delete says how far it came. The window says so in
  the queue, in the explorer's alert and, once the explorer is dropped with
  the session, in its own alert (REQ-S-023, REQ-S-004, REQ-N-008).
- *The window names no file of the technician computer.* What is sent comes
  from the system's file dialog, which the backend opens itself, or from a
  drop on the window, which the backend receives itself. It keeps the paths
  as an offer and tells the window a number and the names; a send names the
  number. A copy goes to `Downloads\CretLink\<target>` or to a folder the
  technician chose in the system's folder dialog, kept under a number. The
  folder that **Show in folder** opens is the one a copy of the queue went
  to. So a window that was led astray could send only what the technician
  dropped or picked, and write only where the technician chose.
- *A place on the customer computer is text, and checked.* A folder from the
  window must be a rooted path on a drive (`validate_remote_absolute_path`,
  FT-12); a name must be one name, without a separator, a colon or a
  character that no name may hold. Neither is ever opened on the technician
  computer. The agent has the last word: it takes up the folder of a change
  or of a transfer as a verified folder and refuses a path through a link
  (FT-8, FT-9, FT-11).
- *A name from the customer computer is untrusted.* A copy is written at the
  chosen folder plus names that listings give. Each such name must be one
  plain name before anything is written (`explorer/local.rs`): not empty,
  not `.` or `..`, no `\` or `/`, no colon (a drive or an alternate data
  stream), no control character, none of `< > " | ? *`, no trailing dot or
  space (Windows would read it as another name), no reserved device name
  (`CON`, `NUL`, `COM1`, `LPT1` and the others, with or without an
  extension), not longer than 255 units, and not the name CretLink gives an
  unfinished copy. Any other name is refused, never repaired: its entry is
  left out with a row that says why, and what lies beside it is still
  copied. The path of every copy is checked once more, when it is about to
  be written, to lie inside the chosen folder through plain names only. The
  folder for `<target>` is made from the host name the customer computer
  reports, with everything but letters, digits, `-`, `_` and `.` replaced,
  and a fixed name when nothing usable is left.
- *What a name is comes from the backend's own look.* The window says which
  entries are selected; whether each is a file, a folder or a link is read
  from a listing the backend asks for, once for the selection and once for
  each folder below it. A link is never followed in either direction: one
  that is selected or lies in a folder that is copied is left out and says
  so, and a link in a dropped folder is not sent (`symlink_metadata`, never
  the target). The walks are bounded — 10 000 files and folders for one send
  or one copy, 32 folders deep, 100 000 transfers in a session's queue — and
  a folder that holds more, or that could not be listed to its end, has a
  row that says it is not complete (REQ-N-006, REQ-N-008).
- *Nothing is replaced without the request.* Before a send the backend lists
  the folder and the window asks, for each name that is taken, whether to
  replace it, keep both (`name (2).ext`) or skip; before a copy it looks at
  the technician computer's folder and asks the same. A file replaces a
  file and a folder goes into a folder; where the kinds differ, replacing is
  not offered, and it is refused when it is asked for all the same. A name
  that is taken and has no choice is left out. A name that turns out to be
  taken after the look fails its transfer: the agent refuses to publish over
  a file without the request (FT-9), and a copy whose place is taken is not
  started.
- *Done means verified.* A row of the queue reads as sent or copied only for
  a transfer the engine reported as completed, which is one whose length and
  SHA-256 were verified (FT-2, FT-3). A failed transfer carries the reason in
  the words the activity has for its category — in use, access denied, the
  disk is full — or the local path and reason for a file of the technician
  computer; a cancelled one says cancelled; one that the end of the session
  cut off says stopped. A transfer that the session cancelled without the
  technician asking is the end of the session, not the technician's cancel.
- *A cancel leaves no part.* The engine asks the agent to discard the part
  of a cancelled send; the queue then asks whether a part is still there and
  says so when it is, with what happens to it (the session's cleanup removes
  it, FT-9). The part of a cancelled or stopped copy is removed from the
  technician computer, and the row says so when it could not be.
- *Delete is confirmed once, for exactly what is selected.* The window asks
  before anything is removed, with the number of items and the sentence that
  there is no recycle bin, and gives the focus to **Cancel**. Each item is
  then removed by the operation its listed kind has: `remove_tree` for a
  folder (FT-8), `fs_delete` for a file or a link, which removes a link as
  the link (FT-11). The kind only chooses the operation: the agent refuses a
  folder that is asked to be deleted as a file and a file or a link that is
  asked to be removed as a folder, so a kind that is out of date removes
  nothing. The answer counts an item as removed only when the agent said so,
  names the entries that could not be removed with their reasons — the first
  twenty, the others as a number — and the folder is listed again. One
  delete takes at most 10 000 items and can be cancelled between two steps.
- *Rename stays in its folder.* The new name is one name; a text with a
  separator would move the entry and is refused before anything is asked.
  The move never replaces: a name that is taken is refused by the agent in
  one step with the rename (FT-11).
- *Text from the customer computer.* Names, paths and reasons are written
  into the queue, the questions and the alerts as text, never as markup, and
  every event of the queue is checked against the contract before it is
  taken; one with a malformed entry is dropped whole (REQ-S-016, REQ-S-018).
- *The file manager is started by its full path.* **Show in folder** starts
  `explorer.exe` in the Windows folder the system names, `/usr/bin/open`, or
  `xdg-open` from one of the system's program folders, with the folder as its
  only argument and never through a shell; a bare name that a search path
  resolves is never handed over.
- *What the customer sees, and what is recorded.* Every request carries the
  origin `console`. Each file that arrives, each file that leaves, each
  folder that is made, each rename and each delete is an item of its own in
  the customer's activity; the looks stay the one item that follows along
  (FT-12). Each is a pair of audit records of the technician
  (`console_send`, `console_download`, `console_make_dir`, `console_move`,
  `console_delete`, `console_remove_tree`, and `console_fs_stat` for the
  look after a cancelled send), with the path on the customer computer, the
  length and the digest of a transfer, and an outcome that is `ok` only for
  what the agent confirmed. The path of a file on the technician computer is
  no argument of a record. It stands in one place only: the reason of a
  transfer that failed on the technician computer, which names the file that
  could not be read or written. That reason stays in the technician's own
  audit log and is not sent to the customer computer (REQ-F-017, REQ-S-017,
  REQ-S-018).

Evidence: `a_name_that_could_leave_its_folder_is_refused`,
`a_name_in_a_folder_stays_in_that_folder`,
`the_folder_of_a_customer_computer_is_a_plain_name`,
`a_link_in_a_dropped_folder_is_named_and_not_followed` and
`a_walk_stops_at_its_bounds_and_says_so`
(`apps/app/src-tauri/src/technician/explorer/local/tests.rs`);
`names_that_try_to_leave_the_chosen_folder_are_left_out`,
`a_copy_whose_place_is_outside_the_chosen_folder_is_refused`,
`what_a_selected_name_is_comes_from_the_listing_not_from_the_window`,
`a_selected_folder_is_copied_file_by_file_and_a_link_is_left_out`,
`a_name_that_is_taken_is_replaced_kept_beside_or_skipped_as_chosen`,
`a_name_that_is_taken_here_is_replaced_kept_beside_or_skipped_as_chosen`,
`a_file_never_replaces_a_folder_on_this_computer`,
`a_file_that_appeared_after_the_look_is_never_replaced`,
`an_offer_is_queued_only_for_the_folder_it_was_checked_against`,
`a_cancelled_transfer_never_reads_as_done_and_says_what_it_left`,
`a_cancelled_or_stopped_copy_leaves_no_part_on_this_computer`,
`the_end_of_the_session_stops_the_running_and_every_waiting_transfer`,
`a_transfer_the_session_cancelled_is_stopped_not_cancelled`,
`a_folder_that_is_not_walked_to_its_end_says_so` and
`a_new_session_starts_with_an_empty_queue`
(`apps/app/src-tauri/src/technician/explorer/transfers/tests.rs`);
`a_new_name_that_is_a_path_is_refused_before_anything_is_asked`,
`a_rename_onto_a_name_that_is_taken_changes_nothing`,
`a_delete_removes_each_item_by_what_it_is`,
`a_kind_that_is_out_of_date_removes_nothing`,
`what_could_not_be_removed_is_named_with_its_reason` and
`the_end_of_the_session_stops_a_delete_and_says_so`
(`apps/app/src-tauri/src/technician/explorer/changes/tests.rs`);
`without_a_session_every_command_fails_closed`,
`without_an_audit_trail_every_command_fails_closed`,
`a_folder_from_the_window_is_a_path_on_a_drive_of_the_customer_computer`,
`a_copy_goes_to_the_default_folder_or_to_one_the_technician_chose`,
`a_selection_whose_name_is_a_path_is_refused_whole` and, on Windows,
`files_are_sent_copied_back_renamed_and_deleted_through_a_session`
(`apps/app/src-tauri/src/technician/explorer/commands/tests.rs`);
`the_file_manager_is_named_by_its_full_path` (`apps/app/src-tauri/src/opener.rs`);
`the_technician_copies_a_file_to_this_computer`,
`a_cancelled_copy_is_never_done_and_its_part_is_named`,
`the_technician_changes_one_entry_and_both_sides_say_the_same`,
`a_stop_ends_a_copy_and_later_changes_fail_closed` and, on Windows,
`changes_and_a_copy_work_in_a_real_folder_and_refuse_a_link_on_the_way`
(`apps/console/tests/technician_files_e2e.rs`); and the frontend tests
`explorer.test.ts`, `explorer-views.test.ts` and
`explorer-controller.test.ts` in `apps/app/src/technician`. The run on real
hardware is scenario MT-14 in [`manual-tests.md`](manual-tests.md), not yet
recorded.

*FT-11 — A change to one entry is led to another place (ST-M010-004).*
Deleting a file, removing an empty folder, creating a folder and moving or
renaming an entry each change one entry of a folder. The operations exist
since M003, the AI reaches them through `remote_delete`, `remote_remove_dir`,
`remote_make_dir` and `remote_move`, and the explorer makes them everyday
actions. Up to release 0.24.1 the agent resolved their paths by name. The
agent is elevated, and the folder may belong to an account that is not. A
program of that account can make a folder on the way a junction — also one
that leads into a directory of the object manager, where a symbolic link
sends one file name to any path — or swap the folder for a junction between
two requests, or between a check and the change. The delete, the new folder
or the moved file then lands where the junction leads: with administrator
rights, in a place the account could not change itself (REQ-S-024).
Mitigations, on Windows and for an absolute path:

- *The folder that holds the entry is the one that lies at the path.* Each of
  the four operations takes up that folder once, as a verified folder (FT-8):
  it is opened without following a link in the last part of its path, where
  it really lies is read from the opened handle, and that is compared with
  the path that was asked, without regard to letter case and with short names
  written out. A path that reaches the folder through a junction or a
  symbolic link, a folder that is itself a link, and a path on a drive made
  with `subst` or on a network drive are refused as `invalid_path` before
  anything changes. So is a folder whose real location the system cannot
  tell, such as one on a volume without a drive letter. A volume that is
  mounted in a folder counts as a link here: an entry directly in its top
  folder cannot be changed by that path.
- *Delete a file* (`fs_delete`). The entry is opened by its one name relative
  to the folder's handle, as the entry it is and never as what a link points
  at. What it is is read from that handle: a folder is refused, a file or a
  link is removed through that same handle.
- *Remove an empty folder* (`remove_dir`). The entry is opened the same way.
  Only a real folder is removed, through its own handle, and only when it is
  empty.
- *Create a folder* (`create_dir`). One level, as before: the folder is
  created relative to the handle of the folder it is created in, never by a
  path, and gets the permissions that folder hands down. A folder above it
  that is missing is not created.
- *Move or rename* (`fs_move`). Both folders are taken up before anything
  changes. The entry is opened relative to the handle of its folder and
  renamed through its own handle into the handle of the destination's
  folder: the system puts the entry into the directory that handle stands
  for, and no path names the destination. It stays a rename on one volume,
  never a copy; a name that is taken is refused, in one step with the
  rename, unless the caller asked to replace it, and a folder under that
  name is never replaced.
- *Nothing is resolved by path a second time.* The check of the folder and
  the change are about the folder that was opened. A folder that is swapped
  for a junction after it was taken up changes nothing: the operation acts
  in the folder it opened. A folder that is swapped before the next request
  is a link when that request takes it up, and the request is refused.
- *A link as the entry itself keeps its meaning.* Only the way to the entry
  must be free of links. A delete removes a link — a symbolic link to a file
  or to a folder, a junction, a link to nothing — as the link, never what it
  points at. The removal of an empty folder refuses a link. No folder is
  created under a name that a link has, and none where the link points. A
  move moves the link, and a move that may replace never reaches what a link
  under the destination's name points at.
- *The staging area goes on by path.* An operation on a staging name works
  by path, as the staging area's own operations do (part, commit, discard,
  delete, cleanup). Since ST-M006-020 the staging area of an elevated
  program is a private folder of the administrators, and that of a program
  without administrator rights lies in the folder of its own account: no
  other account can change a folder in either (FT-7), so there is no link
  another account could have put there. A move between a staging name and a
  path verifies the folder of the path. An elevated program still takes
  nothing into its staging area by a move, and what it moves out takes the
  permissions of the folder it arrives in (FT-7). Both are decided from
  where the two opened folders really lie, so a request that names the
  staging area by its full path is treated like one that uses a staging
  name, and the permissions are replaced through the handle that renamed
  the entry: no path is resolved for it again.
- *Said to the AI.* The four tools say in their description that a path
  through a link is refused, that nothing changes then, and that the real
  location is to be named. Their inputs and their results are unchanged.

Evidence: `a_delete_removes_a_link_and_never_what_it_points_at`,
`a_link_is_not_removed_as_an_empty_folder`,
`a_folder_is_not_created_under_the_name_of_a_link`,
`a_move_moves_a_link_as_the_link`,
`a_move_that_replaces_never_reaches_what_a_link_points_at`,
`a_move_into_another_folder_keeps_its_contract`,
`what_is_not_there_or_not_of_its_kind_is_refused_as_before`,
`a_delete_of_a_second_name_of_a_file_leaves_the_file_under_its_other_name`
and `a_file_is_moved_between_the_staging_area_and_a_folder`
(`crates/exec/src/fs/tests.rs`), which run on every CI platform and pin what
each operation meant before; the two rules about the private staging area
with `nothing_is_moved_into_a_private_staging_area_from_elsewhere` and, on
Windows,
`a_file_moved_out_of_a_private_staging_area_takes_the_permissions_of_its_new_folder`
(the same file), each also with the staging area named by its full path;
with, on Windows,
`a_delete_through_a_junction_is_refused_and_what_it_leads_to_stays`,
`a_folder_removal_through_a_junction_is_refused_and_what_it_leads_to_stays`,
`a_folder_creation_through_a_junction_is_refused_and_nothing_is_created_behind_it`,
`a_move_through_a_junction_on_either_side_is_refused_and_nothing_moves`,
`a_folder_swapped_for_a_junction_between_two_deletes_is_not_followed`,
`a_folder_swapped_for_a_junction_between_two_folder_removals_is_not_followed`,
`a_folder_swapped_for_a_junction_between_two_folder_creations_is_not_followed`,
`a_folder_swapped_for_a_junction_between_two_moves_is_not_followed`,
`a_change_is_made_in_the_folder_that_was_opened_whatever_its_path_leads_to`,
`a_junction_as_the_entry_keeps_its_meaning_for_every_operation`,
`a_move_between_the_staging_area_and_a_folder_behind_a_junction_is_refused`,
`a_folder_spelled_by_a_short_name_or_in_another_letter_case_is_the_same_folder`,
`a_held_file_is_in_use_and_read_only_entries_are_removed` and
`the_root_of_a_drive_is_no_entry_to_change` (the same file);
`a_move_through_handles_lands_in_the_opened_folder_and_replaces_only_when_asked`
and `a_folder_of_cretlinks_own_is_taken_up_as_it_is` on Windows
(`crates/exec/src/fs/anchor/tests.rs`); and
`the_tools_that_change_one_entry_say_that_a_path_through_a_link_is_refused`
(`crates/mcp/tests/bridge_mock.rs`). The tests that need Windows run on the
Windows CI runner only; the run on real hardware is steps 15 to 17 of
scenario MT-14 in [`manual-tests.md`](manual-tests.md), not yet recorded.

**Residual risks.**

- A release up to 0.24.0 has the weakness of FT-7 whenever it is started,
  also on a computer where a later release has run: it uses the old folder
  and takes its location from its environment. Use the current release.
- An account with administrator rights can change any folder. CretLink does
  not defend a computer against its administrators: what stands in
  `CretLink Data` was put there by one of them. The protection assumes
  `Program Files` as Windows ships it, where only administrators can create
  or move anything.
- A folder `CretLink Data`, or a folder in it, that CretLink did not create
  (made by hand, or with its permissions changed) is not used and not
  repaired. The elevated program then stages nothing and says why, until an
  administrator removes that folder.
- Removing the old folder removes all it holds, and an account without
  administrator rights decides what that is. Such an account can move into
  the old folder a folder that it may move but not empty: one that lies
  where the account is in control and holds files that are protected against
  it. Where an elevated program of an earlier release made the old folder,
  the first elevated start of this release removes it with those files.
  After that the account can make the folder again, but then the account
  owns it: an elevated start names such a folder and leaves it, and only
  `recover --remove`, run as an administrator, removes it with all it holds.
  Nothing is removed that the account could not have moved there, and no
  link is followed. This lasts for as long as a release looks for the old
  folder.
- The downloaded program runs from a folder that its account can write. A
  program of that account can change the file before it is started, or put a
  library beside it that Windows loads with it, and an administrator who
  starts a program that another account downloaded trusts that account's
  file. CretLink starts no program by its name alone, but it does not check
  itself. Check the download as [`verifying-artifacts.md`](verifying-artifacts.md)
  describes, and keep it in a folder that other accounts cannot write. A
  customer program that is installed where only administrators can write is
  ST-M009-017.
- Command Prompt looks for a program that is named without its folder in the
  working directory first. A job that names no working directory starts in
  the system folder. A job or a terminal that the technician lets work in a
  folder which another account can write, or that changes to one, can start
  a program planted there with the rights of the session. PowerShell does
  not look in the working directory.
- A job runs with the environment of the CretLink program and what the
  technician adds to it; CretLink does not build that environment itself.
  Windows gives a program that is started through the elevation prompt the
  environment of the account that approved, not that of the program that
  asked, so an account without rights does not choose the search path of an
  elevated session. CretLink relies on that, and MT-4b checks it on real
  hardware. The same holds for the profile of the elevated window's web
  view, which lies in the local application data of the account that
  approved.
- A program without rights can still get in the way. It can hold a file in
  the old folder open so that the folder cannot be removed, or fill the
  folder so that removing it takes long: a start and `recover --remove` wait
  a bounded time, say that the folder is still there, and go on. It can also
  make the old folder again, so that every elevated start names it until an
  administrator removes it. That delays a cleanup and costs a notice; it
  gives nothing away. It can also keep one request of the agent waiting in
  the operating system, for example by holding an entry of a folder under an
  opportunistic lock and not answering when the agent opens it. The agent
  serves one request at a time, so everything else waits with it, a stop by
  the customer included, until that program lets go or CretLink is ended.
  This holds for every file operation, not only for the removal of a folder.
- A process that is elevated without being an administrator, and a computer
  whose `Program Files` is not a folder of the system, get no staging area
  from an elevated CretLink.
- Each kind of program sweeps what its own kind left. What a session without
  administrator rights left in the folder of its account is swept by the
  next start without them, not by an elevated one, and the other way round.
- The folder `CretLink Data` stays in `Program Files` after a session, with
  an empty staging area and its lock. `recover --remove`, run as an
  administrator, removes it when nothing is kept in it. Run it when no
  CretLink window is open or starting: a window that opens the lock at the
  moment the recovery removes it holds a lock that no later window finds, and
  the sweep of a later window could then remove what the first one staged.
- What an elevated session keeps on purpose (cleanup policy *keep*) lies in
  `CretLink Data`, which only administrators can read. The customer needs
  administrator rights to reach it, and a path into it holds a space.
- An authorised technician is trusted with file content. Once the customer
  grants full control, CretLink cannot distinguish a legitimate log download
  from exfiltration of a personal document. The defence is disclosure,
  visibility, audit, and revocation — not prevention.
- Content forwarded to the AI provider leaves both machines. Disclosure
  (REQ-S-016) informs the customer, but CretLink cannot retract data once a
  cloud model has received it.
- No malware scanning of staged artifacts. CretLink verifies integrity (the
  bytes are the bytes the technician staged), not safety. It relies on the
  platform's endpoint security, which it deliberately does not bypass.
- Full-control file access is intentionally broad on the customer side. Path
  restrictions apply to constrained staging and download areas, not to the
  consented general file surface; a semantic allowlist of "safe paths" is a
  non-goal.
- Resource limits are robustness bounds, not quotas. An authorised session can
  still legitimately fill a disk with a large installer; limits exist to stop
  unbounded or accidental exhaustion, not to police approved work.
- Only a folder page is built to fit one record of the secure channel. Other
  answers can still be larger than a record within the bounds of the
  protocol: a list of very many jobs or staged files, a cleanup report that
  names very many files, a file read of more bytes than a record carries, a
  large screenshot, a long clipboard text. Such a request fails with the
  reason and the session goes on (ST-M010-005), but its answer cannot be had
  that way. The transfer engine asks for file content in chunks that fit.
- A removed folder is gone. The removal does not use a recycle bin and cannot
  be undone; the operation removes what it is asked to remove, and any
  question before it is the calling screen's. The defence is consent, the
  customer's view of the action, the audit record, and the stop control.
- A file sent to a folder stays. It is outside the staging area by design, so
  the cleanup that removes staged files does not remove it; the audit record
  and the customer's activity are what remains of how it got there.
- A part can outlive a killed agent. The list of unfinished parts outside the
  staging area is kept in memory. An agent that is killed, or a computer that
  loses power, in the middle of such a transfer leaves a file named
  `.cretlink-part.` followed by sixteen hexadecimal digits beside the
  destination, and `cretlink recover` does not look for it.
- A folder of a cloud drive is removed with its content. A folder that is a
  reparse point without standing for another place — a folder that OneDrive
  manages — is walked like any folder, and the cloud service may then remove
  the same files from the cloud, as it does when the folder is removed in
  Windows itself.
- Where a folder really lies is verified on Windows only. Elsewhere the
  removal of a folder, a file sent to a folder and a change to one entry (a
  delete, the removal of an empty folder, a new folder, a move) work on the
  path they are given, without following a link in the last part of a name;
  a non-Windows agent is a development and test tool that is never elevated
  on a customer computer.
- A path through a link is refused, also when the link is harmless. A
  removal, a transfer or a change to one entry whose path leads through a
  junction or a symbolic link — the ones Windows keeps itself, such as
  `C:\Documents and Settings`, included — or that lies on a drive made with
  `subst` or on a network drive is refused as `invalid_path`. The technician
  and the AI use the path where the folder really lies. Since ST-M010-004
  this also holds for the four AI tools that change one entry, which worked
  through such a path before.
- The folder is the one that lies at the path when the operation begins, and
  the entry is the one that has its name in that folder when the operation
  opens it. The agent refuses a link in the path and a folder that is
  swapped during a removal; it cannot know which real folder, or which file
  under a name, the technician saw a moment earlier. A program that may
  rename a folder or a file can put it where the technician is about to act,
  as it could before these operations existed.
- Reading goes on by name. A stat, a folder listing, a file read and a
  digest resolve their path as before: they change nothing on the customer
  computer, and what they answer is the target's own account of that path.
- The explorer names a place from listings that were true when they were
  made (FT-12). A program on the customer computer can rename a folder, or
  put a link in its place, after the explorer was led there. The explorer
  then still shows the old path; the agent lists whatever lies at that path
  now, and for a change it refuses a path through a link (FT-11). What the
  session remembers of confirmed places is a help against needless requests,
  not a check.
- The customer sees that the technician looks through files and which folder
  is open now, as one item. The item does not keep every folder that was
  opened on the way: those are in the technician's audit trail, one record
  for each, and not in the customer's activity or end report.
- A name can be made to look like another one, for example with characters
  that change the direction of writing. The explorer writes a name as text
  and reads the type of a file from the end of its real name, but it does
  not mark or rewrite such names. It opens no file and runs nothing.
- The explorer was not run against a customer computer on real hardware yet
  (MT-14). Its window was tested in jsdom and looked at in Chromium with a
  simulated customer computer, not in WebView2; a drop from a real file
  manager and the system's own dialogs were not exercised, and no screen
  reader has read its grid, its questions or its queue.
- The technician decides about a name that is taken from a look at the
  folder that was true when it was made (FT-13). A file that appears under
  a free name afterwards is not replaced: its transfer fails. A name the
  technician chose to replace is replaced whatever file lies under it when
  the transfer commits: the one that was listed, or another that took its
  place since.
- A delete removes what lies under the selected names when it runs. The
  technician confirmed names of a listing; a program on the customer
  computer can put another file under one of them in between. The agent
  removes only inside the folder that was named, and a folder only as the
  folder it took up (FT-8, FT-11).
- The names a copy writes are checked for what they could do to the place of
  the file (FT-13), not for what they look like. A name can still be made to
  look like another one, and a copied file is the customer computer's
  content: the explorer opens and runs none of it, and what the technician
  does with it afterwards is outside CretLink.
- A copy to the technician computer that is cancelled, or that fails on the
  technician computer, stays in progress in the customer's activity until
  the session ends: the wire has no way to tell the agent that a copy was
  given up, so the agent closes that item with the session. What left the
  computer is never under-reported by this; the item never reads as done.
- While one file is being sent or copied, the session serves nothing else
  from the technician computer: a listing, the terminal and a call of the AI
  wait until that file has ended. A stop by either side does not wait: it
  cancels the transfer.
- An operation on a staging name works by path (FT-11). That rests on the
  staging area being a place that no other account can change (FT-7).
- The identity of a folder is only as good as its file system. On FAT and
  exFAT it is the volume serial number and a file index that such a file
  system can hand to another folder later. Those file systems have neither
  links nor permissions that a removal could be turned against.
- A step that runs out of its time before it could remove anything ends the
  removal as blocked. On a very slow disk a removal can therefore end with
  that reason although nothing stands in its way; asking again goes on.
- The behaviour that only Windows has was not run on real hardware yet
  (MT-14), and its automated tests run on the Windows CI runner only.
- Cleanup can fail. A staged file may survive an unclean shutdown; the
  commitment is to report residual artifacts prominently (REQ-F-019), not to
  guarantee their removal. This honesty is verified by
  `cleanup_failure_is_reported_then_retried_at_next_start`
  (`crates/exec/src/fs/tests.rs`): a genuine removal failure (a delete-denying
  open handle on Windows, a directory planted at the artifact path elsewhere) is
  reported in the cleanup report's failed set with an honest reason and its
  manifest record kept, and the next startup sweep retries and clears it
  (REQ-N-008).
- The transfer subsystem and its AI-facing tools have landed with automated
  and simulated-agent CI evidence plus a real-elevated-agent-on-hardware
  end-to-end run recorded on 2026-08-10, closing the M003/M004 file and
  transfer stories; the automated half of the privileged adversarial and
  negative suite (ST-M003-007) that adds revocation, race, and hostile-input
  coverage has landed (named per abuse case above), while its maintainer-run
  hardware half ([`manual-tests.md`](manual-tests.md)) remains outstanding, so
  this section must be revisited when that hardware run is recorded (REQ-S-021).

### Desktop observation and control (M005)

**Status scope.** M005 is a draft; an experimental op-set exists behind
`desktop.v1`, following ADR-0015: a
same-session Windows capture-and-input backend (DXGI/GDI duplication,
`SendInput` synthesis, bounded clipboard), an honest cross-platform stub
elsewhere, capture and input governors that bound rate, memory, and burst, an
honest `DesktopState` taxonomy (Active, Locked, Switched, Disconnected,
NoInputDesktop, Secure), and a `DesktopActivity` observe/control signal so the
customer console can indicate live observation and control. The capability is
off unless advertised, and where no capture-and-input backend exists every op
fails closed from the backend rather than fabricating a frame or accepting an
input. This section records both the **binding design constraints** on the new
screen-and-input surface (REQ-F-020, REQ-S-021) and the mitigations now
implemented in the experimental code; it covers screen data, input injection,
clipboard data, session switching, lock screen, and secure desktop. A graphical
customer-facing indicator — as opposed to the colour-independent CLI
Observe/Control text line — is not yet implemented.

**Entry points.**

- Capture requests from the MCP client or console into the bridge: display
  selection, quality/rate parameters, and the returned frames themselves.
- Input actions from the MCP client or console into the bridge: pointer
  coordinates, buttons, key values, scroll deltas, hotkey sequences, and wait
  actions.
- Clipboard exchange in either direction, when explicitly enabled.
- Reflected screen content back to the AI: on-screen text — web pages, chat
  windows, documents, credential fields, and any malware UI visible on the
  desktop — becomes AI-readable input, widening the indirect-prompt-injection
  surface described in the AI-client section from text to pixels.

**Threat actors.** A misbehaving or compromised AI/MCP client on the technician
computer that drives capture and input; adversary content *on the customer
screen* that cannot address the AI directly but can plant instructions the AI
reads out of a screenshot (indirect prompt injection through pixels); a remote
adversary who benefits if a compromised AI is steered into typing data outward
or into sensitive fields; and any party hoping desktop control can be turned
into covert surveillance or credential capture on the target.

**Abuse cases.**

- *Covert or stale capture.* Capture continues after revocation or stop, or a
  cached/black/last-known frame is presented as if it were the live desktop, so
  the customer believes observation ended when it did not, or the technician
  acts on a frame that no longer reflects the screen.
- *Secure-desktop escape.* Capture or input is aimed at the logon screen, the
  UAC/credential-provider prompt, or the Secure Attention Sequence in order to
  read or drive credential entry the attended model keeps outside CretLink.
- *Input after handoff.* Synthetic input lands after a target switch, session
  switch, lock, or revocation, or on the wrong desktop, so actions reach a
  surface no consented endpoint intended.
- *Keylogging or credential harvesting by primitive.* The desktop tool surface
  is pushed to expose a keystroke-logging or password-scraping capability rather
  than only discrete, visible, audited pointer/keyboard actions.
- *Clipboard exfiltration or secret spill.* The clipboard is used to pull
  sensitive customer data off the machine, or CretLink is induced to place a
  session secret onto the remote clipboard.
- *Screen-content prompt injection.* Text rendered on the desktop instructs the
  AI to run further commands, weaken defences, or exfiltrate, reaching the AI as
  trusted-looking content through a screenshot.
- *Resource exhaustion via capture.* Unbounded frame rate, dimensions, or
  in-flight frames are used to exhaust memory or bandwidth on either endpoint.
- *Outcome masking.* A dropped or failed capture or input is reported as
  success, so the technician's picture of the desktop diverges from reality.

**Mitigations (design commitments for M005; implemented in the experimental
op-set unless noted).**

- *Capture is same-session and secure-desktop-bounded.* The agent duplicates or
  blits only the active input desktop of its own interactive session
  (ADR-0015); the secure desktop, logon screen, and SAS are **not reachable**
  from the user-mode agent, and locked, switched, disconnected, and
  no-input-desktop conditions are returned as **explicit metadata states**, not
  a fabricated frame (REQ-N-008, REQ-S-013). CretLink installs no mirror/kernel
  driver, hooks no Winlogon, and injects no synthetic SAS to defeat this
  boundary (REQ-S-002, REQ-S-010).
- *Every frame is attributed and bounded.* Each frame carries target hostname,
  `session_id`, session/desktop identity, display, dimensions, DPI, timestamp,
  and desktop state (REQ-F-016); size, quality, rate, in-flight count, and
  memory are bounded with backpressure that coalesces or drops rather than
  buffering without limit (REQ-N-006, REQ-N-005). Stale frames are identifiable
  by timestamp and state so a changed or dead desktop is never mistaken for a
  live one.
- *Input is explicit, bounded, and fail-closed.* Actions name coordinates,
  button, and key values, are rate-limited, and re-validate session `Active`
  state and target/desktop match immediately before delivery; a switch,
  revocation, stop, or mismatch **refuses** the action (REQ-S-004, REQ-S-009,
  REQ-F-018). Stop and revoke work from either endpoint without the other's
  cooperation and block further capture and input (REQ-S-004).
- *No high-value primitive to inject into.* The desktop surface contains no
  keylogging, credential-scraping, or authentication-bypass function
  (REQ-S-013); an attack must express itself as ordinary, visible, audited
  pointer and keyboard actions, each attributable to a specific tool call via
  the routed `call_id` and `origin` (REQ-F-017, REQ-S-017).
- *Clipboard is explicit and secret-free.* Clipboard exchange is off by default,
  bounded in type and size, visible and audited (REQ-S-016 through REQ-S-018),
  and CretLink never places session secrets or AI/provider credentials on the
  remote clipboard (REQ-F-015, REQ-S-015).
- *Visible, accessible indication and informed consent.* The customer always
  sees when screen observation and when input control are active, distinguished
  without relying on colour alone (REQ-N-013, REQ-N-014, REQ-F-020), and consent
  discloses before `Active` that screen contents may be sent to the technician
  computer and onward to the AI service (REQ-S-016, REQ-S-003).
- *Screen content is untrusted input.* As with reflected command output, pixels
  and the text within them carry **no mechanical authority**: CretLink cannot
  stop an AI *model* persuaded by on-screen text, but the architecture gives
  such text no routing, consent, or execution power of its own, and the
  technician's activity view and audit trail keep every resulting action
  visible and attributable (see the AI-client section).
- *Honest outcomes.* Capture and input distinguish completed, failed, refused,
  and unknown results; a dropped frame or undelivered action is never reported
  as success (REQ-N-008).

**Residual risks.**

- On-screen prompt injection is not preventable, only contained. CretLink cannot
  sanitise the semantic content of the customer's screen before the AI reads it;
  defence is the same visibility, attribution, audit, and immediate stop as for
  reflected command output, plus the technician actually watching. Model-level
  safeguards and technician vigilance are assumed, not enforced.
- Screen content is inherently sensitive. A consented desktop-observation
  session necessarily exposes whatever is on screen, including data the customer
  did not consciously intend to reveal; CretLink bounds, discloses, and makes
  visible this exposure but does not redact screen contents semantically.
- The secure desktop is out of reach *by design*, which is a deliberate
  limitation, not a defect: flows that require entering credentials at a UAC or
  logon prompt must be completed by the attending human, and CretLink reports
  the secure-desktop state rather than driving it.
- The experimental op-set is covered by automated tests — the capability
  gate, backend fail-closed behaviour, honest `DesktopState` frames, the
  malformed/oversized action taxonomy, input pacing, the `DesktopActivity`
  signal, and fail-closed behaviour across a mid-session network interruption
  all have workspace tests. The residual risk is now confined to the surfaces
  that only a real attended Windows desktop can exercise: the *live-capture
  honesty* on the actual secure desktop, lock screen, session switch, and
  display/focus changes, and the *graphical* customer-facing observation/control
  indicator. Those need hardware runs and UI work before M005 can leave draft;
  until then the corresponding secure-desktop and consent-visibility
  mitigations rest on the design and the automated state taxonomy rather than
  on hardware evidence (REQ-S-021).
- A fully compromised technician computer defeats this boundary entirely; see the
  consolidated residual risks.

### Temporary persistence and cleanup

CretLink's value proposition depends on the word *temporary*. Everything the
product places on the customer computer — an elevated execution broker, an
optional SYSTEM helper, staged files, session keys, and the single-use
reboot-resume ticket — is authority or material that could
outlive the attended session if it is not reliably removed. This surface
covers what can be left behind, who benefits from it, and how CretLink is
required to remove it or to admit that it could not.

**Status.** Implemented. Session-scoped cleanup of staged files and the
in-process elevated broker comes from the session lifecycle milestones
(M002–M003). Reboot-resume is specified in ADR-0014 and implemented on
Windows: an attended, local arming gesture (ST-M006-001) mints a single-use,
short-lived, target-bound ticket derived from the armed session's
transcript-bound handshake hash (`K_rr = HKDF-SHA256(…, h ‖ Na_rr ‖ Nc_rr)`),
sealed at rest per-user with DPAPI. After a reboot, a challenge-response over a
fresh handshake proves peer continuity but never restores control — attended
full-control consent still runs (ST-M006-002). It is the **single reviewed
exception** to ADR-0003's "no key material persists across sessions", bounded
to authority-reducing continuity proof only. The owned-resource inventory and
`cretlink recover [--remove]` (ST-M006-003, ST-M009-010; also `cretlink-cli
recover` and a `CretLink-Support*` copy's `recover`, the only subcommand such a
copy accepts) report and remove CretLink residue offline — staged trees,
sealed tickets, what lies in the staging area without being CretLink's, and
the folder of a release up to 0.24.0 (FT-7) — and assert every other ownable
kind empty. The verb opens no
connection and grants no control; removing exits with an error whenever
something was kept or could not be read. The Get help role runs the startup
sweep that reclaims staging trees a crashed run left behind before it
connects, once the window is past the UAC step, and when the sweep keeps
files it tells the customer so in plain words, with the recovery command as
it runs in Windows PowerShell, and whether PowerShell is to be opened as
administrator for it (FT-7): the full path of
`cretlink-cli.exe` next to the program, or of the program itself piped to
`Out-Host`, because PowerShell does not wait for a GUI-subsystem program
otherwise and would show its report after the prompt has returned. The path
is a single-quoted PowerShell literal, so nothing in it is interpreted.
The sweep never removes a live session's staging: every customer-side
process holds a shared lock on `.in-use.lock` in the staging base for as long
as it runs, and the sweep runs only when it gets that lock exclusively —
otherwise it is skipped, nothing is claimed, and a later start retries. The
operating system releases the lock with the process, so a crash cannot strand
it. `recover --remove` is an explicit customer action and does not take the
lock. The same verb and sweep remain in the `cretlink-support-agent`
development tool.
The staging area and the ticket store of an elevated program lie in a folder
that only administrators can write and that is private from the moment it
exists; the folder in which earlier releases kept them, where every account
could write, is never read and is removed (FT-7, ST-M006-020). For the ticket
store this closes a way in that led nowhere far:
a ticket proves that a peer is the one of before the reboot and restores no
control, so a planted store could at most have offered a ticket to a peer
that still needs the attended consent.
The agent does not itself
register the per-user OS auto-start entry or bind redemption into a live
post-reboot dial; the
end-to-end reboot path is hardware scenario MT-8 in
[`manual-tests.md`](manual-tests.md), not yet recorded. On Windows, every
job's process tree is held in a kill-on-close Job Object (ST-M006-007), so even
a hard kill of the agent reaps it.

**Entry points and assets at risk.**

- The elevated execution broker and any optional temporary SYSTEM helper
  running on the customer computer during a session (REQ-S-012).
- Auto-start mechanisms — services, scheduled tasks, or equivalent — that
  would only ever exist to support the explicitly opted-in reboot-resume
  capability (REQ-F-021), plus any reconnect state stored on the customer disk
  for it.
- Session keys and bearer capabilities held in memory or transiently on disk
  on either endpoint (REQ-S-007).
- Files staged onto the customer computer during the session: scripts,
  utilities, installers, and drivers (REQ-F-012).
- The unfinished part of a file on its way into a folder outside the staging
  area (ST-M010-001): it lies beside its destination until the verified
  commit gives the file its name.
- The cleanup process itself, including its result reporting (REQ-F-019) — a
  cleanup that misreports success is an asset an attacker can hide behind.

**Threat actors.** A local opportunist or malware already present on the
customer computer that discovers leftover elevated components or reconnect
material after the session ends; a LAN attacker or malicious peer attempting
to replay or reuse reboot-resume authority; a malicious or negligent
technician seeking access that outlives the attended session, or an AI client
compromised or misled into requesting covert persistence; and non-adversarial
failure — a crash, power loss, forced reboot, or removal of the portable media
while cleanup is incomplete.

**Abuse cases.**

- *Leftover elevated helper.* If the execution broker or a temporary SYSTEM
  helper survives session end, it becomes a local privilege-escalation path:
  any later local process or user could attempt to drive an already-elevated
  component that no longer has a consenting session behind it.
- *Auto-start residue.* A service, scheduled task, or startup entry that
  persists beyond its purpose is functionally a backdoor, regardless of
  intent. Even benign residue erodes the customer's ability to trust that
  "support has ended" means what it says.
- *Reboot-resume token theft or replay (REQ-F-021).* Reboot-resume
  necessarily stores some reconnect authority across a restart. If that
  material could be copied — by co-resident malware, a second local user, or a
  cloned disk — and later presented, an attacker could attempt to resume a
  session the customer believed was bound to one approved reboot. Replay after
  legitimate use, use after expiry, and use against a different target are all
  variants of the same abuse.
- *Dishonest cleanup.* A cleanup routine that reports success while residue
  remains is worse than one that fails loudly: it converts a recoverable
  operational problem into false assurance for both the customer and the
  technician's records.
- *Crash during cleanup.* A crash, power loss, or premature shutdown
  mid-cleanup leaves the system in a partially cleaned state. If the product
  cannot resume or at least report that state, residue persists silently.
- *Lingering session secrets.* Session keys, bearer capabilities, or
  reconnect material remaining in memory, logs, or temporary files after
  session close extend the window in which theft is useful, including via
  crash dumps or swapped memory that CretLink does not fully control.

**Mitigations.**

- *Session-scoped privileged components.* Any administrator or SYSTEM broker
  must be scoped to the current session and removed or disabled during cleanup
  (REQ-S-012). The architecture permits a SYSTEM helper only when it is
  separately specified, visible, session-scoped, and cleaned up.
- *No auto-start by default.* Normal session shutdown must leave no auto-start
  entry or long-lived credential on the customer computer (REQ-F-001). Cleanup
  must remove temporary credentials, temporary services, temporary auto-start
  mechanisms, and CretLink-owned staged files selected for cleanup
  (REQ-F-019). Staged artifacts are digest-verified, audited, and removable
  (REQ-F-012).
- *Ephemeral session authority.* Session keys and bearer capabilities must be
  randomly generated, short-lived, scoped to one session, stored only as long
  as required, and invalidated on stop or expiry (REQ-S-007). Stop from either
  endpoint invalidates session secrets and rejects new actions (REQ-F-018,
  REQ-S-004), and cancellation terminates the tracked process trees or reports
  why it cannot (REQ-S-014). Audit logs must not intentionally store session
  secrets (REQ-S-017), and AI or provider credentials are never on the
  customer computer to leak in the first place (REQ-S-015).
- *Reboot-resume is opt-in, single-use, and expiring.* Reconnection across a
  reboot may only exist after an explicit, plainly worded local consent that
  distinguishes reboot persistence from ordinary session control (REQ-F-021,
  REQ-N-013). The reconnect capability must expire after its approved use or
  timeout, and the temporary persistence must be removed after reconnection or
  failure (REQ-F-021). Replayed authentication material must be rejected
  (REQ-S-008), and reconnection without valid session state must fail closed
  rather than restore control (REQ-S-009). ADR-0014 additionally scopes the
  reconnect authority cryptographically to the session and target, and
  redemption fails closed on expired, replayed, cloned, wrong-target, or
  interrupted state — proven by the ST-M006-002 fail-closed matrix
  (`replayed_redemption_is_refused_single_use`,
  `console_expired_clock_fails_closed`,
  `wrong_target_fails_closed_with_valid_proof`,
  `cloned_agent_blob_in_foreign_environment_fails_closed`,
  `interrupted_before_proof_fails_closed` in
  `crates/session/src/redeem/tests.rs`), with
  the reboot itself covered by MT-8.
- *Honest cleanup outcomes.* Cleanup failures must be shown prominently and
  recorded locally (REQ-F-019); the product must never silently report success
  and must distinguish completed, failed, cancelled, timed-out, and unknown
  outcomes (REQ-N-008). Lifecycle states, including crash recovery and
  cleanup, must be explicit and idempotent where practical (REQ-N-004), so an
  interrupted cleanup can be re-run rather than abandoned. The recovery command
  `cretlink recover` (ST-M006-003, ST-M009-010) inspects and, with
  `--remove`, removes known CretLink residuals without hiding evidence,
  reporting anything it keeps with a reason and manual remediation, consistent
  with the no-stealth rule (REQ-S-002).
- *Visibility as a mitigation.* Because the agent may not hide its process,
  windows, or connection state (REQ-S-002), any residue that does survive is
  at least discoverable by ordinary inspection rather than concealed, and the
  audit trail (REQ-S-017) records what was created during the session.
- *Parts outside the staging area.* A file sent into a folder is written to a
  part beside its destination. The agent remembers each such part until the
  transfer is committed or discarded, removes the remaining ones with the
  session's cleanup — on Windows inside the opened, verified folder, never
  through a link that took the folder's place — and reports a part it could
  not remove in the cleanup report, where the next cleanup tries again
  (ST-M010-001, REQ-F-019; see FT-9).

**Residual risks.**

- Persistence created through arbitrary commands. CretLink deliberately
  exposes general elevated command execution (REQ-F-007) and does not
  semantically allowlist administrator commands. A technician or a compromised
  AI client could instruct the session to create persistence that CretLink did
  not itself install and therefore cannot inventory or remove. The audit trail
  makes this detectable after the fact; it does not prevent it.
- A compromised customer computer. Malware already resident on the customer
  computer can read process memory, copy reconnect state within its validity
  window, or tamper with cleanup. CretLink narrows the useful window of stolen
  material; it does not defend a machine that is already hostile.
- Memory remnants outside CretLink's control. Secrets may transiently exist in
  crash dumps, swap, or hibernation files managed by Windows. Minimising
  secret lifetime reduces, but cannot eliminate, this exposure.
- Catastrophic failure during cleanup. If the agent is destroyed mid-cleanup
  (disk failure, forced power-off, removal of the portable media), residue can
  remain until the documented manual remediation is followed. The commitment
  is honest reporting and recoverable tooling, not impossibility of residue.
- A part beside a destination can outlive a killed agent. The agent remembers
  the unfinished parts outside the staging area in memory only, so a hard
  kill or a power loss in the middle of a transfer into a folder leaves the
  part where it is (see the residual risks of
  [File transfer and staging](#file-transfer-and-staging)).
- The reboot-resume window is persistence by design. Between the approved
  reboot and the resumed session, reconnect authority necessarily exists on
  disk. Scoping, expiry, and single-use consumption bound the risk; they do
  not make the window zero.
- Cleanup has fault-injection evidence in automated tests:
  `cleanup_failure_is_reported_then_retried_at_next_start` (a genuine OS
  refusal to remove a staged file), the ST-M006-003 inventory tests
  (`recover_keeps_unremovable_ticket_store_and_names_remediation`,
  `unreadable_ticket_store_is_surfaced_not_assumed_empty`), and
  `windows_hard_kill_reaps_the_whole_tree_via_kill_on_close` (ST-M006-007).
  Cleanup and reboot-resume on real hardware (MT-4, MT-8) are not yet
  recorded, so that evidence is still pending.

### Internet relay (M007, in progress)

**Status and scope.** M007 is under way. The opaque outbound-rendezvous
forwarder now ships as a real binary (`cretlink-relay-server`, ST-M007-007), the
console and support-agent dial it behind the same transport trait as the LAN
dial (ST-M007-008), and the relay hop is wrapped in `wss` over TLS as the
shipped default (ST-M007-009) with plain TCP kept as a documented interim. The
relay terminates only the transport envelope; it never holds the Noise session
key or decodes a protocol message, so the **binding design constraints** below
are the rules the shipped code satisfies, not merely a future commitment. The
end-to-end trust model is unchanged by the relay's presence (REQ-F-022,
REQ-S-021).

Equally important is what the endpoints expose: neither the console nor the
support agent opens an internet-reachable listener (REQ-S-010). On the LAN path
discovery and control traffic are limited to the intended local interfaces,
and the customer computer may have no internet access at all (REQ-N-003). On
the relay path both endpoints dial the relay **outbound**. The only
internet-facing listener is the operator-hosted `cretlink-relay-server`, which
binds `0.0.0.0:9970` by default by design, because a relay must be reachable
(see [`relay-server.md`](relay-server.md)); it runs on neither endpoint. Any
internet-reachable listener on an endpoint is a defect, not a feature.

**Entry points.** The endpoints expose no relay entry point and no
internet-reachable listener (REQ-S-010). The relay itself exposes a
rendezvous endpoint that the console and the support agent each reach
**outbound**, carrying a one-time preamble (an optional admission token and a
single-use rendezvous ticket) so the relay can match the two ends of a session;
a forwarding channel that carries opaque, end-to-end encrypted session frames
between the matched endpoints; and relay-operator surfaces (admission
authentication, rate limits, concurrency quotas, and metadata-only metrics and
event logs) for self-hosted deployments. The relay answers each preamble once,
with a fixed frame that says "admitted" or names the reason of a refusal
(ST-M007-012); it is the only thing the relay itself ever writes to an
endpoint.

The ticket of a session is derived from a **relay code** that the technician
reads to the customer, and from the host name of the relay that both programs
have in their settings (ST-M007-012). The code is a third thing a person
reads or types, next to the LAN connection code and the approval code, and
like the first it is addressing only.

**Threat actors.** A malicious or compromised relay operator — the defining
actor for this boundary, modelled as fully hostile: it may observe, store,
delay, drop, replay, reorder, or modify every frame it forwards, and may
attempt to act as an endpoint (this includes a hostile self-hosted relay run
by a third party, since the relay is intended to be self-hostable open
source); a network attacker between an endpoint and the relay, including the
relay's hosting provider, with the same capabilities; a relay impersonator who
convinces endpoints to rendezvous through an attacker-controlled relay;
abusive third parties who would use a reachable relay as an anonymising
rendezvous for unauthorised remote access or scanning, independent of any
legitimate session; and a traffic-analysis observer harvesting session
metadata rather than content.

**Abuse cases.**

- *Plaintext recovery.* The relay attempts to read commands, command output,
  transferred files, or screenshots from forwarded traffic, or to downgrade
  the channel to a relay-readable mode.
- *Command authority via the relay.* The relay attempts to synthesise or
  modify tool calls, impersonate an authenticated console or support agent, or
  replay and reorder recorded frames so that the customer computer executes
  actions no endpoint requested.
- *Relay-in-the-middle at pairing.* An attacker-controlled relay inserts
  itself as each endpoint's "peer" during pairing, hoping the humans confirm a
  session that actually terminates at the relay.
- *Consent and revocation interference.* The relay withholds or delays
  traffic to keep a session apparently alive after a stop, to suppress a
  revocation, or to encourage skipping the code comparison and consent steps
  on the internet path.
- *Metadata harvesting.* The relay records who supported whom, endpoint IP
  addresses, session timing, frequency, and traffic volumes.
- *Anonymous abuse service.* A publicly reachable relay is used by parties
  with no legitimate session as free infrastructure for abuse, creating legal
  and reputational exposure for operators.
- *Scope creep in the endpoints.* A convenience feature quietly introduces an
  internet-reachable listener on an endpoint, or makes the LAN workflow depend
  on a relay, bypassing the M007 requirements, review, and adversarial
  testing.

**Mitigations (binding design constraints for M007).**

- *No internet exposure on the endpoints.* Endpoints open no
  internet-reachable listener (REQ-S-010): the LAN path limits discovery and
  control to local interfaces, and on the relay path both endpoints dial
  outbound. The operator-hosted `cretlink-relay-server` binds `0.0.0.0:9970` by
  default by design and runs on neither endpoint. The LAN-only workflow remains
  available alongside the relay (M007 exit criteria), and initial session
  functions work with no customer internet access at all (REQ-N-003).
- *Relay is a forwarder, never an authority.* A relay may only be added
  without changing the end-to-end trust model and without exposing remote
  execution to the relay (REQ-F-022). It forwards opaque end-to-end encrypted
  traffic and never receives session plaintext, session keys, or
  remote-execution authority (REQ-S-005, REQ-S-007).
- *Endpoint authentication excludes the relay.* Pairing authenticates the two
  endpoints — not the transport between them — using established, audited
  cryptography, and binds the human-compared SAC to the cryptographic session
  transcript (REQ-S-005, REQ-S-006, REQ-F-005). A relay-in-the-middle that
  terminates the cryptography at itself produces mismatched codes and cannot
  reach full control.
- *No relay-driven replay or downgrade.* The protocol must reject replayed
  authentication messages and prevent any peer — including a relay posing as
  one — from silently downgrading required security properties (REQ-S-008).
  Messages carry explicit schemas, versions, identifiers, and bounds
  (REQ-N-012), and flow control and quotas bound relay-driven resource
  pressure at the endpoints (REQ-N-006).
- *Fail closed through the relay.* Authentication, integrity, or
  version-negotiation failures on a relayed path deny control with a visible
  error, and reconnecting through a relay cannot restore full control without
  valid session state (REQ-S-009). Session keys are ephemeral, scoped to one
  session, and invalidated on stop or expiry (REQ-S-007), so a relay that
  records traffic gains nothing durable.
- *Revocation does not depend on relay cooperation.* Either endpoint can
  revoke unilaterally, and stop controls are enforced at the endpoints
  (REQ-S-004, REQ-F-018). A relay that withholds or delays frames can degrade
  availability but cannot keep a revoked session authoritative.
- *Consent, identity, and audit are unchanged by transport.* Attended local
  initiation, informed full-control consent, explicit target identity on
  results, and the local audit trail apply identically on a relayed path
  (REQ-S-001, REQ-F-006, REQ-F-016, REQ-S-017). AI and provider credentials
  remain on the technician computer and never transit to the relay or the
  customer device (REQ-S-015).
- *Abuse controls before operation.* M007 requires relay authentication, rate
  limits, abuse controls, and operational metrics so a reachable relay is not
  an anonymous abuse service, plus self-hosting documentation with verifiable
  release provenance (REQ-S-019; M007 scope covering REQ-S-017 through
  REQ-S-022).
- *A relay behind a proxy, and the relay as an image.* A relay started with
  `--behind-proxy` serves WebSocket without TLS, for a reverse proxy that ends
  TLS in front of it (ST-M007-015). The proxy then sees what the relay sees,
  ciphertext with its size and timing, and the rendezvous preamble with the
  admission key crosses the hop between them in the clear: that hop must be
  one host or a private network, and the relay says so when it starts. The
  mode excludes a certificate of the relay's own, and a switch that is neither
  on nor off is refused instead of read as off, so a relay never ends up
  without TLS by a typing error. The container image holds the one program
  and its licence texts on an empty base and runs as a user without rights;
  it is published only by the release workflow, with a provenance attestation
  that names the commit, which an operator verifies before running a version.
- *One admission key per customer.* An operator gives every customer copy and
  technician a key of its own and withdraws one by deleting its line from the
  relay's key file, without a restart and without touching the others
  (ST-M007-014). A key that leaks, for example with a customer copy that was
  passed on, lets a stranger use the forwarder under that label until it is
  withdrawn. It reveals no session and controls none: a key is checked once,
  when a connection arrives, and is never session authority (ADR-0016 §4). The
  relay's log, events and metrics name the label and never the key, and a key
  file that breaks a rule is refused as a whole, so a typing error neither
  opens the relay nor locks everybody out.
- *A relay profile is addressing, and its file is untrusted input.* A copy
  learns its relay from a relay profile: the relay's address, an optional
  admission key, an optional certificate authority file and an optional name
  in the certificate (ST-M007-011). It is read from `cretlink.toml` next to
  the program, by both roles, or from the settings of the installed technician
  copy; the customer role reads the file next to its program and nothing
  else, and writes nothing. Nothing in a profile grants control (ADR-0016 §2,
  §4). Somebody who can write next to the program or into the settings can
  point a copy at a relay of their own, which is then the hostile relay this
  section already models: it sees metadata and can deny service, and the
  approval codes differ when it puts itself in between. The file may have
  been written by somebody else, so it is read up to a fixed size in a small,
  strict form: every unknown section and setting is refused, every setting is
  bounded, the certificate file must be a plain name next to the profile, and
  a file that cannot be used is explained before any connection and never
  passed over for another relay. An explanation names the line and the
  setting and never repeats text from the file.
- *The application speaks only `wss` to a relay.* A profile has no setting
  that turns TLS off and an address with a scheme is refused. The relay's
  certificate is validated against the system's authorities, or against the
  profile's certificate file and nothing else when it names one, for the
  profile's name or else the host of its address. A certificate that is not
  trusted, or that carries another name, ends the dial before the rendezvous
  preamble is written, so neither the admission key nor the ticket leaves the
  computer, and the application never falls back to a connection without TLS.
- *The admission key stays where it is needed.* It is held in a type that is
  erased when dropped and redacted in debug output. The application never
  shows it again once it is saved, never logs it, puts it in no audit record
  and in no error text; a form that leaves the key alone tells the backend to
  keep it instead of sending it back. A kept key stays with the relay it was
  saved for: when the form names another relay, by another host name for its
  certificate, the application refuses to keep the key and asks for that
  relay's own, so a test or a save never presents one relay's key to
  another. The test of a relay in Settings presents a ticket of its own,
  meets nobody and carries nothing of a session.
- *A relay code grants nothing.* It holds a random seed of 64 bits and a
  typing check, no address and no key. Someone who overhears or guesses a code
  can at most reach the rendezvous before the customer does: the approval
  codes then differ on the two screens and the session ends before consent
  (REQ-S-006, REQ-S-009). The relay's rate limit bounds guessing, a ticket
  works once, and a code is minted for one session. Someone who holds the
  code and joins without speaking can hold up one parked leg; the waiting
  side gives up a silent leg after 45 seconds and parks a new one. Because
  the relay's host name enters the derivation, a code read
  aloud names nothing on any other relay. The code is named and drawn
  differently from the approval code, which alone establishes trust.
- *The relay's answer says only what admission decided.* The reasons it can
  name (no key, an unknown key, the rate limit, the quota, not available,
  another preamble version) concern the use of the forwarder, never a session
  (ADR-0016 §4). The answer carries no credential, no ticket and no session
  data, and an endpoint never takes an unknown status for an admission. To a
  stranger it confirms what the port already shows, that a relay runs there
  and whether it asks for a key; something that sends no preamble gets no
  answer.
- *Adversarial validation gates release.* M007 cannot exit until tests
  demonstrate that a relay which observes, delays, drops, replays, reorders,
  or modifies traffic gains neither plaintext nor command authority (M007 exit
  criteria). The shipped relay-server and endpoint dialer are proven against
  these on the raw-TCP and `wss` paths by the relay integration suites, which
  assert byte-identical pairing, consent, attribution, and revocation to a direct
  LAN session and zero forwarding on a fail-closed transport.

**Residual risks.**

- Metadata exposure is inherent. Any relay necessarily learns endpoint
  addresses, session existence, timing, duration, and traffic volume, and can
  attempt traffic analysis on encrypted frames. CretLink commits to measuring
  and documenting this exposure, not to eliminating it; padding and anonymity
  networks are not planned.
- Availability is not defended. A hostile relay can always refuse, delay, or
  degrade service. The guarantee is fail-closed denial of control, not
  liveness.
- The admission key stands in readable form on disk: in `cretlink.toml` next
  to a copy, and in the settings of the installed technician copy. The
  settings file is created for the user only where the system has file modes
  (`0600` on macOS and Linux) and has the access rules of the user's
  `%LOCALAPPDATA%` folder on Windows; a customer copy is a folder that is
  handed over, and whoever has it has its key. A key admits to the forwarder
  and nothing else, and one key per customer bounds what a leaked one is
  worth until its line is deleted.
- Third-party relay deployments. As MIT-licensed software, relay code can be
  self-hosted by anyone; the project cannot police hostile or negligent
  deployments beyond making endpoint security independent of relay honesty.
- Transport TLS crypto provider (accepted). The `wss` transport (ST-M007-009)
  terminates TLS with `rustls` and its default **`aws-lc-rs`** crypto provider
  (ADR-0017, ST-M006-009). It replaced the pure-Rust `rustls-rustcrypto`
  provider, an alpha whose graph carried `rsa` with the unfixed Marvin timing
  side channel (RUSTSEC-2023-0071) and the unmaintained `paste`. The residual is
  that the TLS primitives now run in AWS-LC's C and assembly code, outside
  Rust's memory-safety guarantees, in exchange for a far more widely used and
  audited, partly formally verified implementation; building the transport
  needs a C compiler. The provider stays isolated behind the `cretlink-wss`
  config builders, and the end-to-end Noise session is unaffected by the
  provider choice (REQ-S-005, REQ-S-021).
- Scope discipline still applies to what remains. Screenshot/computer-action
  surfaces and relay abuse-at-scale hardening are not yet exercised on hostile
  hardware; the residual risk is an internet-facing capability outrunning its
  malicious-relay testing, mitigated procedurally by the spec-first gate and
  REQ-S-010 until each remaining part lands its own code and test evidence.
- Compromised endpoints remain out of scope. End-to-end encryption terminates
  on the technician and customer computers; a relay design cannot protect a
  session whose technician computer is already compromised.

### One application, two roles (M009)

**Status.** Implemented in `apps/app` (ST-M009-005, ST-M009-009, ADR-0018);
the UAC prompt, the compact bar and a Windows run of both roles against each
other are manual checks (`docs/manual-tests.md`, MT-12).

**What changed.** The technician console, the customer's support window and the
command-line subcommands are one program, `cretlink`. The customer runs the
same file as the technician, so technician code now sits on the customer
computer, unused.

**Entry points.** The start screen (Windows), the file name (a copy named
`CretLink-Support*` opens Get help), the `--role technician | customer`
argument the application uses to start a role (with `--from-start` for Get
help from the start screen, ST-M009-009), and the subcommands.

**Threats and mitigations.**

- *A customer-side process acts as a technician.* The role is decided before
  any window opens and is fixed for the life of the process. Each role
  registers only its own Tauri commands; the commands that bind the MCP
  bridge's IPC endpoint and the session listener exist only in a technician
  process, so a customer process has no way to reach them, and the reverse. A
  `CretLink-Support*` copy refuses subcommands and `--role technician`, so a
  file handed to a customer cannot serve MCP or open the console
  (REQ-F-024). Its one accepted subcommand is the offline `recover
  [--remove]` (ST-M009-010), which opens no connection and grants nothing.
- *Silent or repeated elevation.* Get help elevates only through the visible
  UAC prompt (`ShellExecuteExW`, `runas`), and only after the unelevated
  window has explained it and the customer pressed **Continue** (`#c-admin`,
  ST-M009-009); declining starts nothing, and the window says honestly that
  administrator tasks will not work (REQ-F-002). The relaunched process gets
  exactly `--role customer` and never explains or relaunches again, so there
  is no prompt loop; it reports its real elevation state (REQ-S-011).
  **Continue without admin rights** goes on in the same unelevated process,
  and the identity says "Admin rights: No". The backend refuses a dial before
  the step is passed. Elevation grants no control: pairing, the Session
  Approval Code and the informed consent still gate everything (REQ-S-001,
  REQ-S-003).
- *Something else starts `cretlink --role customer`.* That is the same as the
  customer double-clicking it: a visible window (elevated only through a UAC
  prompt somebody answered), no session without the customer's actions. It
  adds no hidden path (REQ-S-002). `--from-start` only changes where **Back**
  goes.
- *Persistence through the application.* Get help installs nothing and adds no
  auto-start entry (REQ-F-001, REQ-F-019). Only the technician role installs,
  per user (ST-M009-006, below); the customer role never offers it, and a
  `CretLink-Support*` copy refuses `install` and `uninstall`.
- *Missing WebView2 runtime.* The program says so in a native message box and
  starts nothing, rather than failing in a half-open state.

**Residual risks.** The customer computer carries technician code it never
runs; a flaw that reached arbitrary code execution inside the customer process
could use it, but such a flaw would already control an elevated process. The
customer depends on the WebView2 runtime the technician console already needed
(ADR-0018).

#### The customer window (ST-M009-009)

**What it adds.** The customer interface of the mock: a state band in words on
every screen, the untrusted list of nearby consoles, the compare and consent
screens, the live activity with **Stop now**, a second always-on-top window
(the compact bar), the close question and the end report.

**Threats and mitigations.**

- *Hiding the session from the customer.* While a session exists, the main
  window or the compact bar is always visible: **Make small** hides the main
  window only after the bar is shown, minimising the main window shows the
  bar, the bar refuses to close during a session, and when the session ends
  the main window comes back with the end report (REQ-S-002). The bar is
  always on top and in the taskbar. Its state is in words, never colour alone
  (REQ-N-013).
- *Closing the window without revoking.* A close of the main window during a
  session — its close button, `Alt+F4`, or an exit request — is held in the
  backend (`CloseRequested`, `ExitRequested`) and the window asks "Stop the
  session and close?". **Stop and close** revokes the session, waits (bounded)
  for the cleanup report, then exits; outside a session the window closes at
  once. A close right after **Stop**, while the session task still tears
  down, takes the same bounded wait. A process that is killed still drops the
  encrypted channel, which the technician side sees as a lost connection
  (REQ-S-004, REQ-S-009).
- *A Stop that never reaches the wire.* The window goes safe at once, so the
  session pump must not mistake its own stopped model for the end of the
  session: it prefers queued intents over wire events and sends any Stop or
  decline still queued before it waits for the teardown (REQ-F-018). A
  finished dial that is still tearing down writes only its own state, never
  the next dial's (each dial has a generation).
- *A false "stopped" state.* Stop and `Escape` exist only while a session
  exists; on the resting window the backend refuses a stop rather than
  showing a session that never existed.
- *A forged entry in the nearby list.* Each entry shows the name and host name
  the console advertises — the technician's computer advertises its host name
  as the DNS-SD instance name — but nothing is preselected, nothing connects
  on its own, and an entry grants nothing: trust still comes only from the
  Session Approval Code (ADR-0007, REQ-S-006). The list is capped and every
  field is written as text.
- *Injected text in the activity.* Purposes, operations, paths and names from
  the technician's side are written only as text nodes, never parsed as
  markup (REQ-S-018). The stated purpose is shown beside the real operation,
  never instead of it (REQ-F-026); a file leaving the computer is said in
  words.
- *The second window as a new command surface.* The compact bar loads the same
  frontend and uses the customer role's commands only; it gets the same core
  capability as the main window and nothing more.

**Residual risks.** A customer who reads only the advertised name, not the
code, can still be misled by a forged console (as before). The Screen tile is
only as current as the agent's `DesktopActivity` signals, and desktop control
is not offered by this window yet (M005). A forced kill of the customer process
(Task Manager) cannot ask first; the session then ends as a lost connection.

#### Run once or install the technician role (ST-M009-006)

**What it adds.** *Install for me* copies `cretlink` and `cretlink-cli` to a
location fixed per operating system (`%LOCALAPPDATA%\Programs\CretLink\`,
`~/Applications/CretLink.app`, `~/.local/bin/`), adds a Start-menu shortcut
and an uninstall entry under
`HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\CretLink`
(Windows) or a desktop entry (Linux), registers the selected AI clients with
the installed path, and records all of it in `install.json`. A *run-once* copy
writes nothing of its own but the audit trail.

**Threats and mitigations.**

- *Hidden persistence.* The install is per user and needs no administrator
  rights. It creates no service, no scheduled task, no Run key and no login
  item: nothing starts CretLink except the user (the shortcut, the desktop
  entry or the bundle) or an AI client the technician connected. The only
  registry entry is the uninstall entry, which is visible in *Installed apps*
  (REQ-F-024, REQ-F-019).
- *An install that cannot be removed completely.* Uninstall (`cretlink
  uninstall`, Settings, or the uninstall entry) shows what it will remove and
  asks first (CLI: unless `--yes`). It removes the files, the shortcut or
  desktop entry, the uninstall entry and the recorded registrations, and
  reports anything it could not remove instead of claiming success. The audit
  trail stays on purpose, and the uninstall says so.
- *A tampered `install.json` turns uninstall into a deletion primitive.*
  Uninstall deletes only the fixed paths of the layout, never a path read from
  the record, and reads at most 64 KiB of it. The record decides only which
  AI-client registrations to remove, and each is removed only while it still
  starts the installed program, through the client's own command line.
- *A registration the technician changed by hand.* It no longer points at the
  installed program, so uninstall leaves it alone and says so.
- *Replacing a running copy.* Install and uninstall stop when the installed
  program runs in another process (including as `cretlink mcp` under an AI
  client) and say what to close; nothing is half-replaced.
- *Removing the running program on Windows.* A program cannot delete its own
  file there, so an uninstall started from the installed copy leaves the folder
  to a detached, window-less Windows PowerShell started by its absolute path
  under `%SystemRoot%`. It waits for the uninstalling process to exit
  (`Wait-Process`, without a time limit, so it never removes under a running
  program), removes only the program files and the
  record, and the folder only when it is then empty. `cmd.exe` cannot wait for
  a process id, which is why PowerShell is used; the script is cmdlets only and
  passes paths as single-quoted literals.
- *A customer copy installs itself.* The customer role never shows the
  install choice, and the install and uninstall functions refuse a
  `CretLink-Support*` program as well as the launch rules (REQ-F-024).

**Residual risks.** The install is not code-signed yet (ST-M006-005); on
macOS the generated bundle is unsigned and Gatekeeper may ask. The webview
keeps its own profile in a per-user data folder (Tauri's `dev.cretlink.app`)
for run-once and installed copies alike; uninstall does not remove it. An AI
client registration points at the installed path, so anyone who can replace
that per-user file can run code as the user when the client starts it — the
same as any per-user program, and the folder is the user's own.

### Supply chain and distribution

This surface covers everything between the public CretLink source code and the
binaries that actually run: third-party dependencies, the build and CI
pipeline, release artifacts and their distribution, and repository secret
hygiene. It deserves particular care because the support agent runs elevated
on other people's computers. A compromised official artifact inherits
administrator authority on every customer machine it touches, and the customer
has no practical means of inspecting the binary the technician asks them to
run. The architecture therefore treats repository source, dependencies, CI,
build artifacts, release metadata, and signing identities as having different
trust levels, and requires official artifacts to have verifiable provenance
independent of the fact that the source is public.

**Status.** Enforced on `main` and in the release workflow. The Cargo
workspace with its committed lockfile and `deny.toml` (ST-M001-002), the
continuous-verification pipeline (ST-M001-004), and semantic-release automation
(ST-M001-005) are configured and verified. Official artifacts — one archive
per operating system with the application and its console build, the relay
server, SHA-256 checksums, a dependency inventory, and CycloneDX SBOMs — are
published by the release workflow (ST-M006-004, ST-M009-004). Authenticode
signing is implemented but gated, and no signing identity is configured, so
current releases are unsigned (ST-M006-005); build provenance attestation is
not yet implemented. A ruleset on `main` accepts changes only through a pull
request that passed the required checks, merged as a squash commit, and
rejects force pushes and deletion.
Each mitigation below is marked with its honest implementation status.

**Entry points.**

- crates.io dependencies resolved by Cargo for the Rust workspace (ADR-0001),
  including transitive crates, build scripts, and procedural macros that
  execute code at compile time on maintainer and CI machines.
- Build tooling: the Rust toolchain and any tools fetched by CI.
- Repository content and access: source, GitHub Actions workflow definitions,
  branch-protection configuration, and maintainer accounts.
- CI jobs: pull-request jobs that execute contributed code, and release jobs
  that hold publishing credentials.
- Release artifacts: one archive per operating system (Windows x86_64, macOS
  aarch64, Linux x86_64) holding exactly two programs: the application
  `cretlink` (`cretlink.exe`, both roles, and the program the customer runs)
  and its console build `cretlink-cli` (ST-M009-010, ADR-0018); the loose
  `cretlink-relay-server`; `SHA256SUMS.txt` over the archives,
  per-target checksums, the dependency inventory, SBOMs, and the generated
  release notes. The version stamped into the binaries is computed before the
  build, and semantic-release refuses to publish under any other version
  (ST-M006-014).
- The distribution path to the customer: download on the technician computer,
  copy to removable media or a temporary local copy, and visible elevated
  launch on the customer computer (REQ-F-001, REQ-F-002).
- The documentation site: built from `website/` on `main` and published
  through GitHub Pages (`pages.yml`, ST-M011-001). It offers no download of
  its own; it links to the release page, where the checksums are.

**Threat actors.** A compromised or malicious dependency maintainer
(crates.io account takeover, a hostile transitive dependency, or a
typosquatted crate name added by mistake); an attacker with repository or
workflow access (a compromised maintainer account, or a malicious pull request
that attempts to alter CI behaviour or reach release credentials); an artifact
tamperer or impersonator (someone who modifies binaries after build, or
distributes altered or lookalike `cretlink.exe` or `CretLink-Support*.exe`
files, or programs under the retired names `cretlink-support-agent`,
`cretlink-console`, `cretlink-mcp` or `cretlink-shell`, outside the official
release channel); and a careless insider (accidental commit of
tokens, signing material, or test fixtures containing real credentials).

**Abuse cases.**

- *Backdoored dependency.* A malicious crate executes code at build time or
  ships hostile runtime behaviour inside the support agent. Because the agent
  runs elevated on customer machines, this is the highest-impact supply-chain
  case: the backdoor bypasses every runtime consent and visibility control,
  since those controls are implemented by the very binary that was
  compromised.
- *CI or workflow compromise.* A malicious pull request tries to run with
  release credentials, exfiltrate secrets from CI, or change the pipeline so
  that published artifacts no longer correspond to the tagged source.
- *Release-artifact tampering.* Binaries are replaced or modified between
  build and download, or a third party mirrors modified binaries that
  technicians mistake for official releases.
- *Unsigned-binary impersonation.* Until code signing exists, a lookalike
  binary can imitate CretLink's Get help consent screens while omitting the
  safety properties behind them. Customers cannot cryptographically
  distinguish it, and repeated Windows SmartScreen warnings on the genuine
  unsigned artifact risk training users to click through warnings generally.
- *Update-channel abuse.* If automatic updates are ever added, a hostile
  update or a silent downgrade to a vulnerable version would convert the
  update channel into a remote-execution channel.
- *Repository secret leakage.* A leaked release token or signing key lets an
  attacker publish artifacts that appear official.

**Mitigations.**

*Dependency compromise.* CI enforces a dependency policy with `cargo-deny`:
known RustSec advisories, violations of the MIT-compatible licence policy,
banned or yanked crates, and wildcard or unknown-source dependencies fail the
build, duplicate crate versions are surfaced as warnings, and dependency
review plus secret scanning run on pull requests (REQ-N-010, REQ-N-016,
REQ-N-017; M001 exit criteria, ST-M001-004) — *configured and enforced on
`main` (ST-M001-004, Done; ST-M006-017): `ci.yml` runs `cargo-deny` licence,
advisory, ban, and source checks on every pull request that changes more than
documentation, and again on `main` after the merge, and `hygiene.yml` runs
gitleaks secret scanning and the private-data check (ST-M006-016) on every
pull request, with all actions SHA-pinned. The ruleset on `main` requires the
result of both, merges a pull request only when it is up to date with `main`,
so the merged tree is the tested one, and rejects a direct push, which the
[release policy](releasing.md) forbids*. The committed Cargo lockfile pins the
exact dependency graph, so a release builds from reviewed, recorded versions
rather than whatever resolves on the day — *in place: the Cargo workspace,
`Cargo.lock`, and `deny.toml` are committed (ST-M001-002, Done)*.
Security-critical functionality uses established, maintained cryptographic
libraries rather than novel code (REQ-S-005), which keeps the set of crates
that must be deeply trusted small and auditable.

*Build and CI compromise.* Privileged tests and release credentials are
isolated from untrusted pull-request execution: routine CI runs against the
non-privileged simulated agent, and release secrets and signing keys never
enter untrusted pull-request jobs (REQ-N-011, REQ-N-017; [release policy](releasing.md))
— *configured on `main` (ST-M001-004, Done): pull-request jobs carry no write
tokens, and the release workflow runs only on `main` behind a fork guard*.
A pull-request job reads no secret besides its read-only token: the
maintainer's list of private terms for the private-data check (ST-M006-016)
stays on the maintainer's computer and is applied there before a push.
Only protected release automation may publish from the release branch;
versions, tags, and changelogs come from semantic-release rather than manual
steps that an attacker could imitate (REQ-N-018, REQ-N-019, ST-M001-005) —
*configured on `main` (ST-M001-005, Done): `release.yml` runs semantic-release
with a pinned `.releaserc.json`, `package.json`, and `package-lock.json`; the
only manual tags are the seed tags `v0.0.0` and `v0.21.1`, and releases are
computed by semantic-release*.

*Release-artifact tampering and unsigned binaries.* Official releases must be
produced by documented automation from a tagged commit and ship with
checksums, so a technician can verify an artifact before copying it to
removable media, and any observer can check that a binary in circulation
matches the published release (REQ-S-019) — *in place: the release workflow
publishes checksums, a dependency inventory, and CycloneDX SBOMs with every
release (ST-M006-004); build provenance attestation is not yet implemented
([release policy](releasing.md))*. Code signing is a stated goal (REQ-S-019,
SHOULD) — *Authenticode signing is implemented and gated on a configured
signing identity (ST-M006-005); none is configured, so current releases are
unsigned*. Staged tools and drivers transferred during a
session are verified by cryptographic digest and recorded in the audit trail,
which limits silent substitution during the last hop onto the customer machine
(REQ-F-012, REQ-N-007).

*Documentation site.* A site that an attacker could change would send
technicians to a lookalike download. Only the publish job of `pages.yml` may
write to Pages, through the `github-pages` environment that accepts `main`
alone; pull-request jobs build the site with a read-only token and publish
nothing, and the site's build dependencies are pinned in its own lockfile and
audited in CI — *configured (ST-M011-001)*. The site carries no programs: a
download always comes from the release page and is verified against its
checksums ([`verifying-artifacts.md`](verifying-artifacts.md)).

*Update authenticity.* The initial product has no automatic update mechanism.
If one is introduced, update metadata and artifacts must be authenticated
before execution and downgrade behaviour must be explicit (REQ-S-020) — *not
applicable yet; binding on any future update work*.

*Blast-radius limits and disclosure.* No AI credentials, provider tokens, or
technician account secrets ever reach the customer computer (REQ-S-015,
REQ-F-015), so even a tampered support agent has nothing of that kind to steal
from its own host. The repository maintains this public threat model
(REQ-S-021) and a private coordinated vulnerability-reporting path
(REQ-S-022), so a discovered supply-chain compromise can be reported and
handled without forcing public disclosure of exploitable detail —
*SECURITY.md is published alongside this threat model (ST-M001-006)*.

**Residual risks.**

- Checksums alone do not prove authorship. Checksums published alongside the
  artifacts defend against corruption and casual tampering, but not against an
  attacker who controls the release channel itself. Until signing (REQ-S-019
  SHOULD) and independent provenance attestation exist, artifact authenticity
  ultimately rests on GitHub account security and branch-protection integrity.
- Customers cannot verify the binary. With current releases unsigned, the
  customer's trust in the support agent is trust in the technician who brings
  it.
  CretLink cannot prevent someone from distributing a hostile lookalike
  outside official channels; pairing and the SAC protect genuine endpoints
  from each other, not the customer from a fake agent they were handed.
- Advisory-based scanning lags novel attacks. `cargo-deny` catches known
  vulnerabilities and policy violations, not a well-executed malicious
  dependency change before public disclosure. Dependency-diff review in a
  personal open-source project is best-effort, not a guarantee.
- Bit-for-bit reproducible builds are not promised initially. REQ-S-019
  requires documented automation and checksums; full reproducibility that
  would let third parties independently rebuild and compare artifacts is a
  desirable later hardening, not an M001 deliverable.
- Current releases are unsigned. The CI, dependency, and release gates above
  are configured and enforced on `main` (ST-M001-002, ST-M001-004, and
  ST-M001-005 are Done), and releases ship with checksums, a dependency
  inventory, and SBOMs, but no signing identity is configured and build
  provenance attestation is not yet implemented. Until then, checksums
  verified against the official release page are the practical integrity
  assurance.

## Residual risks and explicit non-goals

The boundary sections above list their own residual risks. The following
cross-cutting risks are accepted deliberately and stated here once so nobody
has to infer them:

- **A fully compromised technician computer is out of scope.** It holds the AI
  credentials, the console, target selection, and the audit log; no
  customer-side control can make it harmless. The customer's protections
  against a hostile technician machine reduce to attendance, the visible
  session UI, and the stop control (REQ-S-002, REQ-S-004, REQ-F-018). This is
  a stated trust-boundary fact, not a mitigation gap.
- **A customer computer that is already compromised is out of scope.** Malware
  resident before the session can subvert the agent's own side of pairing,
  consent, cleanup, and reboot-resume material. CretLink narrows windows and
  keeps everything visible; it does not disinfect or defend a hostile host.
- **Abuse of genuinely granted authority cannot be prevented, only exposed.**
  There is no semantic allowlisting of administrator commands, no "safe paths"
  filter, and no way to distinguish a legitimate log download from
  exfiltration. The defence throughout is attended consent, visibility,
  attribution, audit, and immediate revocation (REQ-S-001..004, REQ-F-017,
  REQ-F-018, REQ-S-017) — detection and interruption, not prevention.
- **Human factors remain a real weakness.** Blind confirmation of the Session
  Approval Code, consent under social pressure, and a technician not watching
  the activity view all reduce the value of the technical controls. UI
  requirements (REQ-N-013, REQ-N-014, REQ-S-003, REQ-S-016) reduce but cannot
  eliminate this.
- **Prompt-injection resistance of the AI model is outside CretLink's
  control.** CretLink guarantees hostile reflected text has no mechanical
  authority over the session; it cannot guarantee the model is not persuaded
  by it.
- **Availability under attack is not guaranteed.** Denial of service on the
  LAN, discovery jamming, or a hostile relay can prevent a session from
  forming or surviving. CretLink commits to failing closed and reporting
  honestly, never to liveness.
- **Metadata is observable.** Session existence, endpoint addresses, timing,
  and traffic volume are visible to network observers and, on the relay path,
  to the relay. Traffic-analysis resistance is out of scope.
- **Data sent to an AI provider cannot be retracted.** Disclosure before
  consent (REQ-S-016) is the control; recall is not possible.
- **The audit trail is technician-held.** It is append-oriented and supports
  honest review; it is not tamper-proof against the person who controls the
  console (REQ-S-017).
- **Executed effects are irreversible.** Stop fails closed for future actions
  but cannot undo changes already made, and an elevated command can in
  principle degrade CretLink itself on the customer computer.
- **Memory remnants are bounded, not eliminated.** Secrets may transiently
  exist in crash dumps, swap, or hibernation files managed by Windows.
- **Provenance is checksum-strength for now.** Current releases are unsigned;
  code signing (REQ-S-019 SHOULD) awaits a signing identity, provenance
  attestation and bit-for-bit reproducible builds are not implemented, and
  artifact authenticity rests meanwhile on release-channel account security.
- **Per-section status is authoritative.** Each boundary section states what
  is implemented; mitigations in sections marked as design commitments are
  enforced by the spec-first gate and traceability, not by shipped code, and
  are not claims about shipped behaviour.

### What CretLink refuses to contain

Some capabilities are excluded on purpose, permanently, because their presence
would convert a support tool into an attack tool. These are requirements and
product non-goals, not implementation gaps, and contributions adding them will
be rejected:

- **No stealth.** The support agent must never hide its process, window, tray
  state, connection state, logs, or active-control indication from the
  customer (REQ-S-002). Covert monitoring and anti-forensics are explicit
  product non-goals.
- **No consent bypass.** Full control exists only after visible local
  initiation, visible UAC elevation, and informed consent on the customer
  computer (REQ-S-001, REQ-F-002, REQ-F-006). CretLink does not bypass
  customer consent, UAC, operating-system protections, endpoint security, or
  organisational policy (product non-goals).
- **No credential capture.** No credential harvesting, password extraction,
  browser-session extraction, keylogging, or mechanisms designed to bypass
  operating-system authentication (REQ-S-013).
- **No hidden or permanent persistence.** Hidden, unattended, or permanent
  access by default is a product non-goal; normal shutdown leaves no
  auto-start entry or long-lived credential (REQ-F-001, REQ-F-019), privileged
  brokers are session-scoped (REQ-S-012), and the only cross-reboot mechanism,
  reboot-resume, is explicitly armed by the customer, single-use, and expiring
  (REQ-F-021, ADR-0014; hardware scenario MT-8).
- **No AI credentials on the customer computer.** AI authentication and
  provider communication remain on the technician computer (REQ-S-015,
  REQ-F-015).
- **No invented cryptography and no unauthenticated encryption** (REQ-S-005).
- **No internet-reachable listener on an endpoint by default** (REQ-S-010);
  the relay remains optional and never an authority (REQ-F-022).

Because CretLink is MIT-licensed, a fork could remove these refusals. The
project cannot prevent that; it can ensure the official artifacts are
verifiable (REQ-S-019) so a modified binary is distinguishable from an
official release, and it will not accept such changes upstream.

## Maintaining this document

REQ-S-021 requires this threat model to cover pairing, LAN attackers,
malicious peers, compromised AI clients, command execution, file transfer,
temporary persistence, relay operation, and supply-chain risks — and to be
kept current. Per the security acceptance evidence in the
security requirements, **every milestone that changes a trust
boundary must ship with**: abuse and misuse cases; negative authorisation
tests; cleanup and revocation tests; dependency and artifact provenance
checks; an updated threat-model section; and a record of unresolved risks.
Pull requests that alter a trust boundary must therefore update the relevant
section here in the same change.

For tabletop review (ST-M001-006), the walkthrough scenarios map onto this
document's sections as follows:

- hostile LAN → LAN attackers, with the pairing-time cases in Pairing and
  session establishment;
- malicious console → Malicious peers (the rogue-console abuse cases);
- compromised AI client → The AI client: compromised AI and prompt injection;
- malicious target → Malicious peers (the tampered support agent), the AI
  client section (reflected content), and Command execution (hostile code on
  the target);
- supply chain → Supply chain and distribution.

Known upcoming triggers for revision:

- the maintainer-run hardware half of the privileged adversarial and negative
  execution suite (ST-M003-007) and the other hardware scenarios in
  [`manual-tests.md`](manual-tests.md), including the reboot-resume run (MT-8)
  and the file operations of the explorer (MT-14), being recorded;
- a way to tell the agent that a copy was given up, and a session service
  that serves other requests while a file is on its way, which would lift two
  of the residual risks of the explorer (FT-13);
- promotion of desktop observation and control (M005) or the technician
  terminal (M008) out of draft, which must record their actual status and any
  new residual risk in the
  [Desktop observation and control (M005)](#desktop-observation-and-control-m005)
  and terminal sections;
- introduction of any automatic update mechanism (REQ-S-020).

Suspected vulnerabilities in CretLink should be reported privately as
described in [`SECURITY.md`](../SECURITY.md) (REQ-S-022), not in public
issues.
