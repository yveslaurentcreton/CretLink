//! In-memory fail-closed matrix for post-reboot redemption (ADR-0014 §3,
//! ST-M006-002).
//!
//! Each test arms a **bilateral** pair of vaults — an agent-side and a
//! console-side copy sharing one `K_rr`, ticket id, target, and expiry, exactly
//! as ADR-0014 §2 mints them — then drives one challenge/response exchange over
//! a pure in-memory [`ControlMessage`] duplex with per-side injectable clocks.
//! No hardware, no Noise handshake, no real reboot: the fresh transcript hash
//! `h'` is injected as the shared input both sides would derive from the fresh
//! post-reboot handshake (the live derivation, OS auto-start and reboot
//! survival are manual test MT-8 in `docs/manual-tests.md`).
//!
//! The matrix proves the security spine: a valid exchange proves continuity but
//! grants no control; every tampered, stale, foreign, or interrupted exchange
//! collapses to `NoConsentedSession`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use cretlink_protocol::{
    ArmedTicket, AtRestSealer, RebootResumeRefusal, RebootResumeVault, SealError, TargetBinding,
    TargetIdentity, TicketId, derive_reboot_resume_key,
};
use tokio::sync::mpsc::unbounded_channel;
use zeroize::Zeroizing;

use super::{
    AnswerOutcome, RedemptionClock, RedemptionOutcome, RedemptionRefusal, agent_answer_challenge,
    console_redeem,
};
use crate::reboot::RebootResumeTicketId;

/// The ticket both endpoints hold after a bilateral arm.
const TICKET: RebootResumeTicketId = [7u8; 16];
/// Expiry as Unix milliseconds; the clocks below sit before or after it.
const NOT_AFTER: i64 = 10_000;
/// A "now" comfortably inside the ticket's lifetime.
const BEFORE_EXPIRY: i64 = 5_000;
/// A "now" past expiry, used to drive the expiry edge from one side.
const AFTER_EXPIRY: i64 = 20_000;

/// A deterministic, environment-tagged [`AtRestSealer`] test double.
///
/// Sealing prefixes the environment tag; unsealing refuses any blob whose tag
/// differs, modelling DPAPI's per-user/per-machine binding — so a sealed record
/// "lifted" to a foreign environment (a different tag) fails to unseal exactly
/// as it would off-box.
#[derive(Debug, Clone)]
struct InMemorySealer {
    env: u8,
}

impl InMemorySealer {
    fn new(env: u8) -> Self {
        Self { env }
    }
}

impl AtRestSealer for InMemorySealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
        let mut out = Vec::with_capacity(plaintext.len() + 1);
        out.push(self.env);
        out.extend_from_slice(plaintext);
        Ok(out)
    }

    fn unseal(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError> {
        match sealed.split_first() {
            Some((&tag, rest)) if tag == self.env => Ok(Zeroizing::new(rest.to_vec())),
            _ => Err(SealError::Unrecoverable),
        }
    }
}

fn identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "CUSTOMER-PC".to_string(),
        os_version: "Windows 11 Pro".to_string(),
        arch: "x86_64".to_string(),
        elevated: true,
        simulated: false,
        agent_version: "0.0.0".to_string(),
    }
}

/// A different honest target, for the console-side target-mismatch case.
fn other_identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "SOMEONE-ELSE-PC".to_string(),
        os_version: "Windows 11 Pro".to_string(),
        arch: "x86_64".to_string(),
        elevated: true,
        simulated: false,
        agent_version: "0.0.0".to_string(),
    }
}

fn clock(now: i64) -> RedemptionClock {
    Arc::new(move || now)
}

/// Arms one bilateral pair: an agent-side and a console-side vault, each sealed
/// to its own environment tag but sharing the *same* `K_rr`, ticket id, target,
/// and expiry — the bilateral outcome ADR-0014 §2 mints. Returns both vaults,
/// each holding exactly the one armed ticket.
fn armed_pair(
    target: &TargetIdentity,
    agent_env: u8,
    console_env: u8,
) -> (
    RebootResumeVault<InMemorySealer>,
    RebootResumeVault<InMemorySealer>,
) {
    // ADR-0014 §2: both endpoints derive the *same* `K_rr` from the shared
    // handshake hash and the exchanged nonces, then each seals it under its own
    // at-rest sealer. Deriving once and arming both vaults with `&key` models
    // that bilateral outcome — identical K_rr, ticket id, target, and expiry,
    // sealed independently per environment.
    let h = [3u8; 32];
    let na = [9u8; 32];
    let nc = [4u8; 32];
    let key = derive_reboot_resume_key(&h, &na, &nc);

    let mut agent = RebootResumeVault::new(InMemorySealer::new(agent_env));
    let agent_ticket = agent
        .arm(TicketId(TICKET), &key, TargetBinding::of(target), NOT_AFTER)
        .expect("agent arm");

    let mut console = RebootResumeVault::new(InMemorySealer::new(console_env));
    let console_ticket = console
        .arm(TicketId(TICKET), &key, TargetBinding::of(target), NOT_AFTER)
        .expect("console arm");

    // Both endpoints minted the same ticket id from the same inputs.
    assert_eq!(agent_ticket.ticket_id, console_ticket.ticket_id);
    (agent, console)
}

