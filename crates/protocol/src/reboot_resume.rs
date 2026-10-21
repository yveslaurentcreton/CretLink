//! Reboot-resume continuity authority (ADR-0014).
//!
//! Reboot-resume lets a technician continue **one** authorized session across a
//! single customer reboot without re-running the whole pairing dance — while
//! never restoring control from stored state. This module carries the pure,
//! transcript-binding pieces that both endpoints must compute **identically**,
//! exactly as [`pairing`](crate::pairing) carries the Session Approval Code:
//!
//! - [`derive_reboot_resume_key`] turns the armed session's Noise handshake
//!   hash `h` and two fresh reboot-resume nonces into the ticket secret
//!   `K_rr = HKDF-SHA256("cretlink-rr-v1", h ‖ Na_rr ‖ Nc_rr)` (REQ-S-006).
//! - [`RebootResumeKey::proof`] / [`RebootResumeKey::verify`] compute and
//!   constant-time check the post-reboot continuity proof
//!   `HMAC-SHA256(K_rr, h' ‖ ticket_id ‖ Nc')`, proving the returning agent is
//!   the same one that armed the ticket (REQ-S-007, REQ-S-008).
//! - [`RebootResumeVault`] is the single-use, expiring, target-bound state
//!   machine over those primitives. Arming seals `K_rr` at rest behind an
//!   injected [`AtRestSealer`]; redeeming removes the record **before** any
//!   check, so a ticket can never be spent twice — on acceptance, expiry, or
//!   any failure (REQ-S-012).
//!
//! # What this is not
//!
//! Redeeming a ticket authenticates the *peer* and reduces re-pair friction; it
//! never sets control state. Attended full-control consent (REQ-F-006) still
//! runs after a successful reconnect. Every refusal ([`RebootResumeRefusal`])
//! collapses upstream to a single `NoConsentedSession` outcome so a caller can
//! never leak which check failed or land in a partial-success state
//! (REQ-S-009).
//!
//! This module invents no cryptography: it composes SHA-256, HKDF-SHA256, and
//! HMAC-SHA256 from the RustCrypto crates exactly as ADR-0014 specifies
//! (REQ-S-005). It is transport- and clock-agnostic — time enters as an
//! `i64` Unix-millisecond argument (the runtime passes `Clock::now_unix_millis`)
//! and persistence enters through the [`AtRestSealer`] trait (the Windows
//! adapter uses DPAPI; tests use an in-memory ideal sealer). No key material
//! lives in this module beyond a single [`RebootResumeKey`] transiently held
//! during derivation or redemption, and it zeroizes on drop.

use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::b64::B64Bytes;
use crate::message::TargetIdentity;

/// HKDF salt (ikm-independent domain separator) for the reboot-resume key
/// (ADR-0014). Distinct from [`crate::SAC_HKDF_SALT`] so the two derivations
/// can never collide even on identical keying material.
pub const REBOOT_RESUME_HKDF_SALT: &[u8] = b"cretlink-rr-v1";

/// Domain-separation prefix for the canonical target-binding pre-image
/// (ADR-0014, frozen wire layout). The trailing NUL keeps the fixed prefix
/// unambiguous from the variable-length body that follows.
pub const REBOOT_RESUME_TARGET_MAGIC: &[u8] = b"cretlink-rr-target-v1\0";

/// Length in bytes of the reboot-resume key `K_rr` (an HMAC-SHA256 key).
pub const REBOOT_RESUME_KEY_LEN: usize = 32;

/// Length in bytes of each reboot-resume nonce (`Na_rr`, `Nc_rr`) and of the
/// post-reboot handshake hash `h'` fed into the continuity proof. Matches the
/// 32-byte SHA-256 output the Noise handshake hash uses.
pub const REBOOT_RESUME_NONCE_LEN: usize = 32;

/// Length in bytes of a [`TicketId`].
pub const REBOOT_RESUME_TICKET_ID_LEN: usize = 16;

/// Length in bytes of a [`TargetBinding`] (a SHA-256 digest).
pub const REBOOT_RESUME_TARGET_LEN: usize = 32;

/// Length in bytes of a continuity proof (an HMAC-SHA256 tag).
pub const REBOOT_RESUME_PROOF_LEN: usize = 32;

