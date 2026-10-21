//! A bounded, metadata-only operational event log with an explicit retention
//! policy (ST-M007-004, ADR-0016 §5, REQ-S-017).
//!
//! Alongside the aggregate [`RelayMetrics`](crate::metrics::RelayMetrics), an
//! operator needs a recent, per-event trail for reporting and abuse follow-up:
//! *which* client was admitted or denied, *when*, and *why*. This is an
//! append-oriented log of [`OpEvent`]s, each carrying only metadata the audit
//! requirement allows — session-scoped identity, a timestamp, an actor/source,
//! an action, and a result (REQ-S-017). It **never** records a session secret,
//! an admission credential, a rendezvous ticket, or any forwarded byte
//! (REQ-N-017); those types are not even in scope here.
//!
//! # Retention (bounded on two axes)
//!
//! [`RetentionPolicy`] bounds the log by **count** and by **age**. Appending
//! prunes anything beyond [`max_events`](RetentionPolicy::max_events) (oldest
//! first) and anything older than [`max_age_ms`](RetentionPolicy::max_age_ms)
//! relative to the newest event. The log therefore has an intrinsic memory
//! ceiling and does not retain operational metadata indefinitely — a
//! configurable-retention posture (REQ-S-018) that keeps the relay's footprint
//! bounded (REQ-N-006).
//!

use std::collections::VecDeque;

use crate::admission::ClientId;
use crate::splice::SpliceOutcome;

/// How long and how many operational events the relay keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Maximum events retained; appending past this drops the oldest.
    pub max_events: usize,
    /// Maximum age in milliseconds relative to the newest event; older events
    /// are pruned on append.
    pub max_age_ms: u64,
}

impl RetentionPolicy {
    /// The documented default for a public relay: keep at most 4096 recent
    /// operational events, none older than 24 hours.
    #[must_use]
    pub const fn public_default() -> Self {
        Self {
            max_events: 4_096,
            max_age_ms: 24 * 60 * 60 * 1_000,
        }
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::public_default()
    }
}

/// What happened. Every variant is metadata only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpEventKind {
    /// A client was admitted to the forwarder.
    Admitted,
    /// A connection was denied: the admission credential was missing or wrong.
    DeniedAuth,
    /// A connection was denied: the client was rate-limited.
    DeniedRateLimit,
    /// A connection was denied: a concurrency quota was full.
    DeniedQuota,
    /// A connection was denied: the relay was temporarily unavailable
    /// (fail-closed, for example a poisoned lock).
    DeniedUnavailable,
    /// A session ended, with its metadata-only forwarding totals and outcome.
    SessionEnded {
        /// Total wire bytes forwarded in both directions.
        bytes: u64,
        /// Total frames forwarded in both directions.
        frames: u64,
        /// Why the splice ended, when a report was produced. `None` records an
        /// **unknown** teardown — the session's guard was dropped without a
        /// final report — kept distinct rather than fabricated as a clean close
        /// (REQ-N-008).
        outcome: Option<SpliceOutcome>,
    },
}

/// One operational event. It carries session-scoped identity (`client`), a
/// timestamp, an actor/source (`peer`), the action (`kind`), and — for a session
/// end — a result. No secret or session content ever appears here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpEvent {
    /// A monotonically increasing sequence number within this log.
    pub seq: u64,
    /// The event time as a monotonic millisecond stamp supplied by the relay.
    pub at_ms: u64,
    /// The admitted client label, when one was determined. Denials for a missing
    /// credential have no client.
    pub client: Option<ClientId>,
    /// The caller-supplied addressing label (for example a remote address). Pure
    /// metadata the audit requirement permits as the source (REQ-S-017).
    pub peer: String,
    /// What happened.
    pub kind: OpEventKind,
}

/// An append-oriented, bounded operational event log. Not internally
/// synchronised; the relay wraps it behind the admission-control lock.
#[derive(Debug)]
pub struct EventLog {
    policy: RetentionPolicy,
    events: VecDeque<OpEvent>,
    next_seq: u64,
}