/// Restores a vault from a single persisted ticket under a chosen environment —
/// used to model a sealed blob replayed in a foreign environment.
fn restore_one(ticket: ArmedTicket, env: u8) -> RebootResumeVault<InMemorySealer> {
    RebootResumeVault::restore(InMemorySealer::new(env), [ticket])
}

/// A normal exchange with matched `h'`, target, and an in-lifetime clock proves
/// continuity — yet grants no control — and consumes both endpoints' tickets.
#[tokio::test]
async fn normal_exchange_proves_continuity_without_control_and_consumes() {
    let target = identity();
    let (mut agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    // agent → console and console → agent halves of the in-memory duplex.
    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();

    let agent_now = clock(BEFORE_EXPIRY);
    let console_now = clock(BEFORE_EXPIRY);

    let agent_fut = agent_answer_challenge(&mut agent, a_tx, a_rx, &TICKET, &h_prime, &agent_now);
    let console_fut = console_redeem(
        &mut console,
        c_tx,
        c_rx,
        &target,
        &h_prime,
        &nc_prime,
        &console_now,
    );
    let (answer, outcome) = tokio::join!(agent_fut, console_fut);

    assert_eq!(answer, AnswerOutcome::Proved);
    assert_eq!(
        outcome,
        RedemptionOutcome::ContinuityProven {
            target: target.clone()
        }
    );
    // The hard invariant: proving continuity restores no control (ADR-0014).
    assert!(!outcome.grants_control());
    assert!(outcome.is_proven());

    // Single use: both endpoints consumed their record.
    assert!(agent.is_empty(), "agent ticket must be consumed");
    assert!(console.is_empty(), "console ticket must be consumed");
}

/// A second redemption against the already-consumed console vault is refused —
/// the ticket is spent (single use), even though a fresh valid-looking proof
/// arrives.
#[tokio::test]
async fn replayed_redemption_is_refused_single_use() {
    let target = identity();
    let (mut agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    // First, a normal successful redemption consumes the console ticket.
    {
        let (a_tx, c_rx) = unbounded_channel();
        let (c_tx, a_rx) = unbounded_channel();
        let now = clock(BEFORE_EXPIRY);
        let (answer, outcome) = tokio::join!(
            agent_answer_challenge(&mut agent, a_tx, a_rx, &TICKET, &h_prime, &now),
            console_redeem(&mut console, c_tx, c_rx, &target, &h_prime, &nc_prime, &now),
        );
        assert_eq!(answer, AnswerOutcome::Proved);
        assert!(outcome.is_proven());
    }

    // A replayed agent (its blob restored again in the same env) still produces a
    // cryptographically valid proof — but the console already spent the ticket.
    let mut replay_agent = restore_one(
        // Re-arm a matching agent copy so it can produce a real proof.
        {
            let (agent2, _) = armed_pair(&target, 1, 2);
            agent2.armed().next().expect("armed ticket").clone()
        },
        1,
    );

    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();
    let now = clock(BEFORE_EXPIRY);
    let (answer, outcome) = tokio::join!(
        agent_answer_challenge(&mut replay_agent, a_tx, a_rx, &TICKET, &h_prime, &now),
        console_redeem(&mut console, c_tx, c_rx, &target, &h_prime, &nc_prime, &now),
    );

    // The agent still answers (it holds its own copy), but the console refuses:
    // its record was already consumed. Fail closed, no control.
    assert_eq!(answer, AnswerOutcome::Proved);
    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::Vault(RebootResumeRefusal::UnknownTicket)
        }
    );
    assert!(!outcome.grants_control());
}

/// A console clock past `not_after` refuses even a cryptographically valid proof
/// — expiry is read from the console's own sealed copy, not the peer's claim.
#[tokio::test]
async fn console_expired_clock_fails_closed() {
    let target = identity();
    let (mut agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();

    // The agent still sits inside the lifetime and produces a real proof; the
    // console's clock has moved past expiry.
    let agent_now = clock(BEFORE_EXPIRY);
    let console_now = clock(AFTER_EXPIRY);
    let (answer, outcome) = tokio::join!(
        agent_answer_challenge(&mut agent, a_tx, a_rx, &TICKET, &h_prime, &agent_now),
        console_redeem(
            &mut console,
            c_tx,
            c_rx,
            &target,
            &h_prime,
            &nc_prime,
            &console_now
        ),
    );

    assert_eq!(answer, AnswerOutcome::Proved);
    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::Vault(RebootResumeRefusal::Expired)
        }
    );
    assert!(
        console.is_empty(),
        "an expired redemption still consumes the ticket"
    );
}

