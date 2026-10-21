//! Operational metrics for the relay (ST-M007-004, ADR-0016 §5).
//!
//! A relay operator needs to see how the forwarder is behaving — how many
//! sessions were admitted, how many were denied and why, how many run right now,
//! and how much traffic passed — without ever seeing session content. These are
//! aggregate, metadata-only counters (REQ-S-017): counts and an outcome tag,
//! never a ticket, a credential, or a forwarded byte (REQ-N-017).
//!
//! The counters are atomics so the relay can update them from concurrent session
//! tasks without a lock, and [`snapshot`](RelayMetrics::snapshot) reads a
//! consistent-enough view for a metrics endpoint or log line. `Relaxed` ordering
//! suffices: the counters carry no cross-counter invariant a reader relies on.
//!

use std::sync::atomic::{AtomicU64, Ordering};

/// Live operational counters for one relay instance. Cheap to share behind an
/// `Arc`; every method is lock-free.
#[derive(Debug, Default)]
pub struct RelayMetrics {
    sessions_admitted: AtomicU64,
    denied_auth: AtomicU64,
    denied_rate_limit: AtomicU64,
    denied_quota: AtomicU64,
    denied_unavailable: AtomicU64,
    sessions_active: AtomicU64,
    sessions_ended: AtomicU64,
    bytes_forwarded: AtomicU64,
    frames_forwarded: AtomicU64,
}

impl RelayMetrics {
    /// A fresh set of zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record_admitted(&self) {
        self.sessions_admitted.fetch_add(1, Ordering::Relaxed);
        self.sessions_active.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_denied_auth(&self) {
        self.denied_auth.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_denied_rate_limit(&self) {
        self.denied_rate_limit.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_denied_quota(&self) {
        self.denied_quota.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_denied_unavailable(&self) {
        self.denied_unavailable.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a session ending, decrementing the active gauge (saturating, so
    /// it can never wrap below zero) and folding in the forwarded totals.
    pub(crate) fn record_ended(&self, bytes: u64, frames: u64) {
        self.sessions_ended.fetch_add(1, Ordering::Relaxed);
        // Saturating decrement: load/compare/store in a small CAS loop.
        let mut active = self.sessions_active.load(Ordering::Relaxed);
        loop {
            let next = active.saturating_sub(1);
            match self.sessions_active.compare_exchange_weak(
                active,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => active = observed,
            }
        }
        self.bytes_forwarded.fetch_add(bytes, Ordering::Relaxed);
        self.frames_forwarded.fetch_add(frames, Ordering::Relaxed);
    }

    /// Reads a point-in-time snapshot of every counter.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            sessions_admitted: self.sessions_admitted.load(Ordering::Relaxed),
            denied_auth: self.denied_auth.load(Ordering::Relaxed),
            denied_rate_limit: self.denied_rate_limit.load(Ordering::Relaxed),
            denied_quota: self.denied_quota.load(Ordering::Relaxed),
            denied_unavailable: self.denied_unavailable.load(Ordering::Relaxed),
            sessions_active: self.sessions_active.load(Ordering::Relaxed),
            sessions_ended: self.sessions_ended.load(Ordering::Relaxed),
            bytes_forwarded: self.bytes_forwarded.load(Ordering::Relaxed),
            frames_forwarded: self.frames_forwarded.load(Ordering::Relaxed),
        }
    }
}

/// A metadata-only, point-in-time view of [`RelayMetrics`]. Safe to log or
/// expose on a metrics endpoint: it carries no session content or secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MetricsSnapshot {
    /// Sessions admitted to the forwarder over the instance's lifetime.
    pub sessions_admitted: u64,
    /// Connections denied because the admission credential was missing or wrong.
    pub denied_auth: u64,
    /// Connections denied because the client was rate-limited.
    pub denied_rate_limit: u64,
    /// Connections denied because a concurrency quota was full.
    pub denied_quota: u64,
    /// Connections denied because the relay was temporarily unavailable (for
    /// example a poisoned internal lock — always fail-closed).
    pub denied_unavailable: u64,
    /// Sessions running right now (admitted minus ended).
    pub sessions_active: u64,
    /// Sessions that have ended over the instance's lifetime.
    pub sessions_ended: u64,
    /// Total wire bytes forwarded across all ended sessions (both directions).
    pub bytes_forwarded: u64,
    /// Total frames forwarded across all ended sessions (both directions).
    pub frames_forwarded: u64,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn counts_admissions_denials_and_traffic() {
        let m = RelayMetrics::new();
        m.record_admitted();
        m.record_admitted();
        m.record_denied_auth();
        m.record_denied_rate_limit();
        m.record_denied_quota();
        m.record_denied_unavailable();
        m.record_ended(100, 3);

        let s = m.snapshot();
        assert_eq!(s.sessions_admitted, 2);
        assert_eq!(s.denied_auth, 1);
        assert_eq!(s.denied_rate_limit, 1);
        assert_eq!(s.denied_quota, 1);
        assert_eq!(s.denied_unavailable, 1);
        assert_eq!(s.sessions_active, 1, "two admitted, one ended");
        assert_eq!(s.sessions_ended, 1);
        assert_eq!(s.bytes_forwarded, 100);
        assert_eq!(s.frames_forwarded, 3);
    }

    #[test]
    fn active_gauge_never_wraps_below_zero() {
        let m = RelayMetrics::new();
        // An end with no matching admit must not underflow the active gauge.
        m.record_ended(0, 0);
        assert_eq!(m.snapshot().sessions_active, 0);
    }
}
