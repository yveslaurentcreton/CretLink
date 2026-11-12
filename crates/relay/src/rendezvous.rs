//! Outbound-rendezvous matching for the relay (ST-M007-002, ADR-0016).
//!
//! Both endpoints of one session dial the relay **outbound** and present the
//! same addressing-only [`RendezvousTicket`]. The first arrival is *parked*; the
//! second arrival on the same ticket is *matched*, and the two connections are
//! handed back to be [`splice`](crate::splice::splice)d into one opaque pipe.
//!
//! The matcher is deliberately minimal and holds no session authority. A ticket
//! only routes an arrival to a waiting peer — it never authenticates a session
//! (authenticity is the end-to-end Noise handshake and the human-compared
//! Session Approval Code, ADR-0003). Tickets are single-use here: a matched
//! ticket is removed from the pending map, so a replayed ticket finds nothing
//! and parks anew rather than hijacking a live session.
//!
//! # Fail-closed
//!
//! A parked arrival whose peer never comes returns [`RendezvousError::Timeout`]
//! and its connection is dropped, never left waiting forever. A poisoned lock —
//! a bug elsewhere in the relay — returns [`RendezvousError::Poisoned`] rather
//! than forwarding through unknown state. A full pending table refuses new parks
//! with [`RendezvousError::AtCapacity`] so a flood of half-open rendezvous
//! cannot exhaust relay memory.
//!

use core::fmt;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::oneshot;

use crate::ticket::RendezvousTicket;

/// Default ceiling on simultaneously parked (half-open) rendezvous.
pub const DEFAULT_MAX_PENDING: usize = 1024;

/// A connection parked under a ticket, waiting for its peer.
struct Pending<S> {
    /// Distinguishes this park from a later one that reuses the same ticket, so
    /// a timed-out waiter only ever reclaims *its own* slot.
    id: u64,
    conn: S,
    /// Wakes the parked task when the second arrival takes this entry.
    notify: oneshot::Sender<()>,
}

/// The outcome of a [`Rendezvous::join`].
pub enum Matched<S> {
    /// This arrival matched a waiting peer. `first` is the earlier (parked)
    /// connection, `second` is this one; hand both to
    /// [`splice`](crate::splice::splice).
    Pair {
        /// The connection that arrived and parked first.
        first: S,
        /// The connection that arrived second (this call).
        second: S,
    },
    /// This arrival parked and its peer took it; the peer received the
    /// [`Pair`](Self::Pair) and drives the splice, so this side is done.
    Parked,
}

/// Why a [`Rendezvous::join`] did not produce a live pair.
#[derive(Debug, Error)]
pub enum RendezvousError {
    /// No peer arrived on this ticket within the timeout. The parked connection
    /// was dropped (fail-closed).
    #[error("no rendezvous peer arrived before the timeout")]
    Timeout,
    /// The pending table's lock was poisoned by a panic elsewhere; the relay
    /// refuses to match through unknown state.
    #[error("rendezvous table lock was poisoned")]
    Poisoned,
    /// The pending table is full; this half-open park was refused.
    #[error("rendezvous table is at capacity")]
    AtCapacity,
}

/// A concurrent rendezvous table mapping tickets to parked connections.
///
/// `S` is the relay's per-endpoint connection type (a `wss` stream in
/// production, an in-memory duplex in tests). The table only moves `S` values
/// between the two arrivals — it never reads or writes their bytes.
pub struct Rendezvous<S> {
    pending: Mutex<HashMap<RendezvousTicket, Pending<S>>>,
    max_pending: usize,
    next_id: AtomicU64,
}

