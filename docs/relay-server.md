# Deploying a self-hosted CretLink relay

The CretLink relay (`cretlink-relay-server`) is **operator-hosted public
infrastructure** that lets a technician console and a customer support agent
reach each other across the internet when they cannot connect directly on a LAN.
It is on neither endpoint's computer.

The relay is a deliberately dumb, opaque forwarder. Both endpoints dial it
*outbound*, present a one-shot rendezvous ticket, and the relay splices their two
connections together. Everything after the tiny rendezvous preamble is the
**unmodified end-to-end Noise `XX` session** the two endpoints would run on a
LAN. The relay therefore:

- **holds no session key and decodes no protocol message** — it forwards
  ciphertext only (REQ-F-022, REQ-S-005);
- **is never a command authority** — the Session Approval Code, attended
  consent, target identity, MCP-origin attribution, revocation, and cleanup are
  derived end-to-end between the endpoints and are byte-identical to the direct
  LAN path (ADR-0016 §5);
- **is fail-closed** — a wrong preamble, an unmatched ticket, or a stop signal
  tears the connection down and forwards zero bytes (REQ-N-008, REQ-S-009).

A malicious or compromised relay can therefore degrade availability (drop,
delay) but can neither read session content nor gain control. This is proven by
the adversarial suite in `crates/relay/tests/malicious_relay.rs` (ST-M007-006).

