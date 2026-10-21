//! A fan-out stop signal that tears every live splice down fail-closed
//! (ST-M007-002).
//!
//! The relay's stop path must fail closed (`Stop and revocation must fail
//! closed`): when the controlling side revokes or the controller itself
//! vanishes, every direction of every affected splice must stop, never linger
//! and never fabricate a clean close. This is a thin, cloneable wrapper over a
//! [`tokio::sync::watch`] channel: one [`StopSignal`] producer, many
//! [`StopHandle`] consumers.

use tokio::sync::watch;

/// The producer half. Calling [`StopSignal::stop`] — or dropping the signal —
/// releases every [`StopHandle`], so a controller that crashes still fails the
/// splice closed rather than leaving it forwarding.
#[derive(Debug)]
pub struct StopSignal {
    tx: watch::Sender<bool>,
}

/// A cloneable consumer handed to each splice direction. Every clone observes
/// the same stop edge.
#[derive(Debug, Clone)]
pub struct StopHandle {
    rx: watch::Receiver<bool>,
}

impl StopSignal {
    /// Creates a fresh, un-stopped signal and one handle to hand out (clone the
    /// handle for additional consumers).
    #[must_use]
    pub fn new() -> (Self, StopHandle) {
        let (tx, rx) = watch::channel(false);
        (Self { tx }, StopHandle { rx })
    }

    /// Signals every live handle to tear down. Idempotent. A send error means
    /// every handle was already dropped, so there is nothing left to stop.
    pub fn stop(&self) {
        let _ = self.tx.send(true);
    }

    /// Whether [`stop`](Self::stop) has already fired.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.tx.borrow()
    }
}

impl StopHandle {
    /// Resolves as soon as the signal is stopped — or was already stopped, or
    /// the controller was dropped. It never resolves spuriously while the
    /// session is meant to stay live, so a splice can `select!` on it safely.
    pub async fn stopped(&mut self) {
        if *self.rx.borrow() {
            return;
        }
        // `changed()` errors only when the sender was dropped. A dropped
        // controller is a stop: fail closed rather than wait forever.
        while self.rx.changed().await.is_ok() {
            if *self.rx.borrow() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stop_releases_every_handle() {
        let (signal, mut a) = StopSignal::new();
        let mut b = a.clone();
        assert!(!signal.is_stopped());
        signal.stop();
        assert!(signal.is_stopped());
        // Both consumers observe the same edge without blocking.
        tokio::time::timeout(Duration::from_secs(1), a.stopped())
            .await
            .expect("handle a resolves");
        tokio::time::timeout(Duration::from_secs(1), b.stopped())
            .await
            .expect("handle b resolves");
    }

    #[tokio::test]
    async fn dropping_the_controller_fails_closed() {
        let (signal, mut handle) = StopSignal::new();
        drop(signal);
        // A vanished controller must not leave the splice waiting.
        tokio::time::timeout(Duration::from_secs(1), handle.stopped())
            .await
            .expect("dropped controller resolves the handle");
    }

    #[tokio::test]
    async fn already_stopped_resolves_immediately() {
        let (signal, handle) = StopSignal::new();
        signal.stop();
        let late = handle.clone();
        drop(handle);
        let mut late = late;
        tokio::time::timeout(Duration::from_secs(1), late.stopped())
            .await
            .expect("a handle cloned after stop still resolves");
    }
}