/// Single-use identifier for one armed reboot-resume ticket (ADR-0014).
///
/// The runtime generates these from OS randomness (like the pairing nonces in
/// `cretlink-transport`); this crate only holds and compares the value. It is
/// bound into the continuity proof, so a proof computed for one ticket can
/// never be replayed against another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TicketId(pub [u8; REBOOT_RESUME_TICKET_ID_LEN]);

impl TicketId {
    /// Borrows the raw identifier bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; REBOOT_RESUME_TICKET_ID_LEN] {
        &self.0
    }
}

impl fmt::Debug for TicketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TicketId({self})")
    }
}

impl fmt::Display for TicketId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for TicketId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for TicketId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = decode_fixed::<D, REBOOT_RESUME_TICKET_ID_LEN>(deserializer, "ticket_id")?;
        Ok(Self(bytes))
    }
}

/// Stable fingerprint of the armed target's identity (ADR-0014).
///
/// It is `SHA-256` over a canonical, length-prefixed encoding of the
/// [`TargetIdentity`] (see [`TargetBinding::of`]). Redeeming after reboot
/// requires the returning peer to present a target that hashes to the same
/// value, so a ticket armed for one machine is refused for another even before
/// the cryptographic proof is checked. The fingerprint is not secret.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TargetBinding(pub [u8; REBOOT_RESUME_TARGET_LEN]);

impl TargetBinding {
    /// Computes the binding for a target identity over a frozen, canonical
    /// pre-image.
    ///
    /// # Wire layout
    ///
    /// 1. [`REBOOT_RESUME_TARGET_MAGIC`];
    /// 2. each of `hostname`, `os_version`, `arch`, `agent_version` as a
    ///    big-endian `u16` UTF-8 byte length followed by the bytes;
    /// 3. `elevated` then `simulated`, each a single `0x00`/`0x01` byte.
    ///
    /// The length prefixes make the concatenation unambiguous, so two distinct
    /// identities can never share a pre-image.
    #[must_use]
    pub fn of(identity: &TargetIdentity) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(REBOOT_RESUME_TARGET_MAGIC);
        for field in [
            identity.hostname.as_str(),
            identity.os_version.as_str(),
            identity.arch.as_str(),
            identity.agent_version.as_str(),
        ] {
            // Field lengths are bounded by the protocol identity limits, well
            // within `u16`; a hostile over-length field is truncated by the
            // cast, which at worst changes the fingerprint (fail closed).
            hasher.update((field.len() as u16).to_be_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update([u8::from(identity.elevated)]);
        hasher.update([u8::from(identity.simulated)]);
        Self(hasher.finalize().into())
    }

    /// Borrows the raw fingerprint bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; REBOOT_RESUME_TARGET_LEN] {
        &self.0
    }
}

impl fmt::Debug for TargetBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show only a short prefix; the full digest adds nothing to logs.
        write!(
            f,
            "TargetBinding({:02x}{:02x}{:02x}{:02x}…)",
            self.0[0], self.0[1], self.0[2], self.0[3]
        )
    }
}

impl Serialize for TargetBinding {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for TargetBinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let bytes = decode_fixed::<D, REBOOT_RESUME_TARGET_LEN>(deserializer, "target")?;
        Ok(Self(bytes))
    }
}

/// Decodes a base64 string into a fixed-length byte array, failing closed when
/// the decoded length is wrong (a malformed at-rest record).
fn decode_fixed<'de, D: Deserializer<'de>, const N: usize>(
    deserializer: D,
    field: &'static str,
) -> Result<[u8; N], D::Error> {
    let text = String::deserialize(deserializer)?;
    let bytes = STANDARD
        .decode(text.as_bytes())
        .map_err(serde::de::Error::custom)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| serde::de::Error::custom(format!("{field} must be {N} bytes")))
}

/// The reboot-resume ticket secret `K_rr` (ADR-0014).
///
/// Derived from the armed session transcript; used only as an HMAC-SHA256 key
/// for the continuity proof. It carries no `Clone`/`Copy` and zeroizes on drop,
/// so it exists in memory only transiently during derivation or redemption.
pub struct RebootResumeKey(Zeroizing<[u8; REBOOT_RESUME_KEY_LEN]>);

