//! A deterministic token-bucket rate limiter for relay admission attempts
//! (ST-M007-004, ADR-0016 §4).
//!
//! A publicly reachable relay must bound how fast any one client may open
//! rendezvous connections, or a single client becomes free abuse infrastructure
//! (REQ-S-021) and can exhaust the shared rendezvous table (REQ-N-006). This is
//! a classic token bucket: each client has a bucket that refills at a fixed rate
//! up to a burst ceiling, and each admission attempt spends one token.
//!
//! # Deterministic time
//!
//! The limiter takes the current time as an explicit monotonic **millisecond**
//! stamp on every call rather than reading a clock. The relay server maps real
//! monotonic time onto it; tests pass exact stamps, so the limiter's behaviour
//! is fully reproducible and needs no wall clock (REQ-N-010). Integer refill
//! avoids any floating-point drift.
//!
//! # Bounded memory
//!
//! The per-client bucket map is itself a resource. It is capped at
//! [`RateLimit::max_clients`]; when full, an **idle** bucket (one refilled back
//! to its burst ceiling, so it carries no live rate state) is evicted to admit a
//! new client. If every tracked bucket is currently rate-limited, a further new
//! client is denied — fail-closed, never an unbounded map.
//!

use std::collections::HashMap;

use crate::admission::ClientId;

/// Token-bucket parameters shared by every client's bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Maximum tokens a bucket can hold — the burst ceiling. A fresh bucket
    /// starts full.
    pub capacity: u32,
    /// Tokens added each [`refill_ms`](Self::refill_ms) interval.
    pub refill_tokens: u32,
    /// Length of one refill interval, in milliseconds.
    pub refill_ms: u64,
    /// Maximum number of distinct clients tracked at once. Bounds limiter
    /// memory; see the [module docs](self).
    pub max_clients: usize,
}

impl RateLimit {
    /// The documented default for a public relay: a burst of 30 admission
    /// attempts, refilling one token per second (a sustained 1 rendezvous/sec),
    /// tracking up to 4096 distinct clients.
    #[must_use]
    pub const fn public_default() -> Self {
        Self {
            capacity: 30,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 4_096,
        }
    }
}

impl Default for RateLimit {
    fn default() -> Self {
        Self::public_default()
    }
}

/// One client's bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    /// The last stamp the bucket was refilled to, advanced only by whole refill
    /// intervals so fractional time is never lost.
    updated_ms: u64,
}

/// A per-client token-bucket rate limiter. Not internally synchronised; the
/// relay wraps it behind the admission-control lock.
#[derive(Debug)]
pub struct RateLimiter {
    limit: RateLimit,
    buckets: HashMap<ClientId, Bucket>,
}

impl RateLimiter {
    /// Creates a limiter with the given parameters.
    #[must_use]
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            buckets: HashMap::new(),
        }
    }

    /// Number of distinct clients currently tracked.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.buckets.len()
    }

    /// Tries to admit one attempt from `client` at `now_ms`. Returns `true` if a
    /// token was available and spent, `false` if the client is rate-limited (or
    /// the limiter is full and cannot track a new client).
    #[must_use]
    pub fn check(&mut self, client: &ClientId, now_ms: u64) -> bool {
        if let Some(bucket) = self.buckets.get_mut(client) {
            let limit = self.limit;
            // Inlined refill to satisfy the borrow checker without re-lookup.
            refill_bucket(bucket, &limit, now_ms);
            if bucket.tokens == 0 {
                return false;
            }
            bucket.tokens -= 1;
            return true;
        }

        // New client. Make room if the table is full.
        if self.buckets.len() >= self.limit.max_clients && !self.evict_one_idle(now_ms) {
            // Every tracked bucket is actively rate-limited; refuse fail-closed
            // rather than grow the map without bound.
            return false;
        }

        // A fresh bucket starts full; this first attempt spends one token.
        let mut bucket = Bucket {
            tokens: self.limit.capacity,
            updated_ms: now_ms,
        };
        if bucket.tokens == 0 {
            // A degenerate zero-capacity limit denies everyone.
            return false;
        }
        bucket.tokens -= 1;
        self.buckets.insert(client.clone(), bucket);
        true
    }

    /// Evicts one bucket that has refilled back to the burst ceiling by
    /// `now_ms` (so it carries no live rate state to lose). Returns whether one
    /// was evicted.
    ///
    /// Idleness is judged on the bucket's *projected* token count at `now_ms`,
    /// not its last stored count: a client that drained its bucket and then went
    /// quiet refills silently (it is never re-checked), so without projecting we
    /// would treat a long-idle, fully-refilled bucket as active forever and
    /// refuse every new client once the table filled. A bucket back at capacity
    /// is indistinguishable from a fresh one — fresh buckets start full — so
    /// evicting it can never reset or bypass an active limit.
    fn evict_one_idle(&mut self, now_ms: u64) -> bool {
        let limit = self.limit;
        let victim = self
            .buckets
            .iter()
            .find(|(_, b)| projected_tokens(b, &limit, now_ms) >= limit.capacity)
            .map(|(id, _)| id.clone());
        match victim {
            Some(id) => {
                self.buckets.remove(&id);
                true
            }
            None => false,
        }
    }
}

