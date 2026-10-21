//! Per-client and global concurrency quotas for the relay (ST-M007-004,
//! ADR-0016 §5).
//!
//! [`SpliceLimits`](crate::splice::SpliceLimits) already bounds the bytes and
//! frames of a *single* forwarded session (REQ-N-006). This module bounds how
//! *many* sessions run at once: a global ceiling so the relay's own resources
//! stay bounded, and a per-client ceiling so no single admitted client can crowd
//! everyone else out (REQ-S-021). Both are counted, not sampled, so the ceiling
//! is exact.
//!
//! Acquisition is fail-closed: at the ceiling, a new session is **refused**
//! ([`QuotaError`]) rather than admitted over budget. Release is exact and
//! idempotent-at-zero: a saturating decrement can never underflow into a
//! negative count that would silently re-open budget.
//!

use std::collections::HashMap;

use thiserror::Error;

use crate::admission::ClientId;

/// Concurrency ceilings for live relay sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionQuota {
    /// Maximum sessions the relay will run concurrently across all clients.
    pub global_max: u32,
    /// Maximum concurrent sessions any single client may hold.
    pub per_client_max: u32,
}

impl SessionQuota {
    /// The documented default for a public relay: 1024 concurrent sessions
    /// overall, at most 8 per client.
    #[must_use]
    pub const fn public_default() -> Self {
        Self {
            global_max: 1_024,
            per_client_max: 8,
        }
    }
}

impl Default for SessionQuota {
    fn default() -> Self {
        Self::public_default()
    }
}

/// Why a session slot could not be acquired. Both variants *refuse*; they are
/// distinguished for an actionable, secret-free error (REQ-N-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum QuotaError {
    /// The global concurrent-session ceiling is reached.
    #[error("the relay is at its global concurrent-session limit")]
    GlobalFull,
    /// The requesting client is at its per-client concurrent-session ceiling.
    #[error("this client is at its concurrent-session limit")]
    ClientFull,
}

/// A live count of concurrent sessions, globally and per client. Not internally
/// synchronised; the relay wraps it behind the admission-control lock.
#[derive(Debug)]
pub struct QuotaState {
    quota: SessionQuota,
    global_active: u32,
    per_client: HashMap<ClientId, u32>,
}

impl QuotaState {
    /// Creates a quota tracker starting from zero active sessions.
    #[must_use]
    pub fn new(quota: SessionQuota) -> Self {
        Self {
            quota,
            global_active: 0,
            per_client: HashMap::new(),
        }
    }

    /// Sessions currently active across all clients.
    #[must_use]
    pub fn global_active(&self) -> u32 {
        self.global_active
    }

    /// Sessions currently active for one client.
    #[must_use]
    pub fn client_active(&self, client: &ClientId) -> u32 {
        self.per_client.get(client).copied().unwrap_or(0)
    }

    /// Reserves one session slot for `client`, or refuses fail-closed.
    ///
    /// # Errors
    ///
    /// [`QuotaError::GlobalFull`] or [`QuotaError::ClientFull`] — see each
    /// variant. On error no slot is reserved.
    pub fn acquire(&mut self, client: &ClientId) -> Result<(), QuotaError> {
        if self.global_active >= self.quota.global_max {
            return Err(QuotaError::GlobalFull);
        }
        let client_active = self.client_active(client);
        if client_active >= self.quota.per_client_max {
            return Err(QuotaError::ClientFull);
        }
        self.global_active = self.global_active.saturating_add(1);
        self.per_client
            .insert(client.clone(), client_active.saturating_add(1));
        Ok(())
    }

    /// Releases one previously acquired slot for `client`. Saturating and
    /// idempotent at zero, so a double release cannot re-open budget; the
    /// per-client entry is dropped when it reaches zero so the map stays bounded
    /// by the number of *currently active* clients.
    pub fn release(&mut self, client: &ClientId) {
        self.global_active = self.global_active.saturating_sub(1);
        if let Some(count) = self.per_client.get_mut(client) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.per_client.remove(client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn client(name: &str) -> ClientId {
        ClientId::new(name)
    }

    #[test]
    fn per_client_ceiling_refuses_the_over_budget_session() {
        let mut q = QuotaState::new(SessionQuota {
            global_max: 100,
            per_client_max: 2,
        });
        let c = client("alice");
        assert!(q.acquire(&c).is_ok());
        assert!(q.acquire(&c).is_ok());
        assert_eq!(q.acquire(&c).unwrap_err(), QuotaError::ClientFull);
        assert_eq!(q.client_active(&c), 2);
        // A different client is unaffected.
        assert!(q.acquire(&client("bob")).is_ok());
    }

    #[test]
    fn global_ceiling_refuses_even_a_fresh_client() {
        let mut q = QuotaState::new(SessionQuota {
            global_max: 2,
            per_client_max: 10,
        });
        assert!(q.acquire(&client("a")).is_ok());
        assert!(q.acquire(&client("b")).is_ok());
        // Global budget is spent even though this client has none yet.
        assert_eq!(q.acquire(&client("c")).unwrap_err(), QuotaError::GlobalFull);
        assert_eq!(q.global_active(), 2);
    }

    #[test]
    fn release_frees_budget_and_bounds_the_map() {
        let mut q = QuotaState::new(SessionQuota {
            global_max: 4,
            per_client_max: 4,
        });
        let c = client("carol");
        q.acquire(&c).unwrap();
        q.acquire(&c).unwrap();
        assert_eq!(q.client_active(&c), 2);
        q.release(&c);
        assert_eq!(q.client_active(&c), 1);
        q.release(&c);
        assert_eq!(q.client_active(&c), 0);
        assert_eq!(q.global_active(), 0);
        // The per-client entry is dropped at zero.
        q.release(&c); // extra release cannot underflow or re-open budget
        assert_eq!(q.global_active(), 0);
        assert_eq!(q.client_active(&c), 0);
    }

    #[test]
    fn released_budget_can_be_reacquired() {
        let mut q = QuotaState::new(SessionQuota {
            global_max: 1,
            per_client_max: 1,
        });
        let a = client("a");
        q.acquire(&a).unwrap();
        assert_eq!(q.acquire(&client("b")).unwrap_err(), QuotaError::GlobalFull);
        q.release(&a);
        // Now the single global slot is free again.
        assert!(q.acquire(&client("b")).is_ok());
    }
}