impl fmt::Debug for RebootResumeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RebootResumeKey(redacted)")
    }
}

impl RebootResumeKey {
    /// Borrows the raw key bytes (HMAC key material). Intentionally
    /// crate-private-ish: only the sealer round-trip and the proof need it.
    #[must_use]
    fn as_bytes(&self) -> &[u8; REBOOT_RESUME_KEY_LEN] {
        &self.0
    }

    /// Reconstructs a key from unsealed bytes, failing closed on a wrong length.
    #[must_use]
    fn from_unsealed(bytes: &[u8]) -> Option<Self> {
        let arr = <[u8; REBOOT_RESUME_KEY_LEN]>::try_from(bytes).ok()?;
        Some(Self(Zeroizing::new(arr)))
    }

    /// Computes the continuity proof `HMAC-SHA256(K_rr, h' ‖ ticket_id ‖ Nc')`.
    ///
    /// The returning agent computes this after a fresh post-reboot Noise
    /// handshake (yielding `h'`) and sends it with `Nc'`; the console verifies
    /// it with [`RebootResumeKey::verify`].
    #[must_use]
    pub fn proof(
        &self,
        h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        ticket_id: &TicketId,
        nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    ) -> RebootResumeProof {
        // `new_mac` only returns `None` on an invalid HMAC key length, which
        // cannot happen for our fixed 32-byte key. We still avoid a panic: the
        // impossible branch yields an all-zero tag that never verifies, so the
        // failure mode stays closed rather than crashing.
        let tag = mac_tag(self.as_bytes(), h_prime, ticket_id, nc_prime)
            .unwrap_or([0u8; REBOOT_RESUME_PROOF_LEN]);
        RebootResumeProof(tag)
    }

    /// Verifies a presented continuity proof in constant time.
    ///
    /// Returns `false` for any mismatch or malformed input — never a panic and
    /// never an early return that leaks timing about the tag contents.
    #[must_use]
    pub fn verify(
        &self,
        h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        ticket_id: &TicketId,
        nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        presented: &[u8],
    ) -> bool {
        let Some(mut mac) = new_mac(self.as_bytes()) else {
            return false;
        };
        mac.update(h_prime);
        mac.update(&ticket_id.0);
        mac.update(nc_prime);
        // `verify_slice` performs a constant-time comparison of the computed
        // tag against `presented` and rejects a wrong-length input.
        mac.verify_slice(presented).is_ok()
    }
}

/// Derives the reboot-resume key from the armed session transcript (ADR-0014).
///
/// `K_rr = HKDF-SHA256(salt = "cretlink-rr-v1", ikm = h ‖ Na_rr ‖ Nc_rr)`.
/// The handshake hash `h` binds the key to the exact armed session; the two
/// fresh nonces keep separate arming rounds from colliding. The transient input
/// keying material is zeroized when it leaves scope.
#[must_use]
pub fn derive_reboot_resume_key(
    h: &[u8; 32],
    na_rr: &[u8; REBOOT_RESUME_NONCE_LEN],
    nc_rr: &[u8; REBOOT_RESUME_NONCE_LEN],
) -> RebootResumeKey {
    let mut ikm = Zeroizing::new([0u8; 32 + REBOOT_RESUME_NONCE_LEN + REBOOT_RESUME_NONCE_LEN]);
    ikm[..32].copy_from_slice(h);
    ikm[32..32 + REBOOT_RESUME_NONCE_LEN].copy_from_slice(na_rr);
    ikm[32 + REBOOT_RESUME_NONCE_LEN..].copy_from_slice(nc_rr);

    let hk = Hkdf::<Sha256>::new(Some(REBOOT_RESUME_HKDF_SALT), ikm.as_ref());
    let mut okm = Zeroizing::new([0u8; REBOOT_RESUME_KEY_LEN]);
    // HKDF-Expand only fails when the requested output exceeds 255*HashLen
    // (8160 bytes for SHA-256); a 32-byte output can never trigger that, so the
    // result is always `Ok`. We discard it explicitly rather than unwrap to
    // keep library code free of panics (workspace lints deny them).
    let _ = hk.expand(b"", okm.as_mut());

    RebootResumeKey(okm)
}

