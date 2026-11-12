//! The relay's operational front door: one place that authenticates a
//! connection, rate-limits it, reserves a concurrency slot, and records the
//! decision — before any bytes are forwarded (ST-M007-004, ADR-0016 §4–§5).
//!
//! [`AdmissionControl`] composes the four abuse controls into a single
//! fail-closed gate:
//!
//! 1. [`AdmissionPolicy`] authenticates *use of the forwarder* — never session
//!    authority (holding a relay credential grants **zero** session control).
//! 2. [`RateLimiter`] bounds how fast one client may attempt admission.
//! 3. [`QuotaState`] bounds how many sessions run at once, globally and per
//!    client.
//! 4. [`RelayMetrics`] and [`EventLog`] record every admission and denial as
//!    metadata only (REQ-S-017) — no credential, ticket, or plaintext ever
//!    (REQ-N-017).
//!
//! A successful [`admit`](AdmissionControl::admit) returns a [`SessionGuard`]:
//! the caller runs [`splice`](crate::splice::splice) with the guard's
//! [`splice_limits`](SessionGuard::splice_limits), then calls
//! [`finish`](SessionGuard::finish) with the resulting
//! [`SpliceReport`]. Dropping the guard **always**
//! releases the client's concurrency slot and decrements the active gauge, even
//! on an abnormal teardown, and records an honest *unknown* end rather than a
//! fabricated success (REQ-N-008).
//!
//! Everything is fail-closed: a poisoned internal lock denies admission
//! ([`Denied::Unavailable`]) rather than forwarding through unknown state, and
//! errors name the failed stage and a safe next action without exposing any
//! secret (REQ-N-015).
//!

use std::sync::Arc;
use std::sync::Mutex;

use thiserror::Error;

use crate::admission::{AdmissionError, AdmissionPolicy, AdmissionToken, ClientId};
use crate::events::{EventLog, OpEvent, OpEventKind, RetentionPolicy};
use crate::metrics::{MetricsSnapshot, RelayMetrics};
use crate::quota::{QuotaError, QuotaState, SessionQuota};
use crate::ratelimit::{RateLimit, RateLimiter};
use crate::splice::{SpliceLimits, SpliceReport};

/// Tunable ceilings and retention for one relay instance. [`Default`] is the
/// documented public-relay posture; a self-hosted operator overrides fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlConfig {
    /// Admission-attempt rate limit, per client.
    pub rate_limit: RateLimit,
    /// Concurrent-session quotas, global and per client.
    pub quota: SessionQuota,
    /// Operational-event retention.
    pub retention: RetentionPolicy,
    /// Per-session forwarding bounds handed to each admitted session's
    /// [`splice`](crate::splice::splice).
    pub splice_limits: SpliceLimits,
}

impl ControlConfig {
    /// The documented default posture for a publicly reachable relay.
    #[must_use]
    pub const fn public_default() -> Self {
        Self {
            rate_limit: RateLimit::public_default(),
            quota: SessionQuota::public_default(),
            retention: RetentionPolicy::public_default(),
            splice_limits: PUBLIC_SPLICE_LIMITS,
        }
    }
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self::public_default()
    }
}

/// The documented per-session forwarding bounds for a public relay: 256 MiB and
/// one million frames per direction. A session that exceeds either is torn down
/// fail-closed by [`splice`](crate::splice::splice).
const PUBLIC_SPLICE_LIMITS: SpliceLimits = SpliceLimits {
    max_frame_bytes: crate::splice::MAX_RELAY_FRAME_BYTES,
    byte_quota: Some(256 * 1024 * 1024),
    frame_quota: Some(1_000_000),
};