impl EventLog {
    /// Creates an empty log with the given retention policy.
    #[must_use]
    pub fn new(policy: RetentionPolicy) -> Self {
        Self {
            policy,
            events: VecDeque::new(),
            next_seq: 0,
        }
    }

    /// Appends one event at `at_ms`, then prunes by age and count. Returns the
    /// assigned sequence number.
    pub fn append(
        &mut self,
        at_ms: u64,
        client: Option<ClientId>,
        peer: &str,
        kind: OpEventKind,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        self.events.push_back(OpEvent {
            seq,
            at_ms,
            client,
            peer: peer.to_string(),
            kind,
        });
        self.prune(at_ms);
        seq
    }

    /// Drops events older than the age bound (relative to `newest_ms`) and any
    /// beyond the count bound, oldest first.
    fn prune(&mut self, newest_ms: u64) {
        let cutoff = newest_ms.saturating_sub(self.policy.max_age_ms);
        while let Some(front) = self.events.front() {
            if front.at_ms < cutoff {
                self.events.pop_front();
            } else {
                break;
            }
        }
        while self.events.len() > self.policy.max_events {
            self.events.pop_front();
        }
    }

    /// Number of events currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The retained events, oldest first.
    #[must_use]
    pub fn recent(&self) -> Vec<OpEvent> {
        self.events.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::splice::Direction;

    fn log(max_events: usize, max_age_ms: u64) -> EventLog {
        EventLog::new(RetentionPolicy {
            max_events,
            max_age_ms,
        })
    }

    #[test]
    fn appends_assign_increasing_sequence_numbers() {
        let mut l = log(16, 1_000_000);
        let s0 = l.append(0, Some(ClientId::new("a")), "peer-a", OpEventKind::Admitted);
        let s1 = l.append(1, None, "peer-b", OpEventKind::DeniedAuth);
        assert_eq!((s0, s1), (0, 1));
        assert_eq!(l.len(), 2);
        let recent = l.recent();
        assert_eq!(recent[0].client, Some(ClientId::new("a")));
        assert_eq!(recent[1].kind, OpEventKind::DeniedAuth);
    }

    #[test]
    fn count_bound_drops_oldest_first() {
        let mut l = log(2, 1_000_000);
        l.append(0, None, "p", OpEventKind::DeniedRateLimit);
        l.append(1, None, "p", OpEventKind::DeniedQuota);
        l.append(2, None, "p", OpEventKind::DeniedUnavailable);
        assert_eq!(l.len(), 2, "capped at max_events");
        let recent = l.recent();
        // The first (seq 0) was pruned; seq 1 and 2 remain.
        assert_eq!(recent[0].seq, 1);
        assert_eq!(recent[1].seq, 2);
    }

    #[test]
    fn age_bound_prunes_older_than_the_newest() {
        let mut l = log(1_000, 100);
        l.append(0, None, "p", OpEventKind::Admitted);
        l.append(50, None, "p", OpEventKind::Admitted);
        // Newest at 200ms: cutoff = 200 - 100 = 100; the 0ms and 50ms events are
        // strictly older and pruned.
        l.append(200, None, "p", OpEventKind::Admitted);
        assert_eq!(l.len(), 1);
        assert_eq!(l.recent()[0].at_ms, 200);
    }

    #[test]
    fn session_ended_records_outcome_and_totals() {
        let mut l = log(16, 1_000_000);
        l.append(
            10,
            Some(ClientId::new("alice")),
            "peer",
            OpEventKind::SessionEnded {
                bytes: 4_096,
                frames: 12,
                outcome: Some(SpliceOutcome::Closed(Direction::AtoB)),
            },
        );
        let ev = &l.recent()[0];
        assert!(matches!(
            ev.kind,
            OpEventKind::SessionEnded {
                bytes: 4_096,
                frames: 12,
                outcome: Some(SpliceOutcome::Closed(Direction::AtoB)),
            }
        ));
    }
}