impl<S> Rendezvous<S> {
    /// A table with the [default capacity](DEFAULT_MAX_PENDING).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_PENDING)
    }

    /// A table that parks at most `max_pending` half-open rendezvous at once.
    #[must_use]
    pub fn with_capacity(max_pending: usize) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            max_pending,
            next_id: AtomicU64::new(0),
        }
    }

    /// Number of currently parked (half-open) rendezvous.
    ///
    /// # Errors
    ///
    /// Returns [`RendezvousError::Poisoned`] if the lock was poisoned.
    pub fn pending_len(&self) -> Result<usize, RendezvousError> {
        let map = self.pending.lock().map_err(|_| RendezvousError::Poisoned)?;
        Ok(map.len())
    }

    /// Whether an arrival on `ticket` would find a peer or a place to park
    /// right now. A relay asks this before it tells an endpoint that it is
    /// admitted, so that a full table is answered as "no free place" instead
    /// of admitted and then dropped (ST-M007-014).
    ///
    /// The answer is a snapshot: another arrival can take the last place
    /// before [`join`](Self::join) is called, and `join` then still refuses
    /// with [`RendezvousError::AtCapacity`].
    ///
    /// # Errors
    ///
    /// Returns [`RendezvousError::Poisoned`] if the lock was poisoned.
    pub fn has_room_for(&self, ticket: &RendezvousTicket) -> Result<bool, RendezvousError> {
        let map = self.pending.lock().map_err(|_| RendezvousError::Poisoned)?;
        Ok(map.contains_key(ticket) || map.len() < self.max_pending)
    }

    /// Joins the rendezvous named by `ticket` with connection `conn`.
    ///
    /// If a peer is already parked on `ticket`, returns [`Matched::Pair`]
    /// immediately with both connections. Otherwise parks `conn` and waits up to
    /// `timeout` for a peer; if one arrives, returns [`Matched::Parked`]; if not,
    /// returns [`RendezvousError::Timeout`] and drops `conn`.
    ///
    /// # Errors
    ///
    /// [`RendezvousError::Timeout`], [`RendezvousError::AtCapacity`], or
    /// [`RendezvousError::Poisoned`] — see each variant.
    pub async fn join(
        &self,
        ticket: RendezvousTicket,
        conn: S,
        timeout: Duration,
    ) -> Result<Matched<S>, RendezvousError> {
        // First critical section: either take a waiting peer, or park.
        let (wait, my_id) = {
            let mut map = self.pending.lock().map_err(|_| RendezvousError::Poisoned)?;

            // A peer parked on this ticket? Take it — single-use — and wake it.
            // If the wake fails, the parked peer already gave up (its receiver is
            // gone) and its connection is being dropped; its entry is now removed,
            // so we fall through and park this arrival fresh under the ticket.
            if let Some(Pending {
                conn: first,
                notify,
                ..
            }) = map.remove(&ticket)
                && notify.send(()).is_ok()
            {
                return Ok(Matched::Pair {
                    first,
                    second: conn,
                });
            }

            if map.len() >= self.max_pending {
                return Err(RendezvousError::AtCapacity);
            }

            let my_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            map.insert(
                ticket.clone(),
                Pending {
                    id: my_id,
                    conn,
                    notify: tx,
                },
            );
            (rx, my_id)
        };

        // Outside the lock: wait for a peer to take our parked connection.
        match tokio::time::timeout(timeout, wait).await {
            // A peer took our slot and signalled us; it drives the splice.
            Ok(Ok(())) => Ok(Matched::Parked),
            // Sender dropped without signalling: no live peer.
            Ok(Err(_)) => Err(RendezvousError::Timeout),
            Err(_elapsed) => {
                // Timed out. Reclaim our slot fail-closed — but only if it is
                // still *ours*. If it is gone or a later park reused the ticket,
                // a peer matched us in the race and drives the splice.
                let mut map = self.pending.lock().map_err(|_| RendezvousError::Poisoned)?;
                let is_mine = match map.get(&ticket) {
                    Some(pending) => pending.id == my_id,
                    None => false,
                };
                if is_mine {
                    map.remove(&ticket);
                    Err(RendezvousError::Timeout)
                } else {
                    Ok(Matched::Parked)
                }
            }
        }
    }
}

impl<S> Default for Rendezvous<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// Manual `Debug` that never bounds or exposes `S`: the table's contents are
/// live connections, not printable state.
impl<S> fmt::Debug for Rendezvous<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pending = self.pending.lock().map(|m| m.len());
        let mut dbg = f.debug_struct("Rendezvous");
        match pending {
            Ok(len) => dbg.field("pending", &len),
            Err(_) => dbg.field("pending", &"<poisoned>"),
        }
        .field("max_pending", &self.max_pending)
        .finish()
    }
}

