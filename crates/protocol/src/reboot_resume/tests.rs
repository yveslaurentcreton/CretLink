//! Reboot-resume authority tests (ADR-0014, ST-M006-002).
//!
//! These cover the pure crypto (frozen derivation + proof vectors, transcript
//! and nonce binding), the target fingerprint, the at-rest seal round-trip and
//! its environment binding, and the full fail-closed redemption matrix:
//! normal, delayed, repeated, interrupted, cloned-state, wrong-target, and
//! expired — plus arm-then-cancel and a faithful reboot (drop the vault,
//! restore from the persisted ticket, redeem once).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::message::TargetIdentity;

// --- small helpers -------------------------------------------------------

fn hx(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn h32(fill: u8) -> [u8; 32] {
    [fill; 32]
}

fn tid(fill: u8) -> TicketId {
    TicketId([fill; REBOOT_RESUME_TICKET_ID_LEN])
}

fn identity() -> TargetIdentity {
    TargetIdentity {
        hostname: "shop-pc-01".to_owned(),
        os_version: "Windows 11 Pro 10.0.26200".to_owned(),
        arch: "x86_64".to_owned(),
        elevated: true,
        simulated: false,
        agent_version: "0.1.0".to_owned(),
    }
}

/// A test-only [`AtRestSealer`] that models DPAPI's per-environment binding: a
/// sealed blob is tagged with an environment id and only the same id can
/// unseal it. It is self-contained (so it survives a simulated reboot) but is
/// **not** confidential — it exercises the vault's control flow and the
/// wrong-environment path, not real encryption.
#[derive(Debug)]
struct InMemorySealer {
    env: u8,
}

impl InMemorySealer {
    const fn new(env: u8) -> Self {
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
        let (tag, body) = sealed.split_first().ok_or(SealError::Unrecoverable)?;
        if *tag != self.env {
            return Err(SealError::Unrecoverable);
        }
        Ok(Zeroizing::new(body.to_vec()))
    }
}

// --- crypto: frozen vectors ---------------------------------------------

#[test]
fn key_derivation_and_proof_are_frozen() {
    // Fixed inputs -> a known continuity proof. This freezes the whole chain:
    // HKDF salt, ikm ordering, HMAC key, and the h'‖ticket_id‖nc' MAC input.
    // A change to any of them breaks this on purpose (ADR-0014 vectors).
    let key = derive_reboot_resume_key(&h32(0x11), &h32(0x22), &h32(0x33));
    let proof = key.proof(&h32(0x44), &tid(0x55), &h32(0x66));
    assert_eq!(
        hx(proof.as_bytes()),
        "3163338e6dd379241816abfca40f2a14da32846e7ed21debed9026ab708bf871",
    );
}

#[test]
fn derivation_binds_to_transcript_and_nonces() {
    let base = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let base_proof = base.proof(&h32(9), &tid(8), &h32(7));

    // Different handshake hash `h` -> different key -> different proof.
    let other_h = derive_reboot_resume_key(&h32(0xAA), &h32(2), &h32(3));
    assert_ne!(
        base_proof.as_bytes(),
        other_h.proof(&h32(9), &tid(8), &h32(7)).as_bytes()
    );
    // Different Na_rr and different Nc_rr each change the key.
    let other_na = derive_reboot_resume_key(&h32(1), &h32(0xAA), &h32(3));
    let other_nc = derive_reboot_resume_key(&h32(1), &h32(2), &h32(0xAA));
    assert_ne!(
        base_proof.as_bytes(),
        other_na.proof(&h32(9), &tid(8), &h32(7)).as_bytes()
    );
    assert_ne!(
        base_proof.as_bytes(),
        other_nc.proof(&h32(9), &tid(8), &h32(7)).as_bytes()
    );
}

#[test]
fn proof_binds_to_h_prime_ticket_and_nonce() {
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let base = key.proof(&h32(4), &tid(5), &h32(6));

    // Each input change yields a different tag, and verify accepts only the
    // matching one.
    assert!(key.verify(&h32(4), &tid(5), &h32(6), base.as_bytes()));
    assert_ne!(
        base.as_bytes(),
        key.proof(&h32(0x40), &tid(5), &h32(6)).as_bytes()
    );
    assert_ne!(
        base.as_bytes(),
        key.proof(&h32(4), &tid(0x50), &h32(6)).as_bytes()
    );
    assert_ne!(
        base.as_bytes(),
        key.proof(&h32(4), &tid(5), &h32(0x60)).as_bytes()
    );
    assert!(!key.verify(&h32(0x40), &tid(5), &h32(6), base.as_bytes()));
    assert!(!key.verify(&h32(4), &tid(0x50), &h32(6), base.as_bytes()));
    assert!(!key.verify(&h32(4), &tid(5), &h32(0x60), base.as_bytes()));
}

#[test]
fn verify_rejects_wrong_length_proof() {
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let good = key.proof(&h32(4), &tid(5), &h32(6));
    // Truncated and over-long candidates both fail closed.
    assert!(!key.verify(&h32(4), &tid(5), &h32(6), &good.as_bytes()[..31]));
    let mut too_long = good.as_bytes().to_vec();
    too_long.push(0);
    assert!(!key.verify(&h32(4), &tid(5), &h32(6), &too_long));
    assert!(!key.verify(&h32(4), &tid(5), &h32(6), &[]));
}

// --- target fingerprint --------------------------------------------------

#[test]
fn target_binding_is_stable_and_distinguishes() {
    let a = TargetBinding::of(&identity());
    assert_eq!(
        a,
        TargetBinding::of(&identity()),
        "same identity -> same binding"
    );

    let mut other = identity();
    other.hostname = "shop-pc-02".to_owned();
    assert_ne!(
        a,
        TargetBinding::of(&other),
        "different hostname -> different"
    );

    let mut elev = identity();
    elev.elevated = false;
    assert_ne!(a, TargetBinding::of(&elev), "elevation is bound");

    let mut sim = identity();
    sim.simulated = true;
    assert_ne!(a, TargetBinding::of(&sim), "simulated flag is bound");
}

#[test]
fn target_binding_length_prefix_prevents_ambiguity() {
    // Without per-field length prefixes, moving a character across a field
    // boundary would collide. The prefixes keep them distinct.
    let mut left = identity();
    left.hostname = "ab".to_owned();
    left.os_version = "c".to_owned();
    let mut right = identity();
    right.hostname = "a".to_owned();
    right.os_version = "bc".to_owned();
    assert_ne!(TargetBinding::of(&left), TargetBinding::of(&right));
}

// --- at-rest sealer ------------------------------------------------------

#[test]
fn in_memory_sealer_round_trips_and_binds_environment() {
    let home = InMemorySealer::new(1);
    let sealed = home.seal(b"secret").unwrap();
    assert_eq!(home.unseal(&sealed).unwrap().as_slice(), b"secret");
    // A foreign environment cannot recover it.
    let foreign = InMemorySealer::new(2);
    assert_eq!(foreign.unseal(&sealed), Err(SealError::Unrecoverable));
}

// --- fail-closed redemption matrix --------------------------------------

const NOT_AFTER: i64 = 10_000;

/// Arms a ticket in a fresh vault and returns the vault, the agent-side key,
/// the ticket id, and the target binding used.
fn armed_vault(
    env: u8,
) -> (
    RebootResumeVault<InMemorySealer>,
    RebootResumeKey,
    TicketId,
    TargetBinding,
) {
    let mut vault = RebootResumeVault::new(InMemorySealer::new(env));
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    vault.arm(id, &key, target, NOT_AFTER).unwrap();
    assert!(!vault.is_empty(), "arming records the ticket");
    (vault, key, id, target)
}

#[test]
fn redeem_normal_accepts_and_consumes() {
    let (mut vault, key, id, target) = armed_vault(1);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    // Well before expiry, matching target and proof.
    assert_eq!(
        vault.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Ok(())
    );
    assert!(vault.is_empty(), "a redeemed ticket is consumed");
}

#[test]
fn redeem_delayed_but_within_window_accepts() {
    let (mut vault, key, id, target) = armed_vault(1);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    // One millisecond before expiry still succeeds.
    assert_eq!(
        vault.redeem(
            &id,
            NOT_AFTER - 1,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Ok(())
    );
}

#[test]
fn redeem_repeated_is_refused_single_use() {
    let (mut vault, key, id, target) = armed_vault(1);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        vault.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Ok(())
    );
    // Second use finds no record — single use.
    assert_eq!(
        vault.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::UnknownTicket)
    );
}

