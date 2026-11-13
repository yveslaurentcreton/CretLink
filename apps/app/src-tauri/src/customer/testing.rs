//! What the tests of a customer session share (ST-M007-013): the role's own
//! session state, dial path and pump, with a window that keeps every view it
//! was given.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;

use super::session_driver::{ViewSink, lock};
use cretlink_session::ActivityEntry;

use super::{
    CommandError, ConsentSession, ConsentView, Typed, ensure_can_connect, grant_now, prepare_dial,
    relay_next_to, stop_now, typed_target,
};
use crate::relay;

/// How long one step of a session inside this process may take.
const STEP: Duration = Duration::from_secs(20);

/// A window that keeps every view the session rendered.
#[derive(Debug)]
struct Window {
    latest: watch::Sender<Option<ConsentView>>,
    all: Mutex<Vec<ConsentView>>,
}

impl ViewSink for Window {
    fn emit(&self, view: &ConsentView) {
        self.all.lock().unwrap().push(view.clone());
        self.latest.send_replace(Some(view.clone()));
    }
}

/// The customer role without its window: the session state the commands
/// share, what the profile file next to the program says about a relay, and
/// what the window would have been shown.
pub(crate) struct Customer {
    shared: Arc<Mutex<ConsentSession>>,
    window: Arc<Window>,
    relay: relay::Lookup,
    dial: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for Customer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Customer")
            .field("phase", &self.view().status.phase)
            .finish_non_exhaustive()
    }
}

impl Customer {
    /// A customer copy past the permission step, resting, whose profile file
    /// says `relay`.
    pub(crate) fn new(relay: relay::Lookup) -> Self {
        Self {
            shared: Arc::new(Mutex::new(ConsentSession::idle())),
            window: Arc::new(Window {
                latest: watch::Sender::new(None),
                all: Mutex::new(Vec::new()),
            }),
            relay,
            dial: None,
        }
    }

    /// The customer copy `program`, past the permission step and resting: it
    /// reads the profile file next to its program, as the role does when its
    /// window starts.
    pub(crate) fn at(program: &std::path::Path) -> Self {
        Self::new(relay_next_to(Some(program)))
    }

    /// The activity feed of this session, oldest first.
    pub(crate) fn activity(&self) -> Vec<ActivityEntry> {
        lock(&self.shared).activity.entries()
    }

    /// What **Connect** does with what was typed into the code field.
    pub(crate) fn connect(&mut self, typed: &str) -> Result<ConsentView, CommandError> {
        self.connect_within(typed, None)
    }

    /// [`Customer::connect`], giving a technician on a relay `limit` to be
    /// heard instead of the ten seconds of the application.
    pub(crate) fn connect_within(
        &mut self,
        typed: &str,
        limit: Option<Duration>,
    ) -> Result<ConsentView, CommandError> {
        ensure_can_connect(&lock(&self.shared), true)?;
        let mut target = typed_target(typed, &self.relay)?;
        if let (Typed::Relay(join), Some(limit)) = (&mut target, limit) {
            join.limit = limit;
        }
        let session = prepare_dial(&self.shared, self.window.clone(), target);
        self.dial = Some(tokio::spawn(session));
        Ok(self.view())
    }

    /// The view as the commands see it now.
    pub(crate) fn view(&self) -> ConsentView {
        lock(&self.shared).view()
    }

    /// Every view the session rendered, in order.
    pub(crate) fn rendered(&self) -> Vec<ConsentView> {
        self.window.all.lock().unwrap().clone()
    }

    /// Waits until the window shows a view that `done` accepts, and answers
    /// it.
    pub(crate) async fn wait_for(
        &self,
        what: &str,
        done: impl Fn(&ConsentView) -> bool,
    ) -> ConsentView {
        let mut views = self.window.latest.subscribe();
        let found = tokio::time::timeout(
            STEP,
            views.wait_for(|view| view.as_ref().is_some_and(&done)),
        )
        .await;
        match found {
            Ok(Ok(view)) => view.clone().expect("a view"),
            _ => panic!(
                "the customer's window never showed: {what}; it shows {:?}",
                self.view().status.phase
            ),
        }
    }

    /// **Allow full control**.
    pub(crate) fn allow(&self) {
        grant_now(&mut lock(&self.shared)).expect("grant consent");
    }

    /// **Stop** and `Escape`.
    pub(crate) fn stop(&self) -> Result<(), CommandError> {
        stop_now(&mut lock(&self.shared))
    }

    /// Waits until this dial's task has ended, its teardown included.
    pub(crate) async fn settled(&mut self) {
        if let Some(task) = self.dial.take() {
            tokio::time::timeout(STEP, task)
                .await
                .expect("the dial's task ends")
                .expect("the dial's task does not panic");
        }
    }
}
