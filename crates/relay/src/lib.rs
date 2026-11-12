//! Opaque outbound-rendezvous forwarding backbone for the CretLink internet
//! relay (ST-M007-002, ADR-0016).
//!
//! # What this crate is
//!
//! The relay lets two endpoints that cannot reach each other on a LAN establish
//! the **same** attended CretLink session across the internet. Both endpoints
//! dial the relay **outbound** (no new customer-side listener, REQ-S-010), name
//! the pending pairing with an addressing-only [`RendezvousTicket`], and the
//! relay [`splice`](splice::splice)s the two connections into one bidirectional
//! byte pipe.
//!
//! # What this crate is *not*
//!
//! The relay is **never a command authority or a plaintext endpoint**
//! (REQ-F-022, REQ-S-005). It forwards the unmodified end-to-end Noise session
//! frames (ADR-0010) as opaque, length-bounded binary blobs. It holds:
//!
//! - no session key and runs no Noise handshake — the endpoints keep those to
//!   themselves, so a relay in the middle produces a different transcript hash
//!   on each leg and the Session Approval Code comparison (ADR-0003) fails
//!   closed;
//! - no protocol decoder — it never constructs a [`cretlink_protocol::Message`];
//! - no ability to fabricate a clean outcome — any fault (oversize frame, quota,
//!   cancellation, truncation, I/O error) tears the splice down fail-closed and
//!   is reported honestly (REQ-N-008).
//!
//! # Contract
//!
//! - **Flow control.** Each direction forwards one whole length-prefixed frame
//!   at a time, bounded by [`MAX_RELAY_FRAME_BYTES`];
//!   a slow reader backpressures its writer because the relay stops pulling the
//!   next frame until the current one is fully written (REQ-N-006).
//! - **Quotas.** [`SpliceLimits`] caps per-direction bytes
//!   and frames; exceeding either stops forwarding fail-closed (REQ-N-006).
//! - **Cancellation.** A [`StopSignal`] tears both
//!   directions down immediately; a dropped controller also fails closed.
//! - **Observability.** A completed splice yields a metadata-only
//!   [`SpliceReport`]: byte and frame counts per direction
//!   and the terminal outcome. It never records session content.
//!
//! # Abuse and operations controls
//!
//! Alongside forwarding, this crate carries the portable, fail-closed abuse and
//! operations controls a reachable relay needs (ST-M007-004, ADR-0016 §4–§5):
//! admission [authentication](admission) that is strictly orthogonal to session
//! authority, a deterministic admission [rate limiter](ratelimit), concurrency
//! [quotas](quota), aggregate [metrics], and a bounded, metadata-only
//! operational [event log](events) with explicit retention. [`control`] composes
//! them into a single [`AdmissionControl`] gate.
//!
//! The public listener process and TLS termination live in
//! `cretlink-relay-server` and `cretlink-wss`; this crate is the portable core
//! they build on.
//!

pub mod admission;
pub mod cancel;
pub mod control;
pub mod events;
pub mod metrics;
pub mod preamble;
pub mod quota;
pub mod ratelimit;
pub mod rendezvous;
pub mod splice;
pub mod ticket;

pub use admission::{AdmissionError, AdmissionPolicy, AdmissionToken, ClientId};
pub use cancel::{StopHandle, StopSignal};
pub use control::{AdmissionControl, ControlConfig, Denied, SessionGuard};
pub use events::{EventLog, OpEvent, OpEventKind, RetentionPolicy};
pub use metrics::{MetricsSnapshot, RelayMetrics};
pub use preamble::{
    MAX_PREAMBLE_TOKEN_BYTES, PREAMBLE_MAGIC, PREAMBLE_VERSION, Preamble, PreambleError,
    read_preamble, write_preamble,
};
pub use quota::{QuotaError, QuotaState, SessionQuota};
pub use ratelimit::{RateLimit, RateLimiter};
pub use rendezvous::{Matched, Rendezvous, RendezvousError};
pub use splice::{
    Direction, MAX_RELAY_FRAME_BYTES, SpliceLimits, SpliceOutcome, SpliceReport, splice,
};
pub use ticket::{
    MAX_RELAY_HOST_LEN, RENDEZVOUS_SEED_LEN, RENDEZVOUS_TICKET_LEN, RendezvousTicket, TicketError,
    canonical_relay_host,
};
