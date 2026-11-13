//! What the tests of a technician session share (ST-M007-013): the role's own
//! state and session task, a screen that keeps every state it was given, and
//! an audit folder and bridge endpoint of the test's own, so that no test
//! touches the audit trail or the MCP bridge of whoever runs it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cretlink_console::{
    AuditServiceConfig, ConsoleSessionService, ConsoleState, Integrity, SessionPhase,
    SessionSummary, list_sessions,
};
use cretlink_discovery::RelayCode;
use cretlink_mcp::IpcEndpoint;
use tokio::sync::watch;

use super::bridge::BridgeStatus;
use super::relay_settings::relay_of_copy;
use super::session_driver::{IntentSender, Place, StateSink, now_unix_millis};
use super::session_task::{Patience, RelayWait};
use super::{
    AppState, CommandError, begin_network_wait, begin_relay_wait, confirm_now, relay_can_be_chosen,
    stop_now, use_relay_now,
};
use crate::relay::testing::Folder;
use crate::relay::{Relay, ThisCopy};

/// How long one step of a session inside this process may take.
pub(crate) const STEP: Duration = Duration::from_secs(20);

/// How long the seal of an audit log may take on a disk that is busy.
const SEAL: Duration = Duration::from_secs(90);

static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(0);

/// A screen that keeps every console state the session's task rendered.
#[derive(Debug)]
struct Screen {
    latest: watch::Sender<Option<ConsoleState>>,
    all: Mutex<Vec<ConsoleState>>,
    /// How often the status of the MCP bridge was rendered. The session's
    /// task renders it first when the session's service is there.
    bridge: watch::Sender<u64>,
}

impl StateSink for Screen {
    fn emit(&self, state: &ConsoleState) {
        self.all.lock().unwrap().push(state.clone());
        self.latest.send_replace(Some(state.clone()));
    }

    fn emit_bridge(&self, _status: &BridgeStatus) {
        self.bridge.send_modify(|count| *count += 1);
    }
}

/// The technician role without its window: the state the commands share,
/// and what the window would have been shown.
pub(crate) struct Technician {
    pub(crate) state: AppState,
    screen: Arc<Screen>,
    place: Place,
    folder: Folder,
}

impl std::fmt::Debug for Technician {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Technician")
            .field("phase", &self.console().phase)
            .finish_non_exhaustive()
    }
}

