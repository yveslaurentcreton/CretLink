//! The task of one technician session (ST-M002-009, ST-M007-013).
//!
//! A session starts in one of two ways, and from then on it is the same
//! session:
//!
//! - **On a network** ([`Wait::Network`]): a listener on a private address of
//!   this computer, which the customer's copy finds or reaches with the
//!   connection code.
//! - **Through a relay** ([`Wait::Relay`]): this copy reaches out to the relay
//!   of its profile and keeps the rendezvous of a relay code open there until
//!   the customer's copy joins it ([`WaitingDial`], ST-M007-012). No listener
//!   is opened.
//!
//! Either way the task stands up the audit backbone, obtains the
//! [`HostHandle`] and hands it to the [`pump`], which folds the real session
//! into the shared state.
//!
//! A relay is addressing only (ADR-0016): it lets two connections find each
//! other and passes the end-to-end encrypted session on. Nothing here treats
//! it as more: the handle of a relayed session is driven by the same pump, so
//! pairing, the comparison of the approval code, the customer's consent, the
//! stop and the cleanup are what they are on one network (REQ-F-022).
//!
//! # Waiting on a relay
//!
//! A handle exists only once the customer is heard, so the wait runs here, in
//! the session's own task, beside the technician's intents: **Cancel** drops
//! the waiting dial, which closes the connection to the relay, and the relay
//! code then leads to no session. Only "nobody has joined yet" is waited out;
//! a relay that cannot be reached, is not trusted or refuses ends the wait at
//! once, in words with a next step ([`crate::relay::failure_words`]). Nothing
//! is tried again by itself (REQ-N-008, REQ-N-015).

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cretlink_console::{
    AUDIT_CHANNEL_CAPACITY, ChannelAudit, ConsoleState, RelayRoute, SessionPhase, prune_audit_dir,
    relay_route_entry, report_prune, spawn_audit_service, technician_display_name,
};
use cretlink_discovery::{Advertiser, ConnectionCode, RelayCode};
use cretlink_protocol::PROTOCOL_V1;
use cretlink_session::{
    ConsoleHints, DEFAULT_PARK_LIMIT, DEFAULT_REDIAL_PAUSE, Dial, HostHandle, RelayFailure,
    RelayLeg, RendezvousTicket, SessionError, SessionHost, WaitingDial,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::UnboundedReceiver;

use super::bridge::BridgeSlot;
use super::history::OpenLog;
use super::session_driver::{
    ActivitySlot, Intent, Place, ServiceSlot, StateSink, Turn, lock, pump,
};
use super::{AUDIT_SEAL_TIMEOUT, AuditLent, advertised_instance};
use crate::relay::{Audience, Relay, Source, failure_words};

/// How the customer reaches this console.
pub(super) enum Wait {
    /// A listener on this private address of this computer.
    Network(IpAddr),
    /// A rendezvous on the relay of this copy.
    Relay(RelayWait),
}

/// How patient a wait on a relay is.
#[derive(Debug, Clone, Copy)]
pub(super) struct Patience {
    /// The pause between two dials after a rendezvous that nobody joined.
    pub(super) pause: Duration,
    /// How long one parked connection is trusted before it is dialled again.
    pub(super) park_limit: Duration,
}

/// As patient as [`WaitingDial`] is by default.
impl Default for Patience {
    fn default() -> Self {
        Self {
            pause: DEFAULT_REDIAL_PAUSE,
            park_limit: DEFAULT_PARK_LIMIT,
        }
    }
}

/// A wait on a relay: the relay of this copy's profile, the code of this one
/// session, and how patient the waiting dial is.
pub(super) struct RelayWait {
    relay: Relay,
    code: RelayCode,
    patience: Patience,
}

impl RelayWait {
    /// A wait on `relay` with the code `code`.
    pub(super) fn new(relay: Relay, code: RelayCode, patience: Patience) -> Self {
        Self {
            relay,
            code,
            patience,
        }
    }

    /// The relay code as the technician reads it aloud.
    pub(super) fn code(&self) -> String {
        self.code.encode()
    }

    /// The relay as both people are shown it and as the audit records it:
    /// its name, where this copy connects, and what its certificate is
    /// checked against.
    pub(super) fn route(&self) -> RelayRoute {
        self.relay.route()
    }
}

/// The key and the code are not in it.
impl std::fmt::Debug for RelayWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayWait")
            .field("relay", &self.relay.host())
            .finish_non_exhaustive()
    }
}