/// Why a connection was denied admission to the forwarder. Every variant denies;
/// none grants partial access. Messages are actionable and secret-free
/// (REQ-N-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Denied {
    /// Authentication failed: the admission credential was missing or wrong.
    /// Next action: present a valid relay credential.
    #[error("relay admission failed at authentication: {0}")]
    Auth(#[from] AdmissionError),
    /// The client is sending admission attempts too fast. Next action: retry
    /// after backing off.
    #[error("relay admission failed at rate limiting: too many attempts; retry later")]
    RateLimited,
    /// A concurrency quota is full. Next action: retry when a session frees up.
    #[error("relay admission failed at the concurrency quota: {0}")]
    Quota(#[from] QuotaError),
    /// The relay is temporarily unable to admit (fail-closed internal state).
    /// Next action: retry; if it persists, the relay needs operator attention.
    #[error("relay admission is temporarily unavailable; retry later")]
    Unavailable,
}

/// What the relay tells the endpoint it denied ([`crate::preamble::RelayAnswer`],
/// ST-M007-012): the same reason, without the internal detail of which quota
/// was full.
impl From<Denied> for crate::preamble::RelayRefusal {
    fn from(denied: Denied) -> Self {
        match denied {
            Denied::Auth(AdmissionError::CredentialRequired) => Self::CredentialRequired,
            Denied::Auth(AdmissionError::CredentialRejected) => Self::CredentialRejected,
            Denied::RateLimited => Self::RateLimited,
            Denied::Quota(_) => Self::Busy,
            Denied::Unavailable => Self::Unavailable,
        }
    }
}

/// The mutable, lock-guarded operational state.
#[derive(Debug)]
struct State {
    limiter: RateLimiter,
    quota: QuotaState,
    events: EventLog,
}

/// Shared inner state behind an `Arc` so a [`SessionGuard`] can release its slot
/// on drop.
#[derive(Debug)]
struct Inner {
    policy: AdmissionPolicy,
    splice_limits: SpliceLimits,
    state: Mutex<State>,
    metrics: RelayMetrics,
}

impl Inner {
    /// Ends a session exactly once: folds metadata into metrics (lock-free),
    /// then — best effort under the lock — releases the client's slot and
    /// records the end event. A poisoned lock still lets the metrics gauge fall,
    /// never leaving a fabricated live session.
    fn end_session(
        &self,
        client: &ClientId,
        peer: &str,
        at_ms: u64,
        bytes: u64,
        frames: u64,
        outcome: Option<crate::splice::SpliceOutcome>,
    ) {
        self.metrics.record_ended(bytes, frames);
        if let Ok(mut state) = self.state.lock() {
            state.quota.release(client);
            state.events.append(
                at_ms,
                Some(client.clone()),
                peer,
                OpEventKind::SessionEnded {
                    bytes,
                    frames,
                    outcome,
                },
            );
        }
    }
}

/// The relay's admission-control gate. Cheap to clone (shares one `Arc`); hand a
/// clone to each accept task.
#[derive(Debug, Clone)]
pub struct AdmissionControl {
    inner: Arc<Inner>,
}

impl AdmissionControl {
    /// Builds a gate with the given policy and configuration.
    #[must_use]
    pub fn new(policy: AdmissionPolicy, config: ControlConfig) -> Self {
        let state = State {
            limiter: RateLimiter::new(config.rate_limit),
            quota: QuotaState::new(config.quota),
            events: EventLog::new(config.retention),
        };
        Self {
            inner: Arc::new(Inner {
                policy,
                splice_limits: config.splice_limits,
                state: Mutex::new(state),
                metrics: RelayMetrics::new(),
            }),
        }
    }

    /// Authenticates, rate-limits, and reserves a concurrency slot for a
    /// connection presenting `token` from `peer` at monotonic `now_ms`. On
    /// success returns a [`SessionGuard`] the caller must keep for the session's
    /// lifetime.
    ///
    /// # Errors
    ///
    /// [`Denied`] — one of authentication, rate limiting, quota, or temporary
    /// unavailability. Every path denies fail-closed and records the decision.
    pub fn admit(
        &self,
        token: Option<&AdmissionToken>,
        peer: &str,
        now_ms: u64,
    ) -> Result<SessionGuard, Denied> {
        // 1. Authenticate use of the forwarder (no session authority).
        let client = match self.inner.policy.admit(token, peer) {
            Ok(client) => client,
            Err(err) => {
                self.inner.metrics.record_denied_auth();
                if let Ok(mut state) = self.inner.state.lock() {
                    state
                        .events
                        .append(now_ms, None, peer, OpEventKind::DeniedAuth);
                }
                return Err(Denied::Auth(err));
            }
        };

        // 2–4. Rate limit, quota, and admit — all under one lock so the decision
        // and its record are consistent.
        let mut state = match self.inner.state.lock() {
            Ok(state) => state,
            Err(_) => {
                self.inner.metrics.record_denied_unavailable();
                return Err(Denied::Unavailable);
            }
        };

        if !state.limiter.check(&client, now_ms) {
            self.inner.metrics.record_denied_rate_limit();
            state.events.append(
                now_ms,
                Some(client.clone()),
                peer,
                OpEventKind::DeniedRateLimit,
            );
            return Err(Denied::RateLimited);
        }

        if let Err(err) = state.quota.acquire(&client) {
            self.inner.metrics.record_denied_quota();
            state
                .events
                .append(now_ms, Some(client.clone()), peer, OpEventKind::DeniedQuota);
            return Err(Denied::Quota(err));
        }

        self.inner.metrics.record_admitted();
        state
            .events
            .append(now_ms, Some(client.clone()), peer, OpEventKind::Admitted);
        drop(state);

        Ok(SessionGuard {
            inner: Arc::clone(&self.inner),
            client,
            peer: peer.to_string(),
            limits: self.inner.splice_limits,
            admitted_ms: now_ms,
            finished: false,
        })
    }

    /// A point-in-time snapshot of the operational metrics (metadata only).
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.inner.metrics.snapshot()
    }

    /// The retained operational events, oldest first. Returns an empty vector if
    /// the internal lock is poisoned (fail-closed: never blocks or panics).
    #[must_use]
    pub fn recent_events(&self) -> Vec<OpEvent> {
        match self.inner.state.lock() {
            Ok(state) => state.events.recent(),
            Err(_) => Vec::new(),
        }
    }
}