/// A post-reboot continuity proof: an HMAC-SHA256 tag (ADR-0014).
#[derive(Clone, PartialEq, Eq)]
pub struct RebootResumeProof(pub [u8; REBOOT_RESUME_PROOF_LEN]);

impl RebootResumeProof {
    /// Borrows the raw tag bytes for transmission.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; REBOOT_RESUME_PROOF_LEN] {
        &self.0
    }
}

impl fmt::Debug for RebootResumeProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RebootResumeProof(len={})", self.0.len())
    }
}

/// Builds a fresh HMAC-SHA256 instance keyed by `key`, or `None` on the
/// impossible invalid-length path (kept panic-free).
fn new_mac(key: &[u8]) -> Option<Hmac<Sha256>> {
    <Hmac<Sha256> as Mac>::new_from_slice(key).ok()
}

/// Computes `HMAC-SHA256(key, h' ‖ ticket_id ‖ Nc')`, or `None` on the
/// impossible invalid-key-length path.
fn mac_tag(
    key: &[u8],
    h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    ticket_id: &TicketId,
    nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
) -> Option<[u8; REBOOT_RESUME_PROOF_LEN]> {
    let mut mac = new_mac(key)?;
    mac.update(h_prime);
    mac.update(&ticket_id.0);
    mac.update(nc_prime);
    let tag = mac.finalize().into_bytes();
    let mut out = [0u8; REBOOT_RESUME_PROOF_LEN];
    out.copy_from_slice(&tag);
    Some(out)
}

/// Frozen byte layout of the sealed reboot-resume payload (ADR-0014):
/// `not_after_be(8) ‖ target(32) ‖ ticket_id(16) ‖ K_rr(32)`.
///
/// The [`AtRestSealer`] authenticates this whole blob, so the security-critical
/// fields (`not_after`, `target`, `ticket_id`) cannot be tampered at rest
/// independently of the key. Redemption reads its expiry, target, and identity
/// from **here**, not from the clear mirror in [`ArmedTicket`].
const SEALED_PAYLOAD_LEN: usize =
    8 + REBOOT_RESUME_TARGET_LEN + REBOOT_RESUME_TICKET_ID_LEN + REBOOT_RESUME_KEY_LEN;

/// Builds the sealed payload pre-image in the frozen [`SEALED_PAYLOAD_LEN`]
/// layout. The buffer is zeroized when it leaves scope so `K_rr` does not
/// linger after sealing.
fn build_sealed_payload(
    not_after_unix_millis: i64,
    target: &TargetBinding,
    ticket_id: &TicketId,
    key: &RebootResumeKey,
) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(SEALED_PAYLOAD_LEN));
    out.extend_from_slice(&not_after_unix_millis.to_be_bytes());
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(ticket_id.as_bytes());
    out.extend_from_slice(key.as_bytes());
    out
}

/// The authoritative fields recovered from a sealed payload.
struct OpenedTicket {
    not_after_unix_millis: i64,
    target: TargetBinding,
    ticket_id: TicketId,
    key: RebootResumeKey,
}

/// Parses a sealed payload back into its fields, failing closed on any wrong
/// length (a truncated or tampered blob).
fn open_sealed_payload(bytes: &[u8]) -> Option<OpenedTicket> {
    if bytes.len() != SEALED_PAYLOAD_LEN {
        return None;
    }
    let not_after_unix_millis = i64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
    let target = TargetBinding(bytes.get(8..40)?.try_into().ok()?);
    let ticket_id = TicketId(bytes.get(40..56)?.try_into().ok()?);
    let key = RebootResumeKey::from_unsealed(bytes.get(56..88)?)?;
    Some(OpenedTicket {
        not_after_unix_millis,
        target,
        ticket_id,
        key,
    })
}