> **Transport note.** The shipped default transport is **`wss` over TLS**
> (WebSocket over TLS, ADR-0016 §3, ST-M007-009): supply the relay a certificate
> and key with `--tls-cert`/`--tls-key` and it authenticates itself to each
> endpoint and hides the rendezvous metadata from a *network* observer. Session
> confidentiality and integrity never depended on the transport — the payload is
> already end-to-end encrypted — so **plain TCP** remains available (supply no
> cert/key) as the ADR-0016 §5-sanctioned interim for a hop that is already
> private (a VPN, an SSH tunnel, or a TLS-terminating reverse proxy in front). The
> relay never falls back silently: a half-supplied or malformed cert/key is
> refused at startup (see [§2](#2-run-the-relay) and [§6](#6-security)).

## 1. Get the official binary

`cretlink-relay-server` is attached to every CretLink GitHub release, per
platform target triple, as `cretlink-relay-server-<target>[.exe]`, alongside the
`CHECKSUMS-<target>.txt`, the dependency inventory, and its own SBOM
(`sbom-cretlink-relay-server-<target>.cdx.json`).

1. Download `cretlink-relay-server-<target>` and `CHECKSUMS-<target>.txt` for
   your operator host's platform from the release page.
2. **Verify the download before running it** — checksum on every platform, and
   the Authenticode signature on Windows when the release is signed. Follow
   [`docs/verifying-artifacts.md`](verifying-artifacts.md); the relay-server
   binary verifies exactly like the other `cretlink-*` binaries.

To build it yourself from a checked-out, tagged commit instead — reproducing the
same binary the release ships:

```sh
cargo build --release --locked -p cretlink-relay-server
# → target/release/cretlink-relay-server[.exe]
```

The single portable artifact producer `scripts/gen-artifacts.sh` builds this
binary into `dist/` with its checksum exactly as CI does, so a local run and the
official release agree byte-for-byte on metadata.

## 2. Run the relay

```text
Usage: cretlink-relay-server [--bind <addr>] [--keys <file> | --token <secret>]
                            [--client-label <label>]
                            [--tls-cert <path> --tls-key <path>]
       cretlink-relay-server keygen
```

| Option | Env | Default | Meaning |
| --- | --- | --- | --- |
| `--bind <addr>` | `CRETLINK_RELAY_BIND` | `0.0.0.0:9970` | Listen address. A relay is meant to be publicly reachable, so it binds all interfaces by default; narrow it to one interface with this flag. |
| `--token <secret>` | `CRETLINK_RELAY_TOKEN` | *(unset → open relay)* | When set, admission requires this shared secret. **Prefer the environment variable** so the secret never appears in a process listing. An empty value is refused, not treated as open. |
| `--client-label <label>` | — | `shared` | Non-secret accountability label that all holders of the shared token are logged under. Ignored for an open relay. |
| `--keys <file>` | `CRETLINK_RELAY_KEYS` | *(unset)* | One admission key per customer copy and technician, see "One key per customer" below. Not together with `--token`: giving both is refused at startup. |
| `--tls-cert <path>` | `CRETLINK_RELAY_TLS_CERT` | *(unset)* | PEM certificate chain the relay presents. Supply **with** `--tls-key` to serve `wss://` (the shipped default transport). |
| `--tls-key <path>` | `CRETLINK_RELAY_TLS_KEY` | *(unset)* | PEM private key for that certificate. Supply **both** cert and key for `wss`; supply **neither** for the raw-TCP interim. Only one of the pair, or a missing/unreadable/malformed file, is refused **at startup** — the relay never opens a listener it cannot protect. |

A command-line flag overrides the environment. On start the relay prints its
bound address, **the selected transport** (`wss://` or raw TCP), admission
posture, and a reminder that it forwards opaque sessions only. On `Ctrl+C` it stops accepting, tears down every in-flight
forward fail-closed, and prints a **metadata-only** metrics snapshot (session
counts, forwarded byte/frame totals, and denial counters — never a credential,
ticket, or byte of session content; REQ-S-017).

### One key per customer

With one shared secret, withdrawing it from one customer means changing it for
everybody. A key file gives every customer copy and every technician a key of
its own (ST-M007-014):

```text
# relay.keys: one label:key per line
sam-laptop:5f2c9d0e…
acme:9a41c7b3…
frontdesk:0c6e12fa…
```

```sh
cretlink-relay-server keygen          # prints a new random key (256 bits)
cretlink-relay-server --keys relay.keys --tls-cert relay.crt --tls-key relay.key
```

- A **label** says who a key belongs to: letters, digits, `.`, `_` and `-`,
  at most 64 characters. The relay's log, its events and its metrics name the
  label. They never hold a key.
- A **key** is 16 to 512 characters of plain text without a blank. Use
  `keygen`; do not make keys up.
- Blank lines and lines that start with `#` are ignored. Every label and
  every key is listed once.
- **To withdraw a key, delete its line.** The relay looks at the file every
  five seconds and applies a change by itself: no restart. A new connection
  with the withdrawn key is refused; a session that already runs continues
  until it ends, because a key is checked once, when a connection arrives.
- A file that breaks a rule is refused as a whole, with the number of the
  line. At startup the relay then does not start. After a change it keeps the
  keys it had and says so in its log, so a typing error never opens the relay
  and never locks everybody out.
- Keep the file readable only by the relay's account. It holds every key.

A key lets a program use the relay and decides nothing else. It gives no
control over a session and cannot stand in for the approval code and the
customer's consent, which alone establish a session (ADR-0016 §4). The relay
holds keys only so that strangers cannot use it as an anonymous forwarding
service.

The admission token is **abuse control only, never session authority**
(ADR-0016 §5). Even an open relay bounds abuse with the public defaults:

- **Rate limit:** a burst of 30 admission attempts per client, refilling one per
  second (a sustained 1 rendezvous/sec), tracking up to 4096 distinct clients.
- **Concurrency quota:** 1024 concurrent sessions overall, at most 8 per client.
- **Preamble read timeout:** 10 s — a peer that stalls mid-preamble is dropped.
- **Rendezvous match timeout:** 30 s — a parked leg whose peer never arrives is
  dropped, never a fabricated pairing.
- **Waiting legs:** at most 1024 at a time. A connection that would find
  neither its peer nor a place to wait is refused as "no free place".

### Run as an unprivileged service (Linux, systemd)

The relay needs no privileges and no persistent files. Run it as a dedicated
unprivileged user:

```ini
# /etc/systemd/system/cretlink-relay.service
[Unit]
Description=CretLink internet relay
After=network-online.target
Wants=network-online.target

[Service]
# A dedicated, unprivileged, no-login account. The relay needs no state on disk.
User=cretlink-relay
DynamicUser=yes
Environment=CRETLINK_RELAY_BIND=0.0.0.0:9970
# Serve wss over TLS in-process (the shipped default): supply BOTH a cert chain
# and its key. Supply neither for the raw-TCP interim, and optionally front that
# with a TLS-terminating reverse proxy instead. Keep the key readable only by
# this account.
# Environment=CRETLINK_RELAY_TLS_CERT=/etc/cretlink/relay.crt
# Environment=CRETLINK_RELAY_TLS_KEY=/etc/cretlink/relay.key
# One admission key per customer (recommended): a file with one label:key per
# line, readable only by this account. The relay rereads it when it changes.
# Environment=CRETLINK_RELAY_KEYS=/etc/cretlink/relay.keys
# Or one shared secret for everybody — keep the secret out of the unit file in
# production; load it from a systemd credential or an EnvironmentFile 0600 file.
# Environment=CRETLINK_RELAY_TOKEN=...
ExecStart=/usr/local/bin/cretlink-relay-server
Restart=on-failure
# Defence in depth: no new privileges, private tmp, read-only system.
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes

[Install]
WantedBy=multi-user.target
```

```sh
sudo install -m 0755 cretlink-relay-server-x86_64-unknown-linux-gnu /usr/local/bin/cretlink-relay-server
sudo systemctl daemon-reload
sudo systemctl enable --now cretlink-relay
```

On Windows, run the verified `.exe` under a dedicated low-privilege service
account (for example via a Scheduled Task or a service wrapper); the binary
itself needs no elevation.

## 3. Point the endpoints at the relay

Both the console and the support agent dial the relay outbound and must present
the **same** single-use rendezvous ticket. Give each side a relay address, that
ticket, and — if the relay requires one — the shared admission token, through
their CLI flags or environment. Today these options exist only in the headless
development tools `cretlink-console` and `cretlink-support-agent`, built from
the source tree (see [`CONTRIBUTING.md`](../CONTRIBUTING.md)); the released
application `cretlink` does not use a relay yet (M007, ST-M009-010):

| Variable | Flag | Used by | Meaning |
| --- | --- | --- | --- |
| `CRETLINK_RELAY` | `--relay` | console, agent | The relay's reachable `host:port`. |
| `CRETLINK_RELAY_TICKET` | `--ticket` | console, agent | The shared 16-byte rendezvous ticket (hex), identical on both sides. |
| `CRETLINK_RELAY_TOKEN` | `--token` | console, agent | The relay's admission secret, when the relay was started with one. |
| `CRETLINK_RELAY_TLS` | `--relay-tls` | console, agent | The DNS name to validate the relay's certificate against. **Its presence selects the `wss://` transport;** its absence keeps the raw-TCP interim. Must match a relay served with `--tls-cert`/`--tls-key`. |
| `CRETLINK_RELAY_CA` | `--relay-ca` | console, agent | Optional path to a pinned CA/certificate PEM. Absent falls back to the OS root store. Only meaningful together with `--relay-tls`; a missing or malformed file fails closed before dialling. |

Both endpoints must agree on the transport: point **both** at a `wss` relay
(supply `--relay-tls` on each) or **neither**. A raw-TCP dial to a `wss` relay,
or a `wss` dial to a raw-TCP relay, is refused fail-closed at the transport
upgrade — the relay never silently downgrades and no session is fabricated.

The ticket is **single-use and short-lived**: generate a fresh one per session
and share it over the same trusted channel you already use to read the customer
the connection code. It names *which* pending pairing a leg joins; it is not a
secret that grants any authority, and the relay compares it in constant time and
erases it after use. A malformed relay address, ticket, or token fails closed
with an actionable error and never a fabricated session (REQ-N-015).

## 4. Upgrade

The relay is **stateless**, so upgrades are a drain-and-swap with no migration:

1. Deploy the new verified binary alongside the old one.
2. Send the running relay `Ctrl+C` (or `systemctl stop`). It stops accepting new
   connections and tears down in-flight forwards fail-closed — active sessions
   drop cleanly rather than hang.
3. Swap the binary and start again.

Run two relay instances behind a load balancer if you need zero-downtime
upgrades; because each session is fully contained in one spliced connection,
draining one instance never affects the other. Always re-run the checksum (and
signature) verification on the new binary before putting it in the start path.

## 5. Backup

There is **nothing session-related to back up**. The relay keeps no session
content, no keys, and no durable session state — its metrics are in-memory
counters that reset on restart. The only durable operator state is your own
configuration:

- the service unit / task definition and its `CRETLINK_RELAY_BIND` /
  `--client-label` settings;
- the admission secret, if you use one — store it in your secret manager, not in
  the unit file, and rotate it by restarting the relay with a new value (holders
  of the old token simply stop being admitted).

Back those up the way you back up any service configuration and secret. No
CretLink-specific backup procedure exists because no CretLink-specific durable
state exists.

## 6. Security

- **The relay is not in the trust path for session content.** Confidentiality,
  integrity, authentication, consent, and command authority are all end-to-end
  between the endpoints (Noise `XX` + the CretLink pairing/consent protocol). A
  hostile relay cannot read or forge session content; it can only drop or delay,
  which the endpoints detect (ST-M007-006).
- **Residual metadata.** A relay unavoidably observes connection metadata:
  per-direction frame counts, byte volume, and timing, plus the source addresses
  and the opaque ticket. This is the exposure already recorded in the relay
  section of [`docs/threat-model.md`](threat-model.md) (REQ-S-021); no session
  plaintext is among it. Wrapping the hop in `wss`/TLS (ST-M007-009) hides this
  metadata from a *network* observer but not from the relay operator.
- **Least privilege.** Run the relay as a dedicated unprivileged account with no
  login and no state, as in the unit above. It needs only its listen port.
- **Network exposure.** Expose only the relay port. The admission token gates
  who may open sessions and bounds abuse; the rate limit and quota above bound a
  single client's share. Nothing about the relay port grants any authority over
  either endpoint.
- **Keys.** With a key file every customer copy and technician has a key of
  its own, and the quota and the rate limit count per label. A key that leaks
  lets a stranger use the forwarder under that label until its line is
  deleted; it shows no session and controls none. The key file is the one
  secret the relay keeps: protect it like the certificate's private key.
- **Transport TLS (built-in).** Serving `wss` over TLS is built into the relay
  (ST-M007-009): supply `--tls-cert`/`--tls-key` and point both endpoints at it
  with `--relay-tls <server-name>`. The TLS stack is `rustls` with the
  `aws-lc-rs` crypto provider (ADR-0017) — audited libraries and no home-grown
  channel. A TLS-terminating reverse proxy (nginx, Caddy, HAProxy) in
  front of a raw-TCP relay remains a valid alternative when you must reuse
  existing edge termination; the proxy then sees the same
  ciphertext-plus-metadata the relay does and gains no session authority. Either
  way, TLS hides the residual metadata above from a *network* observer but not
  from whoever operates the relay or its proxy.

## 7. Interoperability

Any relay that implements the tiny rendezvous framing below and then splices the
two connections opaquely will interoperate with stock CretLink endpoints,
because everything after the preamble is the unmodified end-to-end session the
relay is not party to.

Each endpoint writes this preamble **once**, immediately after connecting,
reads the relay's answer, and then sends and receives only opaque session
bytes:

```text
magic      4 bytes   b"CRLR"  (CretLink Relay Rendezvous)
version    1 byte    0x02
token_len  2 bytes   little-endian, 0..=512
token      token_len bytes   (absent when token_len == 0)
ticket     16 bytes  the single-use rendezvous ticket
```

- **magic / version** guard against a wrong protocol reaching the port and let a
  future transport bump the version; a mismatch is rejected fail-closed, and
  the relay says so in its answer.
- **token** is the optional admission credential — abuse control only, capped at
  512 bytes and validated from the length field *before* any buffer is
  allocated. It is never session authority and is never logged.
- **ticket** is the 16-byte single-use value that names the pairing. The relay
  parks the first leg to arrive with a given ticket and, when the second leg
  presents the same ticket within the rendezvous timeout, splices the two and
  forwards opaque bytes in both directions until either side closes.

### The relay's answer

The relay answers every well-formed preamble **once**, before it forwards
anything (ST-M007-012):

```text
magic      4 bytes   b"CRLR"
version    1 byte    the preamble version the relay speaks (0x02)
status     1 byte    see the table
```

| Status | Meaning | What the endpoint does |
|---|---|---|
| `0` | Admitted. The leg waits for its peer, or has met it; what follows comes from the peer. | Start the session. |
| `1` | The relay asks for an admission key and none was presented. | Stop; a key is missing in the relay settings. |
| `2` | The relay does not know the key that was presented. | Stop; the key is wrong or was withdrawn. |
| `3` | Too many attempts in a short time. | Stop; try again after a moment. |
| `4` | No free place: a concurrency quota is full. | Stop; try again later. |
| `5` | The relay cannot admit anybody right now. | Stop; the relay needs its operator. |
| `6` | The relay does not speak the preamble version of this endpoint. The version byte names the one it speaks. | Stop; the relay server and the program come from releases that do not fit together. |

After any status but `0` the relay closes the connection. Status `6` keeps its
meaning in every preamble version, and an endpoint reads an answer with
another version byte as status `6` whatever its status says, so from version 2
on two versions can tell each other that they differ. A relay server of
version 1 sends no answer at all; an endpoint reports that as a relay that did
not answer. A status that an endpoint does not know is never taken for an
admission. Something that sends no preamble at all, such as a port scan, gets
no answer.

The answer exists so that an endpoint can say what went wrong. Without it a
refusal, a full relay and a rendezvous that nobody joined all looked the same:
a closed connection. With it, a connection that ends **after** status `0` and
before a single byte of the peer arrived was not refused. No session came
about on that rendezvous: nobody joined before the relay let it lapse, or the
relay had no room left to park the leg, or the peer it was matched with had
already gone.

- The **waiting** endpoint then dials again with the same ticket, for as long
  as its person waits. It also gives up a leg on which it hears nothing for
  longer than a relay parks one (45 seconds), and dials again: the relay's
  close may never arrive after a change of network, and a leg may have been
  matched with a peer that stays silent.
- The **joining** endpoint gives the peer ten seconds to be heard, less than
  pairing is given, and then reports that nobody waits on that code.

The answer is the relay's own addressing metadata: fixed in size, written
once, with nothing of a session in it. Its reasons are those of admission,
which decides only who may use the forwarder and never anything about a
session.

### The relay code and the ticket

Two people do not exchange a 16-byte ticket. The technician reads a **relay
code** aloud, sixteen characters in four groups such as `53ST-A71D-JKKV-0VGH`,
and both ends derive the ticket from it (ST-M007-012). This is how the
application names a rendezvous once it offers the relay; the development tools
of section 3 take the ticket itself.

```text
relay code  16 Crockford Base32 characters = 80 bits
              4 bits   format version (2)
             64 bits   rendezvous seed, random, minted per session
             12 bits   CRC-12 (polynomial 0x80F) over the first 68 bits

ticket      HKDF-SHA256(salt = "CretLink relay rendezvous ticket v1",
                        ikm  = seed,
                        info = canonical host name of the relay)[..16]
```

The code carries no address: each program takes the relay from its own
settings. The host name is part of the derivation, so two programs that name
different relays never meet. The canonical form is the host name in lower case
without a trailing dot, or an IP address in its standard text form; the port
is not part of it. An address in another spelling, such as one with leading
zeros, is refused instead of taken for a name. The alphabet has no `I`, `L`, `O` or `U`; input in any case
is accepted, `I` and `L` read as `1`, `O` as `0`, and hyphens and spaces are
ignored. The checksum rejects every mistyped character and every swap of two
neighbours before anything is dialled. It is a typing check and no secret.

A relay code grants nothing, like the ticket it stands for. Someone who
overhears or guesses it can at most reach the rendezvous first; the approval
codes that both people compare then differ, and the session ends before any
control exists.

The session that flows after the preamble is CretLink's standard wire encoding
(ADR-0010) carrying a Noise `XX` handshake and secure channel (ADR-0016 §5); a
conforming relay treats it as opaque and MUST NOT attempt to parse, modify,
reorder, duplicate, or inject frames — the endpoints' AEAD check fails closed on
any such tampering, permanently poisoning the channel (REQ-S-008, REQ-S-009).

---

*Requirements: REQ-F-022 (internet relay), REQ-S-019/REQ-S-020 (relay operator
posture), REQ-N-016 through REQ-N-019 (provenance, honest failure, versioned
artifacts). Stories: ST-M007-005 (deployment), ST-M007-007 (server binary),
ST-M007-009 (built-in wss/TLS transport), ST-M007-012 (the relay's answer and
the relay code), ST-M007-014 (one key per customer). Architecture: ADR-0016 §2 (rendezvous is addressing), §3
(shipped wss transport), §5 (opaque rendezvous forwarder).*