/// A reserved concurrency slot for one admitted session. Dropping it releases
/// the slot and decrements the active gauge; call [`finish`](Self::finish) to
/// record the session's forwarding totals and outcome first.
#[derive(Debug)]
pub struct SessionGuard {
    inner: Arc<Inner>,
    client: ClientId,
    peer: String,
    limits: SpliceLimits,
    admitted_ms: u64,
    finished: bool,
}

impl SessionGuard {
    /// The non-secret client label this session is accounted under.
    #[must_use]
    pub fn client(&self) -> &ClientId {
        &self.client
    }

    /// The per-session forwarding bounds to hand this session's
    /// [`splice`](crate::splice::splice).
    #[must_use]
    pub fn splice_limits(&self) -> SpliceLimits {
        self.limits
    }

    /// Records the session's end from its [`SpliceReport`] at monotonic
    /// `now_ms`, folding the forwarding totals into metrics and the event log,
    /// and releasing the concurrency slot. Consumes the guard so the end is
    /// recorded exactly once.
    pub fn finish(mut self, now_ms: u64, report: &SpliceReport) {
        let bytes = report.a_to_b_bytes.saturating_add(report.b_to_a_bytes);
        let frames = report.a_to_b_frames.saturating_add(report.b_to_a_frames);
        self.inner.end_session(
            &self.client,
            &self.peer,
            now_ms,
            bytes,
            frames,
            Some(report.outcome),
        );
        self.finished = true;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // An abnormal teardown (the guard was dropped without a report): still
        // release the slot and drop the active gauge, and record an honest
        // *unknown* end stamped at the session's admit time (no clock in drop).
        self.inner
            .end_session(&self.client, &self.peer, self.admitted_ms, 0, 0, None);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::admission::ClientId;
    use crate::splice::{Direction, SpliceOutcome};

    fn shared_token_control() -> AdmissionControl {
        AdmissionControl::new(
            AdmissionPolicy::SharedToken {
                token: AdmissionToken::from_text("relay-secret"),
                client: ClientId::new("shared"),
            },
            ControlConfig::default(),
        )
    }

    fn report(bytes: u64, frames: u64, outcome: SpliceOutcome) -> SpliceReport {
        SpliceReport {
            a_to_b_frames: frames,
            a_to_b_bytes: bytes,
            b_to_a_frames: 0,
            b_to_a_bytes: 0,
            outcome,
        }
    }

    #[test]
    fn a_missing_or_wrong_credential_is_denied_and_counted() {
        let ctrl = shared_token_control();
        // Missing credential.
        let err = ctrl.admit(None, "192.0.2.1", 0).unwrap_err();
        assert!(matches!(
            err,
            Denied::Auth(AdmissionError::CredentialRequired)
        ));
        // Wrong credential.
        let wrong = AdmissionToken::from_text("nope");
        let err = ctrl.admit(Some(&wrong), "192.0.2.1", 0).unwrap_err();
        assert!(matches!(
            err,
            Denied::Auth(AdmissionError::CredentialRejected)
        ));

        let m = ctrl.metrics();
        assert_eq!(m.denied_auth, 2);
        assert_eq!(m.sessions_admitted, 0);
        // The denials are recorded as metadata-only events, with no client for
        // the un-authenticated attempts.
        let events = ctrl.recent_events();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.client.is_none()));
        assert!(matches!(events[0].kind, OpEventKind::DeniedAuth));
    }

    #[test]
    fn a_valid_credential_is_admitted_and_the_guard_tracks_the_session() {
        let ctrl = shared_token_control();
        let token = AdmissionToken::from_text("relay-secret");
        let guard = ctrl.admit(Some(&token), "192.0.2.1", 0).unwrap();
        assert_eq!(guard.client(), &ClientId::new("shared"));

        let m = ctrl.metrics();
        assert_eq!(m.sessions_admitted, 1);
        assert_eq!(m.sessions_active, 1);

        // Finishing records the forwarding totals and frees the slot.
        guard.finish(
            10,
            &report(4_096, 12, SpliceOutcome::Closed(Direction::AtoB)),
        );
        let m = ctrl.metrics();
        assert_eq!(m.sessions_active, 0);
        assert_eq!(m.sessions_ended, 1);
        assert_eq!(m.bytes_forwarded, 4_096);
        assert_eq!(m.frames_forwarded, 12);

        let ended = ctrl
            .recent_events()
            .into_iter()
            .find(|e| matches!(e.kind, OpEventKind::SessionEnded { .. }))
            .expect("an end event");
        assert!(matches!(
            ended.kind,
            OpEventKind::SessionEnded {
                outcome: Some(SpliceOutcome::Closed(Direction::AtoB)),
                ..
            }
        ));
    }

    #[test]
    fn dropping_a_guard_without_finish_records_an_unknown_end_fail_closed() {
        let ctrl = shared_token_control();
        let token = AdmissionToken::from_text("relay-secret");
        {
            let _guard = ctrl.admit(Some(&token), "peer", 0).unwrap();
            assert_eq!(ctrl.metrics().sessions_active, 1);
            // Guard dropped here without finish (e.g. a panic upstream).
        }
        let m = ctrl.metrics();
        assert_eq!(m.sessions_active, 0, "slot released on drop, fail-closed");
        assert_eq!(m.sessions_ended, 1);
        let ended = ctrl
            .recent_events()
            .into_iter()
            .find(|e| matches!(e.kind, OpEventKind::SessionEnded { .. }))
            .expect("an end event");
        assert!(
            matches!(ended.kind, OpEventKind::SessionEnded { outcome: None, .. }),
            "an abandoned session is an honest unknown end, not a clean close"
        );
    }

    #[test]
    fn the_per_client_quota_refuses_over_budget_sessions() {
        let ctrl = AdmissionControl::new(
            AdmissionPolicy::Open,
            ControlConfig {
                quota: SessionQuota {
                    global_max: 100,
                    per_client_max: 1,
                },
                ..ControlConfig::default()
            },
        );
        let g1 = ctrl.admit(None, "same-peer", 0).unwrap();
        // The same client's second concurrent session is refused.
        let err = ctrl.admit(None, "same-peer", 0).unwrap_err();
        assert!(matches!(err, Denied::Quota(QuotaError::ClientFull)));
        // A different client still gets in.
        let _g2 = ctrl.admit(None, "other-peer", 0).unwrap();

        // Freeing the first session re-opens that client's budget.
        g1.finish(1, &report(0, 0, SpliceOutcome::Closed(Direction::AtoB)));
        let _g1b = ctrl.admit(None, "same-peer", 0).unwrap();

        assert_eq!(ctrl.metrics().denied_quota, 1);
    }

    #[test]
    fn the_rate_limit_denies_a_burst_from_one_client() {
        let ctrl = AdmissionControl::new(
            AdmissionPolicy::Open,
            ControlConfig {
                rate_limit: RateLimit {
                    capacity: 2,
                    refill_tokens: 1,
                    refill_ms: 1_000,
                    max_clients: 64,
                },
                // Generous quota so the rate limit, not the quota, is what bites.
                quota: SessionQuota {
                    global_max: 100,
                    per_client_max: 100,
                },
                ..ControlConfig::default()
            },
        );
        // Two admissions exhaust the burst...
        let _a = ctrl.admit(None, "peer", 0).unwrap();
        let _b = ctrl.admit(None, "peer", 0).unwrap();
        // ...the third at the same instant is rate-limited.
        let err = ctrl.admit(None, "peer", 0).unwrap_err();
        assert!(matches!(err, Denied::RateLimited));
        // After a refill interval, admission is possible again.
        let _c = ctrl.admit(None, "peer", 1_000).unwrap();

        assert_eq!(ctrl.metrics().denied_rate_limit, 1);
    }

    #[test]
    fn denials_never_reserve_a_slot() {
        let ctrl = shared_token_control();
        let wrong = AdmissionToken::from_text("wrong");
        let _ = ctrl.admit(Some(&wrong), "peer", 0);
        assert_eq!(ctrl.metrics().sessions_active, 0);
    }
}