/// What one session's task holds: the shared state and its sink, the
/// technician's intents, the slots the commands read, and where the task
/// writes outside this process.
pub(super) struct Run {
    pub(super) shared: Arc<Mutex<ConsoleState>>,
    /// This wait's claim on the shared state: what the task writes, it
    /// writes only while no newer wait began ([`Turn`]).
    pub(super) turn: Turn,
    pub(super) sink: Arc<dyn StateSink>,
    pub(super) intents: UnboundedReceiver<Intent>,
    pub(super) service: ServiceSlot,
    pub(super) activity: ActivitySlot,
    pub(super) bridge: BridgeSlot,
    pub(super) audit: Arc<Mutex<Option<ChannelAudit>>>,
    pub(super) audit_log: OpenLog,
    pub(super) place: Place,
}

/// A handle to hand to the pump, and what has to stay alive beside it.
struct Waited {
    handle: HostHandle,
    /// The mDNS announcement of a session on a network; it ends with the
    /// session.
    _advertiser: Option<Advertiser>,
}

impl Run {
    /// Waits for the customer as `wait` says and drives the session that
    /// follows until it has ended and its audit log is sealed.
    pub(super) async fn drive(self, wait: Wait) {
        let Self {
            shared,
            turn,
            sink,
            mut intents,
            service,
            activity,
            bridge,
            audit: audit_slot,
            audit_log,
            place,
        } = self;

        // Stand up the audit backbone the console CLI runs (ST-M003-006): one
        // bounded channel feeds one drain that owns the durable, hash-chained
        // per-session log. Retention pruning runs first so the log directory
        // stays bounded. The GUI is the real host, so it owns this spine
        // (ST-M004-007).
        report_prune(prune_audit_dir(
            &place.audit.dir,
            &place.audit.retention,
            None,
        ));
        let (audit, audit_rx) = ChannelAudit::bounded(AUDIT_CHANNEL_CAPACITY);
        // Lend the terminal commands this session's audit sink; the guard
        // takes it back on every exit path so the log can still be sealed.
        // Only while the state is this wait's: a task that was outlived
        // neither lends its sink over the one of the newer wait nor takes
        // that one back.
        turn.while_current(&shared, || {
            *audit_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(audit.clone());
        });
        let audit_lent = AuditLent {
            slot: audit_slot,
            shared: Arc::clone(&shared),
            turn: turn.clone(),
        };
        // The history leaves this log out until it is sealed (ST-M009-012).
        let (start_tx, start_rx) = audit_log.track();
        let audit_task = spawn_audit_service(place.audit.clone(), audit_rx, start_rx);

        let waited = match wait {
            Wait::Network(bind_ip) => listen(&shared, &turn, sink.as_ref(), bind_ip, &audit).await,
            Wait::Relay(wait) => {
                wait_on_relay(&shared, &turn, &sink, &mut intents, wait, &audit).await
            }
        };
        // Without a handle the wait ended honestly: it failed in words, or
        // the technician cancelled it. No session log was opened.
        let Some(waited) = waited else {
            return;
        };

        // Fold the live event stream into the shared state until it ends,
        // dispatching compare/stop intents from the UI to the handle. At full
        // control the pump hands the handle to the hosted service so the MCP
        // bridge and the interactive terminal both route through this one
        // consented session (ST-M004-007).
        pump(
            waited.handle,
            shared,
            turn,
            sink,
            intents,
            audit.clone(),
            place,
            start_tx,
            service,
            activity,
            bridge,
        )
        .await;

        // Seal the durable log: release the terminal commands' sender, then
        // drop the last one so the drain observes channel close, then wait a
        // bounded moment for the seal record to land. A timeout is recorded
        // honestly: the log verifies as unsealed rather than the exit
        // blocking forever (REQ-N cleanup honesty).
        drop(audit_lent);
        drop(audit);
        if tokio::time::timeout(AUDIT_SEAL_TIMEOUT, audit_task)
            .await
            .is_err()
        {
            eprintln!(
                "note: the audit log could not be sealed before exit; it will verify as unsealed"
            );
        }
        audit_log.closed();
    }
}

/// What this computer says about itself in the pairing handshake.
fn hints() -> ConsoleHints {
    ConsoleHints {
        label: "This computer (technician)".to_string(),
        // The account's full name, else its user name, sent in the pairing
        // handshake as what this computer says (ST-M009-011).
        display_name: technician_display_name(),
    }
}