/// The persistable record of one armed reboot-resume ticket (ADR-0014).
///
/// This is the durable, at-rest representation the owned-resource inventory
/// (ST-M006-003) stores and the agent reloads after a reboot. The secret
/// `K_rr` never appears here in the clear — only [`sealed_key`], an
/// [`AtRestSealer`] blob over the frozen `SEALED_PAYLOAD_LEN` payload, which
/// also carries the authoritative `not_after`, `target`, and `ticket_id`.
///
/// The `ticket_id`, `target`, and `not_after_unix_millis` fields here are a
/// **clear mirror** for the inventory listing, the both-endpoints "armed"
/// indication, and the expiry sweep. Redemption never trusts them for a
/// security decision — it uses the sealed copy — so tampering the clear mirror
/// at rest cannot widen the expiry window or retarget the ticket.
///
/// [`sealed_key`]: ArmedTicket::sealed_key
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmedTicket {
    /// Single-use identifier (clear mirror), bound into the continuity proof.
    pub ticket_id: TicketId,
    /// Fingerprint of the target this ticket was armed for (clear mirror).
    pub target: TargetBinding,
    /// Expiry as Unix milliseconds (UTC), clear mirror for the sweep; the
    /// authoritative copy is sealed.
    pub not_after_unix_millis: i64,
    /// The sealed payload: `K_rr` plus the authoritative `not_after`, `target`,
    /// and `ticket_id`, produced by an [`AtRestSealer`].
    pub sealed_key: B64Bytes,
}

/// An error sealing or unsealing reboot-resume key material at rest.
///
/// Deliberately coarse: it must not distinguish *why* an unseal failed (wrong
/// user, wrong machine, tamper), because every such failure collapses to the
/// same fail-closed reconnect refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SealError {
    /// The platform sealing backend refused or is unavailable.
    Backend,
    /// The sealed material could not be recovered (absent, wrong environment,
    /// or tampered).
    Unrecoverable,
}

impl fmt::Display for SealError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend => f.write_str("at-rest sealing backend refused or unavailable"),
            Self::Unrecoverable => f.write_str("sealed material could not be recovered"),
        }
    }
}

impl std::error::Error for SealError {}

/// Seals reboot-resume key material so it survives a reboot without ever
/// resting in the clear (ADR-0014).
///
/// The production Windows adapter wraps DPAPI (`CryptProtectData`,
/// per-user), which binds the ciphertext to the same user account on the same
/// machine — so a copy of the sealed file taken to another machine cannot be
/// unsealed. Tests use an in-memory ideal sealer with the same contract. An
/// implementation must be authenticated: [`unseal`](AtRestSealer::unseal) must
/// fail rather than return altered plaintext for tampered input.
pub trait AtRestSealer: fmt::Debug {
    /// Seals `plaintext`, returning opaque sealed bytes.
    ///
    /// # Errors
    ///
    /// Returns [`SealError::Backend`] if the platform backend refuses.
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError>;

    /// Recovers plaintext previously produced by [`seal`](AtRestSealer::seal).
    ///
    /// # Errors
    ///
    /// Returns [`SealError::Unrecoverable`] if the input is absent for this
    /// environment, wrong, or tampered, and [`SealError::Backend`] if the
    /// backend is unavailable.
    fn unseal(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, SealError>;
}

/// Why a reboot-resume redemption was refused (ADR-0014).
///
/// The granularity exists for honest local audit only. The session layer maps
/// **every** variant to a single `NoConsentedSession` outcome toward the peer,
/// so a caller can never learn which check failed and can never observe a
/// partial success (REQ-S-007 through REQ-S-009).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RebootResumeRefusal {
    /// No armed ticket with that id — never armed, already redeemed, or
    /// disarmed. Enforces single use.
    UnknownTicket,
    /// The ticket is at or past its `not_after` expiry.
    Expired,
    /// The presented target does not match the armed target fingerprint.
    TargetMismatch,
    /// The sealed key could not be unsealed in this environment.
    Sealed(SealError),
    /// The continuity proof did not verify against `K_rr`.
    BadProof,
}

impl fmt::Display for RebootResumeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTicket => f.write_str("no such armed reboot-resume ticket"),
            Self::Expired => f.write_str("reboot-resume ticket has expired"),
            Self::TargetMismatch => f.write_str("reboot-resume target does not match"),
            Self::Sealed(err) => write!(f, "reboot-resume key unseal failed: {err}"),
            Self::BadProof => f.write_str("reboot-resume continuity proof did not verify"),
        }
    }
}

impl std::error::Error for RebootResumeRefusal {}