#[test]
fn redeem_interrupted_bad_proof_leaves_no_authority() {
    let (mut vault, key, id, target) = armed_vault(1);
    // A wrong proof (wrong nc') is refused...
    assert_eq!(
        vault.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x99),
            key.proof(&h32(0x77), &id, &h32(0x88)).as_bytes()
        ),
        Err(RebootResumeRefusal::BadProof)
    );
    // ...and it still consumed the ticket: no lingering, replayable authority.
    assert!(vault.is_empty());
    let good = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        vault.redeem(&id, 5_000, &target, &h32(0x77), &h32(0x88), good.as_bytes()),
        Err(RebootResumeRefusal::UnknownTicket)
    );
}

#[test]
fn redeem_cloned_state_to_foreign_environment_fails_closed() {
    // Arm on environment 1, then copy the persisted ticket to a vault whose
    // sealer is a different environment (a stolen ProgramData file on another
    // machine). The clone cannot unseal K_rr.
    let mut home = RebootResumeVault::new(InMemorySealer::new(1));
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let persisted = home.arm(id, &key, target, NOT_AFTER).unwrap();

    let mut foreign = RebootResumeVault::restore(InMemorySealer::new(2), [persisted]);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        foreign.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::Sealed(SealError::Unrecoverable))
    );
    assert!(
        foreign.is_empty(),
        "the clone attempt still consumes its copy"
    );
}