/// Starts a listener on `bind_ip`, announces it and enters the listening
/// phase with its connection code (ST-M002-009, ST-M002-012).
async fn listen(
    shared: &Mutex<ConsoleState>,
    turn: &Turn,
    sink: &dyn StateSink,
    bind_ip: IpAddr,
    audit: &ChannelAudit,
) -> Option<Waited> {
    // The failure of a wait that could not begin, on the state of this wait
    // only. The state names no relay then: the wait before it may have gone
    // through one, and this one touched none ([`Waits::begin`](
    // super::session_driver::Waits::begin)).
    let fail = |message: String| {
        if let Some(mut console) = turn.lock(shared) {
            console.fail("listen", message);
            sink.emit(&console);
        }
    };
    // 1. Bind the listener on a private interface, recording lifecycle
    //    outcomes (pairing, consent, cleanup) to the session's log via
    //    `host_with_audit` (REQ-F-017). A bind failure is an honest terminal
    //    state, never a silent no-op.
    let handle = match SessionHost::host_with_audit(
        SocketAddr::new(bind_ip, 0),
        hints(),
        PROTOCOL_V1,
        Vec::new(),
        Arc::new(audit.clone()),
    )
    .await
    {
        Ok(handle) => handle,
        Err(error) => {
            fail(format!("could not start the listener: {error}"));
            return None;
        }
    };
    let Some(local_addr) = handle.local_addr() else {
        fail("the listener reported no local address".to_string());
        return None;
    };

    // 2. Best-effort mDNS advertising. Discovery is convenience only and
    //    grants no authority; the manual code below always works.
    //    The advertised name is the technician's display name (ST-M009-011),
    //    an untrusted hint on the customer side; without one, the product
    //    name.
    let advertised_name = technician_display_name().map_or_else(
        || "CretLink Console".to_string(),
        |name| name.as_str().to_string(),
    );
    //    The instance name is this computer's host name, so the customer's
    //    list shows the name and host name the waiting screen tells them to
    //    pick (ST-M009-009); equally untrusted on their side.
    let instance = advertised_instance(&lock(shared).technician.hostname);
    //    Only the address the listener is bound to is announced, on its own
    //    adapter (ST-M009-016).
    let advertiser =
        Advertiser::advertise(&instance, local_addr, PROTOCOL_V1, &advertised_name).ok();

    // 3. Enter the listening phase carrying the connection code the customer
    //    types in and the private listen address to read out. No target is
    //    fabricated here: the customer materialises only when a real
    //    `Connected` event arrives over the pump (ST-M002-012), so the
    //    console never shows a target it has not actually heard from.
    let code = match local_addr {
        SocketAddr::V4(v4) => ConnectionCode::encode(v4).to_string(),
        SocketAddr::V6(addr) => addr.to_string(),
    };
    {
        // A newer wait began while this listener was bound: the listener
        // goes with this task, and the screen is left to the newer wait.
        let mut console = turn.lock(shared)?;
        let technician = console.technician.clone();
        *console = ConsoleState::new(technician);
        if let Err(error) = console.listen(code, local_addr.to_string()) {
            console.fail("listen", error.to_string());
            sink.emit(&console);
            return None;
        }
        sink.emit(&console);
    }
    Some(Waited {
        handle,
        _advertiser: advertiser,
    })
}

/// Whether the console still waits on the relay code `code`: the wait was
/// neither cancelled nor ended in another way. Whether the state is still
/// this wait's own is the question of its [`Turn`]; the code is asked beside
/// it, because every wait has a code of its own.
fn waits_on(console: &ConsoleState, code: &str) -> bool {
    console.relay.is_some()
        && matches!(&console.phase, SessionPhase::Listening { code: shown, .. } if shown == code)
}

/// A relay dial that says when the relay took the connection in.
///
/// It wraps the dial that [`WaitingDial`] repeats, so it sees every
/// connection the relay admits; the first one is when the relay holds the
/// rendezvous that the code leads to.
struct Admitted<D, F> {
    inner: D,
    admitted: F,
}

impl<D, S, F> Dial for Admitted<D, F>
where
    D: Dial<Stream = RelayLeg<S>> + Sync,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn() + Sync,
{
    type Stream = RelayLeg<S>;

    async fn dial(&self) -> Result<Self::Stream, SessionError> {
        let leg = self.inner.dial().await?;
        (self.admitted)();
        Ok(leg)
    }
}