/// Manual `Debug` that never bounds or exposes `S`.
impl<S> fmt::Debug for Matched<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pair { .. } => f.write_str("Matched::Pair"),
            Self::Parked => f.write_str("Matched::Parked"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;

    // The matcher only moves the connection value; a plain integer stands in for
    // a real stream so the tests exercise routing, not I/O.
    type Conn = u32;

    fn ticket(seed: u8) -> RendezvousTicket {
        RendezvousTicket::from_bytes([seed; RENDEZVOUS_TICKET_LEN])
    }
    use crate::ticket::RENDEZVOUS_TICKET_LEN;

    // Two arrivals on the same ticket pair up; the second caller receives both
    // connections and the first caller learns it was parked-and-taken.
    #[tokio::test]
    async fn two_arrivals_pair_up() {
        let rv: Arc<Rendezvous<Conn>> = Arc::new(Rendezvous::new());

        let first = {
            let rv = Arc::clone(&rv);
            tokio::spawn(async move { rv.join(ticket(1), 100, Duration::from_secs(5)).await })
        };

        // Let the first arrival park before the second joins.
        tokio::task::yield_now().await;
        while rv.pending_len().unwrap() == 0 {
            tokio::task::yield_now().await;
        }

        let second = rv
            .join(ticket(1), 200, Duration::from_secs(5))
            .await
            .unwrap();
        match second {
            Matched::Pair { first, second } => {
                assert_eq!(first, 100, "earlier arrival is `first`");
                assert_eq!(second, 200, "this arrival is `second`");
            }
            Matched::Parked => panic!("second arrival should match, not park"),
        }

        assert!(matches!(first.await.unwrap().unwrap(), Matched::Parked));
        assert_eq!(rv.pending_len().unwrap(), 0, "ticket consumed after match");
    }

    // A lone arrival whose peer never comes fails closed with Timeout, and its
    // slot is reclaimed so the table does not leak.
    #[tokio::test(start_paused = true)]
    async fn lone_arrival_times_out_fail_closed() {
        let rv: Rendezvous<Conn> = Rendezvous::new();
        let err = rv
            .join(ticket(7), 42, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, RendezvousError::Timeout));
        assert_eq!(rv.pending_len().unwrap(), 0, "timed-out slot reclaimed");
    }

    // Different tickets never cross-match: each parks independently.
    #[tokio::test(start_paused = true)]
    async fn distinct_tickets_do_not_cross_match() {
        let rv: Rendezvous<Conn> = Rendezvous::new();
        let a = rv.join(ticket(1), 1, Duration::from_millis(10));
        let b = rv.join(ticket(2), 2, Duration::from_millis(10));
        let (ra, rb) = tokio::join!(a, b);
        assert!(matches!(ra.unwrap_err(), RendezvousError::Timeout));
        assert!(matches!(rb.unwrap_err(), RendezvousError::Timeout));
    }

    // A full table refuses to park a new half-open rendezvous.
    #[tokio::test(start_paused = true)]
    async fn at_capacity_refuses_new_parks() {
        let rv: Arc<Rendezvous<Conn>> = Arc::new(Rendezvous::with_capacity(1));

        // Occupy the single slot with a parked arrival.
        let parked = {
            let rv = Arc::clone(&rv);
            tokio::spawn(async move { rv.join(ticket(1), 1, Duration::from_secs(60)).await })
        };
        while rv.pending_len().unwrap() == 0 {
            tokio::task::yield_now().await;
        }

        // A different ticket cannot find a free slot.
        let err = rv
            .join(ticket(2), 2, Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(matches!(err, RendezvousError::AtCapacity));

        parked.abort();
    }

    // A relay can ask beforehand whether an arrival would find a peer or a
    // place, so that it does not admit a leg it must then drop.
    #[tokio::test(start_paused = true)]
    async fn a_full_table_has_room_only_for_the_peer_of_a_parked_leg() {
        let rv: Arc<Rendezvous<Conn>> = Arc::new(Rendezvous::with_capacity(1));
        assert!(
            rv.has_room_for(&ticket(1)).unwrap(),
            "an empty table has room"
        );

        let parked = {
            let rv = Arc::clone(&rv);
            tokio::spawn(async move { rv.join(ticket(1), 1, Duration::from_secs(60)).await })
        };
        while rv.pending_len().unwrap() == 0 {
            tokio::task::yield_now().await;
        }

        // The table is full: another ticket finds no place, the peer of the
        // parked leg needs none.
        assert!(!rv.has_room_for(&ticket(2)).unwrap());
        assert!(rv.has_room_for(&ticket(1)).unwrap());

        // The peer arrives and takes the parked leg; the table has room again.
        let matched = rv
            .join(ticket(1), 2, Duration::from_secs(60))
            .await
            .unwrap();
        assert!(matches!(
            matched,
            Matched::Pair {
                first: 1,
                second: 2
            }
        ));
        assert!(rv.has_room_for(&ticket(2)).unwrap());
        assert!(matches!(parked.await.unwrap(), Ok(Matched::Parked)));
    }
}