#[test]
fn redeem_wrong_target_is_refused() {
    let (mut vault, key, id, _target) = armed_vault(1);
    let mut other = identity();
    other.hostname = "attacker-pc".to_owned();
    let wrong = TargetBinding::of(&other);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        vault.redeem(&id, 5_000, &wrong, &h32(0x77), &h32(0x88), proof.as_bytes()),
        Err(RebootResumeRefusal::TargetMismatch)
    );
}

#[test]
fn redeem_expired_is_refused() {
    let (mut vault, key, id, target) = armed_vault(1);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    // Exactly at not_after is already expired (half-open window).
    assert_eq!(
        vault.redeem(
            &id,
            NOT_AFTER,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::Expired)
    );
    assert!(
        vault.is_empty(),
        "an expired attempt still consumes the ticket"
    );
}

#[test]
fn redeem_unknown_ticket_is_refused() {
    let (mut vault, key, id, target) = armed_vault(1);
    let proof = key.proof(&h32(0x77), &tid(0x00), &h32(0x88));
    // Never-armed id.
    assert_eq!(
        vault.redeem(
            &tid(0x00),
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::UnknownTicket)
    );
    // The real ticket is untouched by the miss.
    let good = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        vault.redeem(&id, 5_000, &target, &h32(0x77), &h32(0x88), good.as_bytes()),
        Ok(())
    );
}

// --- agent-side prove mirror --------------------------------------------

#[test]
fn prove_normal_produces_verifiable_proof_and_consumes() {
    let (mut vault, key, id, _target) = armed_vault(1);
    // The console challenge is (h', Nc'); the agent answers with prove.
    let proof = vault
        .prove(&id, 5_000, &h32(0x77), &h32(0x88))
        .expect("a live ticket answers the challenge");
    // The proof is exactly the continuity proof the console will verify.
    assert!(key.verify(&h32(0x77), &id, &h32(0x88), proof.as_bytes()));
    assert!(
        vault.is_empty(),
        "answering a challenge consumes the ticket"
    );
}

#[test]
fn prove_repeated_is_refused_single_use() {
    let (mut vault, _key, id, _target) = armed_vault(1);
    assert!(vault.prove(&id, 5_000, &h32(0x77), &h32(0x88)).is_ok());
    // The agent can never answer twice for the same ticket.
    assert_eq!(
        vault.prove(&id, 5_000, &h32(0x77), &h32(0x99)),
        Err(RebootResumeRefusal::UnknownTicket)
    );
}