/// Where the technician changes the relay of this copy, as a sentence.
fn where_it_is_set(relay: &Relay) -> String {
    match relay.source() {
        Source::Settings(_) => "You can change and test the relay in Settings, under Relay.".into(),
        Source::File(path) => format!(
            "This copy takes its relay from the file {}; Settings, Relay shows it and can test it.",
            path.display()
        ),
    }
}

/// Ends the wait on `relay` as a failed session in plain words, with a next
/// step (REQ-N-015). The stage names the failure: `relay_unreachable`,
/// `relay_untrusted`, `relay_key_refused`, and so on. Only the wait that is
/// still on screen is ended.
fn fail_relay(
    shared: &Mutex<ConsoleState>,
    turn: &Turn,
    sink: &dyn StateSink,
    code: &str,
    relay: &Relay,
    failure: &RelayFailure,
) {
    let words = failure_words(failure, &relay.view(), Audience::Technician);
    // What is put right in the relay profile gets the place to do that.
    let in_the_profile = matches!(
        words.kind,
        "name_not_found"
            | "unreachable"
            | "untrusted"
            | "no_answer"
            | "key_required"
            | "key_refused"
            | "settings"
    );
    let next = if in_the_profile {
        format!("{} {}", words.next, where_it_is_set(relay))
    } else {
        words.next
    };
    if let Some(mut console) = turn.lock(shared)
        && waits_on(&console, code)
    {
        console.fail_with_next(format!("relay_{}", words.kind), words.message, next);
        sink.emit(&console);
    }
}