/// Single-use, expiring, target-bound reboot-resume state machine (ADR-0014).
///
/// The vault owns the set of currently-armed tickets and an [`AtRestSealer`].
/// It is deliberately I/O- and clock-free: arming seals `K_rr` through the
/// sealer and hands back the persistable [`ArmedTicket`]; the caller writes
/// that to the owned-resource inventory. After a reboot the caller rebuilds the
/// vault with [`restore`](RebootResumeVault::restore) from the persisted
/// tickets and the environment's sealer, then redeems.
///
/// # Single use
///
/// [`redeem`](RebootResumeVault::redeem) removes the record **before** any
/// check runs, so the same `ticket_id` can never be redeemed twice — whatever
/// the outcome. A second attempt (including a cloned copy replayed against the
/// same vault) finds nothing and is refused [`UnknownTicket`].
///
/// [`UnknownTicket`]: RebootResumeRefusal::UnknownTicket
#[derive(Debug)]
pub struct RebootResumeVault<S: AtRestSealer> {
    sealer: S,
    records: std::collections::BTreeMap<TicketId, ArmedTicket>,
}

impl<S: AtRestSealer> RebootResumeVault<S> {
    /// Creates an empty vault backed by `sealer`.
    #[must_use]
    pub fn new(sealer: S) -> Self {
        Self {
            sealer,
            records: std::collections::BTreeMap::new(),
        }
    }

    /// Rebuilds a vault after a reboot from persisted tickets and the
    /// environment's sealer. Tickets whose id repeats keep the last one.
    #[must_use]
    pub fn restore(sealer: S, tickets: impl IntoIterator<Item = ArmedTicket>) -> Self {
        let mut records = std::collections::BTreeMap::new();
        for ticket in tickets {
            records.insert(ticket.ticket_id, ticket);
        }
        Self { sealer, records }
    }

    /// Arms a ticket: seals `K_rr` at rest, stores the record, and returns the
    /// persistable [`ArmedTicket`] for the owned-resource inventory.
    ///
    /// Arming by itself grants no access and sets no control state — it only
    /// mints the single-use secret bound to `target` and expiring at
    /// `not_after_unix_millis`.
    ///
    /// # Errors
    ///
    /// Returns [`SealError`] if the at-rest sealer refuses.
    pub fn arm(
        &mut self,
        ticket_id: TicketId,
        key: &RebootResumeKey,
        target: TargetBinding,
        not_after_unix_millis: i64,
    ) -> Result<ArmedTicket, SealError> {
        let payload = build_sealed_payload(not_after_unix_millis, &target, &ticket_id, key);
        let sealed = self.sealer.seal(&payload)?;
        let ticket = ArmedTicket {
            ticket_id,
            target,
            not_after_unix_millis,
            sealed_key: B64Bytes::new(sealed),
        };
        self.records.insert(ticket_id, ticket.clone());
        Ok(ticket)
    }

    /// Disarms an armed ticket before reboot (arm-then-cancel), returning
    /// whether a record was present. Leaves no ticket behind.
    pub fn disarm(&mut self, ticket_id: &TicketId) -> bool {
        self.records.remove(ticket_id).is_some()
    }

    /// The currently-armed tickets, for the both-endpoints "armed" indication
    /// and the residual inventory.
    pub fn armed(&self) -> impl Iterator<Item = &ArmedTicket> {
        self.records.values()
    }