#[test]
fn prove_expired_refused_and_consumed() {
    let (mut vault, _key, id, _target) = armed_vault(1);
    // At/after not_after the agent emits no proof — and still consumes.
    assert_eq!(
        vault.prove(&id, NOT_AFTER, &h32(0x77), &h32(0x88)),
        Err(RebootResumeRefusal::Expired)
    );
    assert!(
        vault.is_empty(),
        "an expired challenge still consumes the ticket"
    );
}

#[test]
fn prove_unknown_ticket_refused() {
    let (mut vault, _key, _id, _target) = armed_vault(1);
    assert_eq!(
        vault.prove(&tid(0x00), 5_000, &h32(0x77), &h32(0x88)),
        Err(RebootResumeRefusal::UnknownTicket)
    );
}

#[test]
fn prove_cloned_state_to_foreign_environment_fails_closed() {
    // A sealed agent ticket lifted to another machine cannot be unsealed, so no
    // proof is ever produced from the stolen file — and the clone consumes its
    // own copy.
    let mut home = RebootResumeVault::new(InMemorySealer::new(1));
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let persisted = home.arm(id, &key, target, NOT_AFTER).unwrap();

    let mut foreign = RebootResumeVault::restore(InMemorySealer::new(2), [persisted]);
    assert_eq!(
        foreign.prove(&id, 5_000, &h32(0x77), &h32(0x88)),
        Err(RebootResumeRefusal::Sealed(SealError::Unrecoverable))
    );
    assert!(
        foreign.is_empty(),
        "the clone attempt still consumes its copy"
    );
}

#[test]
fn bilateral_prove_then_redeem_reconnects_once() {
    // The whole ADR-0014 §2/§3 continuity, end to end at the crypto layer: both
    // endpoints derived the *same* K_rr from (h ‖ Na_rr ‖ Nc_rr) at arming and
    // each persisted its own sealed ticket. After the reboot the agent runs a
    // fresh Noise handshake (h'), the console sends a fresh challenge Nc', the
    // agent proves, and the console redeems — accepting the peer and consuming
    // both records. No control is set by any of this.
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());

    // Two independent environments (agent + console), same ticket material.
    let mut agent = RebootResumeVault::new(InMemorySealer::new(1));
    let _ = agent.arm(id, &key, target, NOT_AFTER).unwrap();
    let mut console = RebootResumeVault::new(InMemorySealer::new(2));
    let _ = console.arm(id, &key, target, NOT_AFTER).unwrap();

    let h_prime = h32(0xA1);
    let nc_prime = h32(0xB2);

    let proof = agent
        .prove(&id, 5_000, &h_prime, &nc_prime)
        .expect("the returning agent answers the console challenge");
    assert_eq!(
        console.redeem(&id, 5_000, &target, &h_prime, &nc_prime, proof.as_bytes()),
        Ok(())
    );
    assert!(
        agent.is_empty() && console.is_empty(),
        "both records consumed"
    );
}

#[test]
fn bilateral_wrong_target_fails_closed_even_with_valid_proof() {
    // The agent proves honestly, but the console sees a different target after
    // reboot (a swapped or spoofed machine). Even a cryptographically valid
    // proof is refused: identity binding is authoritative.
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());

    let mut agent = RebootResumeVault::new(InMemorySealer::new(1));
    let _ = agent.arm(id, &key, target, NOT_AFTER).unwrap();
    let mut console = RebootResumeVault::new(InMemorySealer::new(2));
    let _ = console.arm(id, &key, target, NOT_AFTER).unwrap();

    let proof = agent.prove(&id, 5_000, &h32(0xA1), &h32(0xB2)).unwrap();
    let mut other = identity();
    other.hostname = "attacker-pc".to_owned();
    let wrong = TargetBinding::of(&other);
    assert_eq!(
        console.redeem(&id, 5_000, &wrong, &h32(0xA1), &h32(0xB2), proof.as_bytes()),
        Err(RebootResumeRefusal::TargetMismatch)
    );
}

// --- lifecycle: disarm and reboot ---------------------------------------

#[test]
fn disarm_before_reboot_leaves_no_ticket() {
    let (mut vault, key, id, target) = armed_vault(1);
    assert!(vault.disarm(&id), "disarm reports the record was present");
    assert!(vault.is_empty());
    // A later reconnect attempt finds nothing.
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        vault.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::UnknownTicket)
    );
    assert!(!vault.disarm(&id), "disarming twice reports absent");
}