impl Technician {
    /// A technician copy that is ready and idle, with an audit folder and a
    /// bridge endpoint of its own.
    pub(crate) fn new(test: &str) -> Self {
        let folder = Folder::new(&format!("technician-{test}"));
        let place = Place {
            audit: AuditServiceConfig {
                dir: folder.path("audit"),
                ..AuditServiceConfig::default()
            },
            bridge: IpcEndpoint::named(format!(
                "cretlink-app-test-{}-{}",
                std::process::id(),
                NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed)
            )),
        };
        Self {
            state: AppState::live_idle(),
            screen: Arc::new(Screen {
                latest: watch::Sender::new(None),
                all: Mutex::new(Vec::new()),
                bridge: watch::Sender::new(0),
            }),
            place,
            folder,
        }
    }

    /// The folder of this technician, for a profile file next to a program.
    pub(crate) fn folder(&self) -> &Folder {
        &self.folder
    }

    /// The relay a session of the copy `program` goes through, as **Use
    /// relay** reads it: a copy that runs once and takes its relay from the
    /// profile file next to the program.
    pub(crate) fn relay_of(&self, program: &std::path::Path) -> Result<Relay, CommandError> {
        relay_of_copy(&ThisCopy {
            exe: program.to_path_buf(),
            installed: None,
        })
    }

    /// Whether **Use relay** is offered in the state the console is in.
    pub(crate) fn relay_can_be_chosen(&self) -> bool {
        relay_can_be_chosen(&self.console()).is_ok()
    }

    /// Whether a session is under full control: only then is there a service
    /// that a command, a transfer or the bridge could go through.
    pub(crate) fn has_control(&self) -> bool {
        self.state.service().is_some()
    }

    /// **Start a session**: wait for the customer on the loopback address of
    /// this computer, as the command does it, and answer the waiting state
    /// with its connection code.
    pub(crate) async fn start_on_network(&self) -> ConsoleState {
        assert!(
            matches!(
                self.console().phase,
                SessionPhase::Idle | SessionPhase::Stopped { .. } | SessionPhase::Failed { .. }
            ),
            "a session is in progress: {:?}",
            self.console().phase
        );
        self.listen_on(IpAddr::V4(Ipv4Addr::LOCALHOST));
        self.wait_for("the connection code", |state| {
            matches!(state.phase, SessionPhase::Listening { .. }) && state.relay.is_none()
        })
        .await
    }

    /// **Use relay**, as the command does it ([`use_relay_now`]): from a wait
    /// on a network to a wait on the relay of the copy `program`, a copy
    /// that runs once and takes its relay from the profile file next to it.
    /// Answers what the window gets back.
    pub(crate) async fn use_relay(
        &self,
        program: &std::path::Path,
    ) -> Result<ConsoleState, CommandError> {
        let relay = self.relay_of(program);
        self.use_relay_reading(async { relay }).await
    }

    /// [`Technician::use_relay`], with `reading` in the place of reading the
    /// profile: what happens while the profile is read happens inside it.
    pub(crate) async fn use_relay_reading(
        &self,
        reading: impl Future<Output = Result<Relay, CommandError>>,
    ) -> Result<ConsoleState, CommandError> {
        use_relay_now(
            &self.state,
            self.screen.clone(),
            self.place.clone(),
            reading,
            Patience::default(),
        )
        .await
    }

    /// [`Technician::use_relay`] with another pause between two dials of the
    /// waiting connection, for a test that lets a rendezvous lapse.
    pub(crate) async fn use_relay_redialling_after(
        &self,
        program: &std::path::Path,
        pause: Duration,
    ) -> Result<ConsoleState, CommandError> {
        let relay = self.relay_of(program);
        use_relay_now(
            &self.state,
            self.screen.clone(),
            self.place.clone(),
            async { relay },
            Patience {
                pause,
                ..Patience::default()
            },
        )
        .await
    }

    /// Begins a wait on `relay` beside whatever wait there is, without
    /// stopping that one first: what is left when the task of an earlier
    /// wait does not let go in time and the command goes on without it.
    pub(crate) fn begin_relay_wait_beside(&self, relay: Relay) -> ConsoleState {
        let wait = RelayWait::new(relay, RelayCode::generate().unwrap(), Patience::default());
        let task = begin_relay_wait(&self.state, self.screen.clone(), self.place.clone(), wait);
        // The handle of the earlier task is let go of; the task itself is
        // not stopped by that.
        *self.state.pump.lock().unwrap() = Some(task);
        self.console()
    }

    /// What the commands send the session's task through: **Codes match**,
    /// **Stop**. It is closed once that task has ended.
    pub(crate) fn intents(&self) -> IntentSender {
        self.state
            .live
            .lock()
            .unwrap()
            .clone()
            .expect("a session's task was started")
    }

    /// What **Start a session** does: wait for the customer on `ip`.
    pub(crate) fn listen_on(&self, ip: IpAddr) {
        let task = begin_network_wait(&self.state, self.screen.clone(), self.place.clone(), ip);
        *self.state.pump.lock().unwrap() = Some(task);
    }

    /// The console state as the commands see it now.
    pub(crate) fn console(&self) -> ConsoleState {
        self.state.guard().clone()
    }

    /// Every state the session's task rendered, in order.
    pub(crate) fn rendered(&self) -> Vec<ConsoleState> {
        self.screen.all.lock().unwrap().clone()
    }

    /// Waits until the screen shows a state that `done` accepts, and answers
    /// it.
    pub(crate) async fn wait_for(
        &self,
        what: &str,
        done: impl Fn(&ConsoleState) -> bool,
    ) -> ConsoleState {
        let mut states = self.screen.latest.subscribe();
        let found = tokio::time::timeout(
            STEP,
            states.wait_for(|state| state.as_ref().is_some_and(&done)),
        )
        .await;
        match found {
            Ok(Ok(state)) => state.clone().expect("a state"),
            _ => panic!(
                "the technician's screen never showed: {what}; it shows {:?}",
                self.console().phase
            ),
        }
    }

    /// **Codes match**.
    pub(crate) fn confirm(&self) -> ConsoleState {
        confirm_now(&self.state).expect("confirm the approval code")
    }

    /// **Cancel**, **Stop** and `Escape`.
    pub(crate) fn stop(&self) -> Result<ConsoleState, CommandError> {
        stop_now(&self.state)
    }

    /// The session's service, which exists only under full control: what a
    /// command, a transfer and the bridge go through. The session's task
    /// publishes it a moment after the screen shows full control, and renders
    /// the bridge's status once it has.
    pub(crate) async fn service(&self) -> ConsoleSessionService {
        let mut rendered = self.screen.bridge.subscribe();
        tokio::time::timeout(STEP, rendered.wait_for(|_| self.state.service().is_some()))
            .await
            .expect("the session's service is published under full control")
            .expect("the screen is there");
        self.state.service().expect("a session under full control")
    }

    /// Waits until the session's task has ended: the wait is over, or the
    /// session has ended and its audit log is sealed.
    pub(crate) async fn finished(&self) {
        let task = self.state.pump.lock().unwrap().take();
        if let Some(task) = task {
            // The task's own teardown is bounded: the bridge's grace, then
            // the wait for the seal of the audit log.
            tokio::time::timeout(SEAL, task)
                .await
                .expect("the session's task ends")
                .expect("the session's task does not panic");
        }
    }

    /// The one session of this technician's history, once its audit log is
    /// sealed. The session's task waits a bounded moment for the seal and
    /// then goes on; on a slow disk the seal can land after that, so the
    /// history is read until it is there.
    pub(crate) async fn sealed_session(&self) -> SessionSummary {
        let sealed = async {
            loop {
                let sessions = self.sessions();
                if let [only] = sessions.as_slice()
                    && only.integrity == Integrity::Verified
                {
                    return only.clone();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        match tokio::time::timeout(SEAL, sealed).await {
            Ok(summary) => summary,
            Err(_) => panic!(
                "the audit log was not sealed; the history holds {:?}",
                self.sessions()
            ),
        }
    }

    /// The session history of this technician's audit folder, newest first.
    pub(crate) fn sessions(&self) -> Vec<SessionSummary> {
        list_sessions(
            &self.place.audit.dir,
            &self.place.audit.retention,
            now_unix_millis(),
        )
    }

    /// Every file of the audit folder as text, to show what the trail holds
    /// and what it does not.
    pub(crate) fn audit_text(&self) -> String {
        let mut all = String::new();
        if let Ok(entries) = std::fs::read_dir(&self.place.audit.dir) {
            for entry in entries.flatten() {
                all.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
            }
        }
        all
    }
}