/// A valid proof against a **different** presented target is refused: target
/// binding is enforced authoritatively console-side (ADR-0014 §3).
#[tokio::test]
async fn wrong_target_fails_closed_with_valid_proof() {
    let target = identity();
    let (mut agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();
    let now = clock(BEFORE_EXPIRY);

    // The console presents a foreign identity even though the proof is valid for
    // the armed target.
    let wrong = other_identity();
    let (answer, outcome) = tokio::join!(
        agent_answer_challenge(&mut agent, a_tx, a_rx, &TICKET, &h_prime, &now),
        console_redeem(&mut console, c_tx, c_rx, &wrong, &h_prime, &nc_prime, &now),
    );

    assert_eq!(answer, AnswerOutcome::Proved);
    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::Vault(RebootResumeRefusal::TargetMismatch)
        }
    );
    assert!(!outcome.grants_control());
}

/// A proof computed against a **different** `h'` than the console verifies with
/// (a mismatched/forged transcript) does not verify — fail closed.
#[tokio::test]
async fn mismatched_transcript_hash_fails_closed() {
    let target = identity();
    let (mut agent, mut console) = armed_pair(&target, 1, 2);
    let nc_prime = [24u8; 32];

    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();
    let now = clock(BEFORE_EXPIRY);

    // Agent proves against one transcript hash; the console verifies against a
    // different one, as if the fresh handshakes did not agree.
    let agent_h = [42u8; 32];
    let console_h = [43u8; 32];
    let (answer, outcome) = tokio::join!(
        agent_answer_challenge(&mut agent, a_tx, a_rx, &TICKET, &agent_h, &now),
        console_redeem(
            &mut console,
            c_tx,
            c_rx,
            &target,
            &console_h,
            &nc_prime,
            &now
        ),
    );

    assert_eq!(answer, AnswerOutcome::Proved);
    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::Vault(RebootResumeRefusal::BadProof)
        }
    );
    assert!(!outcome.grants_control());
}

/// A returning agent whose sealed blob was lifted to a **foreign environment**
/// cannot unseal `K_rr`, so it produces no proof; the console fails closed on no
/// proof (the transport closes without an answer).
#[tokio::test]
async fn cloned_agent_blob_in_foreign_environment_fails_closed() {
    let target = identity();
    // The agent's persisted ticket, restored under the WRONG environment tag.
    let (agent_src, mut console) = armed_pair(&target, 1, 2);
    let stolen = agent_src.armed().next().expect("armed ticket").clone();
    let mut foreign_agent = restore_one(stolen, 99); // wrong env → unseal fails

    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];
    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, a_rx) = unbounded_channel();
    let now = clock(BEFORE_EXPIRY);

    let (answer, outcome) = tokio::join!(
        agent_answer_challenge(&mut foreign_agent, a_tx, a_rx, &TICKET, &h_prime, &now),
        console_redeem(&mut console, c_tx, c_rx, &target, &h_prime, &nc_prime, &now),
    );

    // The foreign agent refuses at unseal and sends nothing; the console sees a
    // closed channel and fails closed on no proof.
    assert_eq!(
        answer,
        AnswerOutcome::Refused {
            local_reason: RebootResumeRefusal::Sealed(SealError::Unrecoverable)
        }
    );
    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::NoProof
        }
    );
    assert!(!outcome.grants_control());
    // The foreign agent's single-use record is spent even though it could not
    // prove: nothing survives to a second attempt.
    assert!(foreign_agent.is_empty());
}

/// An interrupted exchange — the agent drops before answering — fails the
/// console closed on no proof rather than hanging.
#[tokio::test]
async fn interrupted_before_proof_fails_closed() {
    let target = identity();
    let (_agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    let (_a_tx, c_rx) = unbounded_channel::<cretlink_protocol::ControlMessage>();
    let (c_tx, _a_rx) = unbounded_channel();

    // Drop the agent's sender immediately: no proof will ever arrive.
    drop(_a_tx);

    let outcome = console_redeem(
        &mut console,
        c_tx,
        c_rx,
        &target,
        &h_prime,
        &nc_prime,
        &clock(BEFORE_EXPIRY),
    )
    .await;

    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::NoProof
        }
    );
    assert!(!outcome.grants_control());
}

/// A peer that sends a proof naming a malformed (wrong-length) ticket id is
/// refused before the vault is consulted.
#[tokio::test]
async fn malformed_ticket_id_fails_closed() {
    let target = identity();
    let (_agent, mut console) = armed_pair(&target, 1, 2);
    let h_prime = [42u8; 32];
    let nc_prime = [24u8; 32];

    let (a_tx, c_rx) = unbounded_channel();
    let (c_tx, _a_rx) = unbounded_channel::<cretlink_protocol::ControlMessage>();

    // A hand-crafted proof with a 15-byte ticket id (one short).
    a_tx.send(cretlink_protocol::ControlMessage::RebootResumeProof {
        ticket_id: cretlink_protocol::B64Bytes(vec![7u8; 15]),
        proof: cretlink_protocol::B64Bytes(vec![0u8; 32]),
    })
    .expect("send malformed proof");

    let outcome = console_redeem(
        &mut console,
        c_tx,
        c_rx,
        &target,
        &h_prime,
        &nc_prime,
        &clock(BEFORE_EXPIRY),
    )
    .await;

    assert_eq!(
        outcome,
        RedemptionOutcome::NoConsentedSession {
            local_reason: RedemptionRefusal::MalformedTicketId
        }
    );
    assert!(!outcome.grants_control());
}