    /// Whether any ticket is currently armed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Redeems a ticket after reboot, consuming it whatever the outcome.
    ///
    /// The record is removed first (single use), then the checks run in
    /// cheap-to-costly order: expiry, target binding, unseal, and finally the
    /// constant-time continuity proof. Success authenticates the returning
    /// peer; it does **not** restore control — attended full-control consent
    /// still runs afterward.
    ///
    /// # Errors
    ///
    /// Returns the specific [`RebootResumeRefusal`] for local audit; callers
    /// present a single `NoConsentedSession` outcome to the peer.
    pub fn redeem(
        &mut self,
        ticket_id: &TicketId,
        now_unix_millis: i64,
        presented_target: &TargetBinding,
        h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        presented_proof: &[u8],
    ) -> Result<(), RebootResumeRefusal> {
        // Single use: remove before any check so this id is spent regardless of
        // acceptance, expiry, or failure (REQ-S-012).
        let ticket = self
            .records
            .remove(ticket_id)
            .ok_or(RebootResumeRefusal::UnknownTicket)?;

        // Recover the authoritative fields from the sealed payload. A copy of
        // the sealed bytes taken to another environment cannot be unsealed, and
        // a tampered blob fails to parse — both fail closed here.
        let opened_bytes = self
            .sealer
            .unseal(ticket.sealed_key.as_bytes())
            .map_err(RebootResumeRefusal::Sealed)?;
        let opened = open_sealed_payload(&opened_bytes)
            .ok_or(RebootResumeRefusal::Sealed(SealError::Unrecoverable))?;

        // Integrity: the identity sealed into the payload must be the one we
        // looked this record up by, or the at-rest record was swapped.
        if opened.ticket_id != *ticket_id {
            return Err(RebootResumeRefusal::Sealed(SealError::Unrecoverable));
        }
        // Expiry and target come from the sealed (authenticated) copy, never
        // the clear mirror.
        if now_unix_millis >= opened.not_after_unix_millis {
            return Err(RebootResumeRefusal::Expired);
        }
        if presented_target != &opened.target {
            return Err(RebootResumeRefusal::TargetMismatch);
        }

        if opened
            .key
            .verify(h_prime, ticket_id, nc_prime, presented_proof)
        {
            Ok(())
        } else {
            Err(RebootResumeRefusal::BadProof)
        }
    }

    /// Produces the returning agent's continuity proof for a challenge, consuming
    /// the ticket whatever the outcome — the agent-side mirror of
    /// [`redeem`](RebootResumeVault::redeem).
    ///
    /// After a reboot the console sends a fresh challenge nonce `Nc'`; the agent
    /// runs a fresh Noise XX handshake (yielding `h'`), then calls this to answer
    /// with `HMAC-SHA256(K_rr, h' ‖ ticket_id ‖ Nc')`. As with redemption the
    /// record is removed **before** any check, so the agent can never emit a
    /// proof for the same ticket twice (single use, REQ-S-012); the sealed
    /// (authenticated) copy supplies the expiry, so a stale ticket yields no
    /// proof; and a sealed blob that cannot be unsealed in this environment (a
    /// file lifted to another machine) fails closed here rather than producing a
    /// usable proof. Emitting a proof still restores no control: attended
    /// full-control consent runs afterward, and the console independently
    /// verifies target and proof on [`redeem`](RebootResumeVault::redeem).
    ///
    /// # Errors
    ///
    /// Returns the specific [`RebootResumeRefusal`] for local audit; callers
    /// present a single `NoConsentedSession` outcome to the peer.
    pub fn prove(
        &mut self,
        ticket_id: &TicketId,
        now_unix_millis: i64,
        h_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
        nc_prime: &[u8; REBOOT_RESUME_NONCE_LEN],
    ) -> Result<RebootResumeProof, RebootResumeRefusal> {
        // Single use: remove before any check so this id is spent regardless of
        // whether we go on to emit a proof (REQ-S-012).
        let ticket = self
            .records
            .remove(ticket_id)
            .ok_or(RebootResumeRefusal::UnknownTicket)?;

        // Recover K_rr and the authoritative fields from the sealed payload. A
        // copy taken to another environment cannot be unsealed, and a tampered
        // blob fails to parse — both fail closed here before any proof exists.
        let opened_bytes = self
            .sealer
            .unseal(ticket.sealed_key.as_bytes())
            .map_err(RebootResumeRefusal::Sealed)?;
        let opened = open_sealed_payload(&opened_bytes)
            .ok_or(RebootResumeRefusal::Sealed(SealError::Unrecoverable))?;

        // Integrity: the identity sealed into the payload must be the one we
        // looked this record up by, or the at-rest record was swapped.
        if opened.ticket_id != *ticket_id {
            return Err(RebootResumeRefusal::Sealed(SealError::Unrecoverable));
        }
        // Never answer a challenge for an expired ticket — expiry comes from the
        // sealed (authenticated) copy, never the clear mirror.
        if now_unix_millis >= opened.not_after_unix_millis {
            return Err(RebootResumeRefusal::Expired);
        }

        Ok(opened.key.proof(h_prime, ticket_id, nc_prime))
    }
}

#[cfg(test)]
mod tests;