/// Computes what a bucket's token count would be after refilling to `now_ms`,
/// without mutating it. Refill advances only by whole intervals and never
/// exceeds the burst ceiling; a non-monotonic stamp yields the stored count
/// unchanged.
fn projected_tokens(bucket: &Bucket, limit: &RateLimit, now_ms: u64) -> u32 {
    if limit.refill_ms == 0 || limit.refill_tokens == 0 || now_ms <= bucket.updated_ms {
        return bucket.tokens;
    }
    let intervals = (now_ms - bucket.updated_ms) / limit.refill_ms;
    if intervals == 0 {
        return bucket.tokens;
    }
    let added = intervals.saturating_mul(u64::from(limit.refill_tokens));
    let raised = u64::from(bucket.tokens).saturating_add(added);
    raised.min(u64::from(limit.capacity)) as u32
}

/// Refills a bucket in place to `now_ms`: raises its tokens by
/// [`projected_tokens`] and advances `updated_ms` by the same whole intervals so
/// fractional time is never lost. Called by `check` on a mutably-borrowed bucket
/// without re-borrowing `self`.
fn refill_bucket(bucket: &mut Bucket, limit: &RateLimit, now_ms: u64) {
    if limit.refill_ms == 0 || limit.refill_tokens == 0 || now_ms <= bucket.updated_ms {
        return;
    }
    let intervals = (now_ms - bucket.updated_ms) / limit.refill_ms;
    if intervals == 0 {
        return;
    }
    bucket.tokens = projected_tokens(bucket, limit, now_ms);
    bucket.updated_ms = bucket
        .updated_ms
        .saturating_add(intervals.saturating_mul(limit.refill_ms));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn client(name: &str) -> ClientId {
        ClientId::new(name)
    }

    #[test]
    fn burst_up_to_capacity_then_denies() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 3,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        let c = client("alice");
        // Three attempts at the same instant drain the burst.
        assert!(rl.check(&c, 0));
        assert!(rl.check(&c, 0));
        assert!(rl.check(&c, 0));
        // The fourth is denied — no time has passed to refill.
        assert!(!rl.check(&c, 0));
    }

    #[test]
    fn refills_over_time_by_whole_intervals() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 2,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        let c = client("bob");
        assert!(rl.check(&c, 0));
        assert!(rl.check(&c, 0));
        assert!(!rl.check(&c, 0), "burst drained");
        // 999ms is not a full interval: still denied, and the fractional time is
        // not lost.
        assert!(!rl.check(&c, 999));
        // At 1000ms one token has refilled.
        assert!(rl.check(&c, 1_000));
        assert!(!rl.check(&c, 1_000), "only one refilled");
        // The earlier 999ms was preserved: one more full second yields a token
        // at 2000ms, not 1999ms.
        assert!(!rl.check(&c, 1_999));
        assert!(rl.check(&c, 2_000));
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 2,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        let c = client("carol");
        assert!(rl.check(&c, 0));
        assert!(rl.check(&c, 0));
        // A long idle period cannot bank more than the burst ceiling.
        assert!(rl.check(&c, 1_000_000));
        assert!(rl.check(&c, 1_000_000));
        assert!(
            !rl.check(&c, 1_000_000),
            "capped at capacity, not unbounded"
        );
    }

    #[test]
    fn distinct_clients_have_independent_buckets() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 1,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        assert!(rl.check(&client("a"), 0));
        assert!(!rl.check(&client("a"), 0), "a drained");
        // b is unaffected by a's spend.
        assert!(rl.check(&client("b"), 0));
    }

    #[test]
    fn non_monotonic_stamp_does_not_refill() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 1,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        let c = client("dave");
        assert!(rl.check(&c, 10_000));
        // A stamp earlier than the last update must not grant a token.
        assert!(!rl.check(&c, 5_000));
    }

    #[test]
    fn full_table_evicts_idle_but_refuses_when_all_limited() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 1,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 2,
        });
        // Two clients each drain their single token at t=0 → both actively
        // limited, table full.
        assert!(rl.check(&client("x"), 0));
        assert!(rl.check(&client("y"), 0));
        assert_eq!(rl.tracked(), 2);
        // A third client at the same instant cannot be tracked: neither bucket
        // has refilled, so there is no idle bucket to evict → denied fail-closed,
        // and the map stays bounded.
        assert!(!rl.check(&client("z"), 0));
        assert_eq!(rl.tracked(), 2, "map stayed bounded");

        // By t=2000 both x and y have silently refilled back to their burst
        // ceiling (1 token/sec, capacity 1) — they were never re-checked, but
        // idleness is judged on projected tokens. A new client can now evict one
        // fully-refilled bucket and be admitted, without exceeding the cap.
        assert!(rl.check(&client("z"), 2_000));
        assert_eq!(rl.tracked(), 2, "still bounded after eviction");
    }

    #[test]
    fn zero_capacity_denies_everyone() {
        let mut rl = RateLimiter::new(RateLimit {
            capacity: 0,
            refill_tokens: 1,
            refill_ms: 1_000,
            max_clients: 16,
        });
        assert!(!rl.check(&client("a"), 0));
    }
}