#[test]
fn restore_after_reboot_then_redeem_once() {
    // Arm, persist the ticket, drop the vault (the reboot), then rebuild from
    // the persisted record and the same environment's sealer, and redeem.
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let persisted = {
        let mut pre = RebootResumeVault::new(InMemorySealer::new(1));
        pre.arm(id, &key, target, NOT_AFTER).unwrap()
    };

    let mut post = RebootResumeVault::restore(InMemorySealer::new(1), [persisted]);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        post.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Ok(())
    );
    assert!(post.is_empty());
}

#[test]
fn tampered_clear_expiry_does_not_widen_the_window() {
    // Editing the clear-mirror `not_after` at rest must not extend the real
    // window: redemption reads expiry from the sealed copy.
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let mut persisted = {
        let mut pre = RebootResumeVault::new(InMemorySealer::new(1));
        pre.arm(id, &key, target, NOT_AFTER).unwrap()
    };
    // Attacker pushes the clear expiry far into the future.
    persisted.not_after_unix_millis = i64::MAX;

    let mut post = RebootResumeVault::restore(InMemorySealer::new(1), [persisted]);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    // Real (sealed) expiry still governs: past NOT_AFTER is refused.
    assert_eq!(
        post.redeem(
            &id,
            NOT_AFTER + 1,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::Expired)
    );
}

#[test]
fn tampered_sealed_blob_fails_closed() {
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let persisted = {
        let mut pre = RebootResumeVault::new(InMemorySealer::new(1));
        pre.arm(id, &key, target, NOT_AFTER).unwrap()
    };

    // Truncating the sealed blob makes the payload unparseable.
    let mut truncated = persisted.clone();
    let short = truncated.sealed_key.as_bytes()[..10].to_vec();
    truncated.sealed_key = B64Bytes::new(short);
    let mut v1 = RebootResumeVault::restore(InMemorySealer::new(1), [truncated]);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        v1.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Err(RebootResumeRefusal::Sealed(SealError::Unrecoverable))
    );
}

// --- at-rest serde -------------------------------------------------------

#[test]
fn armed_ticket_round_trips_through_json() {
    let mut vault = RebootResumeVault::new(InMemorySealer::new(1));
    let key = derive_reboot_resume_key(&h32(1), &h32(2), &h32(3));
    let id = tid(0x55);
    let target = TargetBinding::of(&identity());
    let ticket = vault.arm(id, &key, target, NOT_AFTER).unwrap();

    let json = serde_json::to_string(&ticket).unwrap();
    let back: ArmedTicket = serde_json::from_str(&json).unwrap();
    assert_eq!(back.ticket_id, id);
    assert_eq!(back.target, target);
    assert_eq!(back.not_after_unix_millis, NOT_AFTER);
    assert_eq!(back.sealed_key.as_bytes(), ticket.sealed_key.as_bytes());

    // The restored ticket still redeems.
    let mut post = RebootResumeVault::restore(InMemorySealer::new(1), [back]);
    let proof = key.proof(&h32(0x77), &id, &h32(0x88));
    assert_eq!(
        post.redeem(
            &id,
            5_000,
            &target,
            &h32(0x77),
            &h32(0x88),
            proof.as_bytes()
        ),
        Ok(())
    );
}

#[test]
fn armed_ticket_rejects_unknown_fields() {
    let json = r#"{
        "ticket_id": "VVVVVVVVVVVVVVVVVVVVVA==",
        "target": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "not_after_unix_millis": 10000,
        "sealed_key": "AQ==",
        "rogue": true
    }"#;
    assert!(serde_json::from_str::<ArmedTicket>(json).is_err());
}

#[test]
fn armed_ticket_rejects_wrong_length_ids() {
    // ticket_id decodes to 2 bytes, not 16 -> fail closed.
    let json = r#"{
        "ticket_id": "AAA=",
        "target": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        "not_after_unix_millis": 10000,
        "sealed_key": "AQ=="
    }"#;
    assert!(serde_json::from_str::<ArmedTicket>(json).is_err());
}

#[test]
fn ticket_id_display_is_lowercase_hex() {
    assert_eq!(
        tid(0xAB).to_string(),
        "ab".repeat(REBOOT_RESUME_TICKET_ID_LEN)
    );
}