/// Keeps the rendezvous of the wait's code open on its relay until the
/// customer's copy joins it, and answers the handle of the session that then
/// begins. `None` when the wait failed (said in words on the console state)
/// or was cancelled.
async fn wait_on_relay(
    shared: &Arc<Mutex<ConsoleState>>,
    turn: &Turn,
    sink: &Arc<dyn StateSink>,
    intents: &mut UnboundedReceiver<Intent>,
    wait: RelayWait,
    audit: &ChannelAudit,
) -> Option<Waited> {
    let RelayWait {
        relay,
        code,
        patience,
    } = wait;
    let shown = code.encode();
    let fail =
        |failure: &RelayFailure| fail_relay(shared, turn, sink.as_ref(), &shown, &relay, failure);

    // The TLS settings first: a profile without usable authorities is said
    // before anything is dialled.
    let connector = match relay.connector().await {
        Ok(connector) => connector,
        Err(failure) => {
            fail(&failure);
            return None;
        }
    };
    // Both copies derive the ticket from the code that is read aloud and
    // from the relay's host name (ST-M007-012). The code itself never
    // reaches the relay.
    let Ok(ticket) = RendezvousTicket::derive(code.seed(), relay.host()) else {
        fail(&RelayFailure::Settings(
            "the name of the relay cannot be used for a rendezvous".to_owned(),
        ));
        return None;
    };

    let admitted = {
        let shared = Arc::clone(shared);
        let turn = turn.clone();
        let sink = Arc::clone(sink);
        let shown = shown.clone();
        move || {
            if let Some(mut console) = turn.lock(&shared)
                && waits_on(&console, &shown)
                && console.relay_admitted()
            {
                sink.emit(&console);
            }
        }
    };
    let waiting = WaitingDial::with_timing(
        Admitted {
            inner: relay.dial(connector, ticket),
            admitted,
        },
        patience.pause,
        patience.park_limit,
    );

    // Wait for as long as the technician does. Cancel is looked at first, so
    // that a customer who arrives at the same moment does not get a session
    // the technician just called off.
    let stream = {
        let dialing = waiting.dial();
        tokio::pin!(dialing);
        loop {
            tokio::select! {
                biased;
                intent = intents.recv() => match intent {
                    // Cancelled, or every command handle is gone: dropping
                    // the dial closes the connection to the relay, and the
                    // code leads to no session any more.
                    None | Some(Intent::Stop(_)) => return None,
                    // There is no approval code to compare yet.
                    Some(Intent::Confirm | Intent::Mismatch) => {}
                },
                dialled = &mut dialing => break dialled,
            }
        }
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(SessionError::Relay(failure)) => {
            fail(&failure);
            return None;
        }
        Err(other) => {
            fail(&RelayFailure::Settings(other.to_string()));
            return None;
        }
    };

    // The customer's copy is heard. A wait that was called off in this very
    // moment, or that a newer wait took the screen from, gets no session:
    // the connection is dropped.
    if !turn
        .lock(shared)
        .is_some_and(|console| waits_on(&console, &shown))
    {
        return None;
    }
    // The audit says first which way the session goes: through this relay,
    // by its name, where this copy connected and what vouched for the name.
    // Pairing, the approval code and consent follow as their own records,
    // exactly as on one network (ADR-0016).
    audit.record_entry(relay_route_entry(&relay.route()));
    let handle = SessionHost::host_over_stream(
        stream,
        hints(),
        PROTOCOL_V1,
        Vec::new(),
        Arc::new(audit.clone()),
    );
    Some(Waited {
        handle,
        _advertiser: None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use cretlink_cli::relay_profile::RelayProfile;
    use cretlink_console::TechnicianHost;
    use cretlink_relay::RelayRefusal;

    use super::super::session_driver::Waits;
    use super::*;

    const KEY: &str = "5f2c9d0e7a1b4c3d8e9f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5";
    const CODE: &str = "53ST-A71D-JKKV-0VGH";

    #[derive(Debug, Default)]
    struct Counting(AtomicUsize);

    impl StateSink for Counting {
        fn emit(&self, _state: &ConsoleState) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn relay(source: Source) -> Relay {
        let profile =
            RelayProfile::from_fields("relay.example.org:443", Some(KEY), None, None).unwrap();
        Relay::with_ca(source, profile, None).unwrap()
    }

    fn from_settings() -> Relay {
        relay(Source::Settings(PathBuf::from("settings.toml")))
    }

    fn console() -> ConsoleState {
        ConsoleState::new(TechnicianHost {
            label: "This computer (technician)".to_string(),
            hostname: "TECH-LAPTOP".to_string(),
            os: "Windows 11".to_string(),
            display_name: None,
        })
    }

    /// A console that waits on the relay code `code`, with the turn of the
    /// task of that wait.
    fn waiting(code: &str) -> (Mutex<ConsoleState>, Turn) {
        let mut state = console();
        let turn = Waits::default().begin(&mut state);
        state
            .listen_on_relay(code, from_settings().route())
            .unwrap();
        (Mutex::new(state), turn)
    }

    fn failure(shared: &Mutex<ConsoleState>) -> (String, String, String) {
        match &lock(shared).phase {
            SessionPhase::Failed {
                stage,
                message,
                next,
            } => (
                stage.clone(),
                message.clone(),
                next.clone().unwrap_or_default(),
            ),
            other => panic!("the wait did not fail: {other:?}"),
        }
    }

    #[test]
    fn a_wait_is_known_by_its_code() {
        let mut state = console();
        assert!(!waits_on(&state, CODE));
        // Waiting on a network is no wait on a relay, whatever the code.
        state.listen(CODE, "10.0.0.42:7000").unwrap();
        assert!(!waits_on(&state, CODE));
        let (shared, _turn) = waiting(CODE);
        assert!(waits_on(&lock(&shared), CODE));
        assert!(!waits_on(&lock(&shared), "6RRR-2H8F-0000-0000"));
        // Once the wait is over, nothing of it is on screen any more.
        lock(&shared)
            .stop(cretlink_console::StopReason::TechnicianRequested)
            .unwrap();
        assert!(!waits_on(&lock(&shared), CODE));
    }

    #[test]
    fn a_relay_that_cannot_be_used_ends_the_wait_in_words_and_says_where_it_is_set() {
        let sink = Counting::default();

        // Put right in the profile: the next step says where that is.
        let (shared, turn) = waiting(CODE);
        let key = RelayFailure::Refused(RelayRefusal::CredentialRequired);
        fail_relay(&shared, &turn, &sink, CODE, &from_settings(), &key);
        let (stage, message, next) = failure(&shared);
        assert_eq!(stage, "relay_key_required");
        assert_eq!(
            message,
            "The relay relay.example.org asks for an admission key, and this copy has none."
        );
        assert_eq!(
            next,
            "Enter the key that the relay's operator gave this copy. You can change and test \
             the relay in Settings, under Relay."
        );
        assert_eq!(sink.0.load(Ordering::Relaxed), 1);
        // The relay stays named, and nobody has any authority.
        assert_eq!(
            lock(&shared).relay.as_ref().map(|relay| relay.host.clone()),
            Some("relay.example.org".to_owned())
        );
        assert!(!lock(&shared).can_issue_actions());

        // A copy that takes its relay from the file next to the program.
        let (shared, turn) = waiting(CODE);
        let file = relay(Source::File(PathBuf::from("cretlink.toml")));
        let gone = RelayFailure::Unreachable(std::io::Error::other("refused"));
        fail_relay(&shared, &turn, &sink, CODE, &file, &gone);
        let (stage, _, next) = failure(&shared);
        assert_eq!(stage, "relay_unreachable");
        assert!(
            next.ends_with(
                "This copy takes its relay from the file cretlink.toml; Settings, Relay shows it \
                 and can test it."
            ),
            "{next}"
        );

        // Nothing to change in the profile: the next step stands alone.
        let (shared, turn) = waiting(CODE);
        let busy = RelayFailure::Refused(RelayRefusal::RateLimited);
        fail_relay(&shared, &turn, &sink, CODE, &from_settings(), &busy);
        assert_eq!(
            failure(&shared),
            (
                "relay_rate_limited".to_owned(),
                "The relay relay.example.org had too many attempts in a short time.".to_owned(),
                "Wait a minute, then try again.".to_owned(),
            )
        );

        // No word of any of them holds the key.
        for text in [format!("{:?}", lock(&shared)), next, message] {
            assert!(!text.contains(KEY), "{text}");
        }
    }

    #[test]
    fn a_wait_that_is_over_is_not_failed_afterwards() {
        let sink = Counting::default();
        let gone = RelayFailure::Unreachable(std::io::Error::other("refused"));

        // The technician cancelled: the wait stays cancelled.
        let (shared, turn) = waiting(CODE);
        lock(&shared)
            .stop(cretlink_console::StopReason::TechnicianRequested)
            .unwrap();
        fail_relay(&shared, &turn, &sink, CODE, &from_settings(), &gone);
        assert!(matches!(lock(&shared).phase, SessionPhase::Stopped { .. }));

        // Another wait took the screen meanwhile: it is left alone, whatever
        // code it waits on. The turn of the earlier wait is over.
        for code in ["6RRR-2H8F-0000-0000", CODE] {
            let waits = Waits::default();
            let mut state = console();
            let earlier = waits.begin(&mut state);
            let later = waits.begin(&mut state);
            state
                .listen_on_relay(code, from_settings().route())
                .unwrap();
            let shared = Mutex::new(state);
            fail_relay(&shared, &earlier, &sink, CODE, &from_settings(), &gone);
            assert!(matches!(
                lock(&shared).phase,
                SessionPhase::Listening { .. }
            ));
            assert!(earlier.lock(&shared).is_none());
            assert!(later.lock(&shared).is_some());
        }
        assert_eq!(sink.0.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_new_wait_takes_the_state_from_the_task_of_the_wait_before() {
        let waits = Waits::default();
        let mut state = console();
        // A session through a relay ended; its state still names the relay.
        let first = waits.begin(&mut state);
        state
            .listen_on_relay(CODE, from_settings().route())
            .unwrap();
        state
            .stop(cretlink_console::StopReason::TechnicianRequested)
            .unwrap();
        assert!(state.relay.is_some());
        // The next wait begins: the state is its own from now on, and names
        // no relay that this wait never touched.
        let second = waits.begin(&mut state);
        assert_eq!(state.relay, None);
        let shared = Mutex::new(state);
        assert!(first.lock(&shared).is_none());
        assert!(second.lock(&shared).is_some());
        // What the earlier task would still write to a slot is not written.
        let mut written = Vec::new();
        first.while_current(&shared, || written.push("earlier"));
        second.while_current(&shared, || written.push("later"));
        assert_eq!(written, ["later"]);
        // A turn is this wait's alone, however it is passed around.
        assert!(second.clone().lock(&shared).is_some());
        assert!(first.clone().lock(&shared).is_none());
    }

    #[test]
    fn a_wait_prints_neither_its_code_nor_the_key() {
        let wait = RelayWait::new(
            from_settings(),
            RelayCode::parse(CODE).unwrap(),
            Patience::default(),
        );
        assert_eq!(wait.code(), CODE);
        assert_eq!(
            wait.route(),
            RelayRoute::new(
                "relay.example.org",
                "relay.example.org:443",
                cretlink_console::RelayTrust::SystemAuthorities
            )
        );
        let printed = format!("{wait:?}");
        assert!(printed.contains("relay.example.org"), "{printed}");
        assert!(
            !printed.contains("53ST") && !printed.contains(KEY),
            "{printed}"
        );
    }
}
