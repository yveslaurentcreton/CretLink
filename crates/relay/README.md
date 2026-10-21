# cretlink-relay

Opaque outbound-rendezvous forwarding backbone for the CretLink internet relay
(milestone **M007**, story **ST-M007-002**, ADR-0016).

The relay lets two endpoints that cannot reach each other on a LAN establish the
**same** attended CretLink session across the internet. Both endpoints dial the
relay **outbound** (no new customer-side listener, REQ-S-010), name the pending
pairing with an addressing-only ticket, and the relay splices the two
connections into one bidirectional byte pipe.

This crate is the portable, workspace-tested **core**: forwarding, rendezvous,
the connection preamble, and the abuse and operations controls (admission,
rate limiting, concurrency quotas, metrics, and the operational event log),
composed into one admission gate. The TLS/`wss` transport envelope lives in
[`crates/wss`](../wss), and the deployable listener that wires this core onto a
real socket is [`apps/relay-server`](../../apps/relay-server) (see
[`docs/relay-server.md`](../../docs/relay-server.md)).

## What the relay is not

The relay is **never a command authority or a plaintext endpoint** (REQ-F-022,
REQ-S-005). It forwards the unmodified end-to-end Noise session frames
(ADR-0010) as opaque, length-bounded binary blobs. It holds:

- **no session key** and runs no Noise handshake — the endpoints keep those to
  themselves, so a relay in the middle produces a different transcript hash on
  each leg and the Session Approval Code comparison (ADR-0003) fails closed;
- **no protocol decoder** — it never constructs a `cretlink_protocol::Message`;
- **no way to fabricate a clean outcome** — any fault tears the splice down
  fail-closed and is reported honestly (REQ-N-008).

These properties are proven end to end in
[`tests/opaque_forwarding.rs`](tests/opaque_forwarding.rs), which runs the real
Noise `XX` pairing across the splice: both endpoints derive the identical
Session Approval Code, a marker placed inside an encrypted message never appears
in the forwarded bytes, and one flipped handshake byte makes the pairing fail
closed.

## Contract

### Flow control (REQ-N-006)

Each direction reads exactly one whole length-prefixed frame, then writes it in
full before reading the next. At most one frame — bounded by
`MAX_RELAY_FRAME_BYTES` (the 1 MiB plaintext ceiling plus the 16-byte Noise
tag) — is buffered per direction. A slow reader backpressures its writer: the
relay stops pulling the next frame from the source until the destination has
drained the current one, so a fast sender cannot force unbounded relay memory.
An over-ceiling length prefix is rejected from the 4-byte prefix alone, before
the body buffer is allocated (REQ-N-012).

### Quotas (REQ-N-006)

`SpliceLimits` caps, **per direction**:

| Field             | Meaning                                            | Default              |
| ----------------- | -------------------------------------------------- | -------------------- |
| `max_frame_bytes` | largest single frame accepted                      | `MAX_RELAY_FRAME_BYTES` |
| `byte_quota`      | total wire bytes (prefixes included), `None` = off | `None`               |
| `frame_quota`     | total frames, `None` = off                         | `None`               |

Reaching a byte or frame quota stops forwarding with
`SpliceOutcome::QuotaExceeded`; an oversize frame stops with
`SpliceOutcome::FrameOversize`. The frame that would cross a quota is **not**
forwarded — the cap fails closed.

### Cancellation

`StopSignal::stop` tears both directions of every attached splice down
immediately (`SpliceOutcome::Cancelled`). Dropping the `StopSignal` — a
controller that crashed — also releases every `StopHandle`, so a splice never
lingers when its controller is gone. On return the splice drops both legs, so
whichever endpoint was still open observes a close rather than a hang.

### Observability

A finished splice yields a metadata-only `SpliceReport`: per-direction frame and
byte counts plus the terminal `SpliceOutcome`. It records counts and an outcome
tag only — never any forwarded session content, which the relay cannot read.

## Layout

| Module       | Responsibility                                                          |
| ------------ | ----------------------------------------------------------------------- |
| `splice`     | the opaque bidirectional frame forwarder, limits, and report            |
| `rendezvous` | matching the two outbound arrivals on a ticket, fail-closed on timeout  |
| `ticket`     | the 128-bit addressing-only, single-use, constant-time-compared ticket  |
| `cancel`     | the fan-out stop signal that fails closed                               |
| `preamble`   | the one-time per-leg preamble carrying the admission token and ticket   |
| `admission`  | admission credentials for abuse control only, never session authority   |
| `ratelimit`  | the deterministic token-bucket limiter for admission attempts           |
| `quota`      | global and per-client concurrent-session ceilings                       |
| `metrics`    | aggregate, metadata-only relay counters                                 |
| `events`     | the bounded, metadata-only operational event log with retention         |
| `control`    | the single admission gate composing admission, rate limit, quota, logs  |

## Testing

```sh
cargo test -p cretlink-relay
cargo clippy -p cretlink-relay --all-targets -- -D warnings
```

Besides the unit tests in each module, the integration tests run the real Noise
`XX` pairing across the relay:

- [`tests/opaque_forwarding.rs`](tests/opaque_forwarding.rs) — the honest relay
  is an opaque byte pipe;
- [`tests/admission_control.rs`](tests/admission_control.rs) — the admission
  gate composes with the splice and records metadata-only metrics and events;
- [`tests/malicious_relay.rs`](tests/malicious_relay.rs) — a relay that
  observes, delays, drops, replays, reorders, or modifies frames gains neither
  control nor plaintext.

