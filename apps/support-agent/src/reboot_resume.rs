//! Concrete agent-side reboot-resume armer and its persisted ticket store
//! (ADR-0014 §2/§4, ST-M006-001).
//!
//! This is the customer-edge adapter that turns the session runtime's injected
//! [`RebootResumeArmer`] seam into real
//! single-use, expiring, target-bound continuity tickets. On an attended local
//! arming gesture it:
//!
//! 1. draws a fresh ticket id and two nonces from the OS CSPRNG;
//! 2. derives `K_rr` from the armed session's Noise handshake hash `h` and those
//!    nonces via [`derive_reboot_resume_key`] — binding the secret to the exact
//!    consented session (ADR-0014 §2);
//! 3. seals `K_rr` at rest through an injected [`AtRestSealer`] (DPAPI on
//!    Windows, per-user — see `crate::dpapi`) inside a
//!    [`RebootResumeVault`]; and
//! 4. persists the sealed ticket to an owned-resource file on the customer
//!    machine ([`TicketStore`]) so exactly one ticket can survive the reboot.
//!
//! # Off by default, attended-only, no standing access
//!
//! The armer is reachable **only** through
//! [`SessionJoiner::join_with_reboot_resume`](cretlink_session::SessionJoiner::join_with_reboot_resume),
//! and the runtime calls [`arm`](RebootResumeArmer::arm) only inside a live,
//! consented session driven by the agent-local
//! [`arm_reboot_resume`](cretlink_session::JoinHandle::arm_reboot_resume) gesture
//! — never from the console or the MCP surface (REQ-S-001, REQ-S-003). Arming
//! mints a dormant secret; it creates no access path and never sets control
//! state, and a post-reboot reconnect still runs full attended consent
//! (REQ-S-007, REQ-S-012). Redeeming the ticket after the reboot, and listing or
//! recovering residual tickets, are the neighbouring stories ST-M006-002 and
//! ST-M006-003; this module only arms, persists, and disarms.
//!
//! The `VaultArmer` logic is portable and unit-tested on every host with an
//! in-memory ideal sealer; only the DPAPI sealer it is wired to in production is
//! Windows-gated, mirroring the agent's other `cfg(windows)` adapters.

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cretlink_protocol::{
    ArmedTicket, AtRestSealer, REBOOT_RESUME_NONCE_LEN, REBOOT_RESUME_TICKET_ID_LEN,
    RebootResumeVault, TargetBinding, TargetIdentity, TicketId, derive_reboot_resume_key,
};
use cretlink_session::{ArmOutcome, RebootResumeArmError, RebootResumeArmer, RebootResumeTicketId};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// The attended-local disclosure shown before arming reboot-resume, as plain,
/// screen-reader-friendly text (ADR-0014, REQ-N-014). It names the four facts
/// the customer must understand before consenting to arm: what it is for, that
/// it is **single-use** and **expires** (naming the lifetime), that it grants
/// **no control by itself**, and the **cleanup** that removes the stored ticket.
/// It carries no key material and is safe to display or log. The agent binary
/// prints exactly this before honoring the local `arm` gesture, so the disclosed
/// terms stay in lockstep with the armer's behaviour.
#[must_use]
pub fn reboot_resume_arming_disclosure(lifetime: Duration) -> String {
    let minutes = lifetime.as_secs() / 60;
    format!(
        "Arming reboot-resume for THIS session (attended):\n\
         - It lets the technician reconnect once after a single reboot without pairing from scratch.\n\
         - It is single-use and expires in {minutes} minutes.\n\
         - It grants NO control by itself: attended full-control consent runs again after the reboot.\n\
         - The stored ticket is sealed to this Windows user and removed once redeemed or expired."
    )
}

/// Reads a monotonic-enough wall clock as Unix milliseconds (UTC). Injected so
/// tests can pin the expiry the armer stamps.
type NowMillis = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Fills `buf` with cryptographically secure random bytes, or fails closed.
/// Injected so tests can drive deterministic ticket ids/nonces and exercise the
/// RNG-failure path.
type FillRandom = Arc<dyn Fn(&mut [u8]) -> Result<(), RebootResumeArmError> + Send + Sync>;

/// Format version of the ticket store, to guard a future forward migration
/// (ADR-0014 §6).
const STORE_FORMAT_VERSION: u32 = 1;

/// The persisted set of armed reboot-resume tickets, written atomically to a
/// well-known file on the customer machine (ADR-0014 §6, ST-M006-001).
///
/// Only the sealed ticket bytes and their clear mirrors (id, target fingerprint,
/// expiry) rest here — never `K_rr` in the clear, which the sealer protects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredTickets {
    /// On-disk format version for a forward-compatible read (ST-M006-003).
    version: u32,
    /// The currently-armed tickets.
    tickets: Vec<ArmedTicket>,
}

/// An error reading or writing the reboot-resume ticket store.
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// The store file could not be read, written, or replaced.
    Io(std::io::Error),
    /// The store file exists but is not valid inventory JSON.
    Format(serde_json::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "reboot-resume ticket store I/O failed: {err}"),
            Self::Format(err) => write!(f, "reboot-resume ticket store is malformed: {err}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Format(err) => Some(err),
        }
    }
}

/// A file-backed store for armed reboot-resume tickets on the customer machine.
///
/// The store is deliberately tiny and standalone so it can be unit-tested and,
/// in ST-M006-003, folded into the general owned-resource inventory and its
/// recovery CLI verb. Writes go through a temp file and an atomic rename so a
/// crash mid-write never leaves a half-written inventory (REQ-F-019).
#[derive(Debug, Clone)]
pub struct TicketStore {
    path: PathBuf,
}

impl TicketStore {
    /// Binds a store to an explicit inventory file path.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The inventory file path this store owns.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Loads the persisted tickets, treating a missing file as an empty
    /// inventory (nothing was ever armed, or it was fully cleaned up).
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file exists but cannot be read, or
    /// [`StoreError::Format`] if its contents are not valid inventory JSON.
    pub fn load(&self) -> Result<Vec<ArmedTicket>, StoreError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let doc: StoredTickets =
                    serde_json::from_slice(&bytes).map_err(StoreError::Format)?;
                Ok(doc.tickets)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(StoreError::Io(err)),
        }
    }

    /// Writes the given tickets, replacing any prior inventory atomically.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the parent directory, temp file, or rename fails,
    /// or [`StoreError::Format`] if serialization fails.
    pub fn save(&self, tickets: &[ArmedTicket]) -> Result<(), StoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(StoreError::Io)?;
        }
        let doc = StoredTickets {
            version: STORE_FORMAT_VERSION,
            tickets: tickets.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&doc).map_err(StoreError::Format)?;
        // Write to a sibling temp file then rename over the target. `std::fs`
        // maps this to a replacing move on Windows and a POSIX rename elsewhere,
        // so a reader never sees a partially-written inventory.
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(StoreError::Io)?;
        fs::rename(&tmp, &self.path).map_err(StoreError::Io)?;
        Ok(())
    }

    /// Removes the ticket store file entirely, so the recovery verb can clear the
    /// reboot-resume residue without hiding evidence (ST-M006-003 AC3): the
    /// caller reads the tickets first, then this deletes the sealed inventory.
    ///
    /// Idempotent — an already-absent store is `Ok(false)` (nothing to remove),
    /// a store that was present and is now gone is `Ok(true)`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file exists but cannot be removed; the caller
    /// keeps the residue in its report and names safe manual remediation rather
    /// than claiming a clean removal (REQ-N-008).
    pub fn remove(&self) -> Result<bool, StoreError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(StoreError::Io(err)),
        }
    }
}

/// The default well-known reboot-resume ticket store path on the customer
/// machine: `%ProgramData%\CretLink\reboot-resume\tickets.json` on Windows (a
/// machine-wide, reboot-surviving location mirroring the exec staging base), or
/// a `cretlink/reboot-resume` folder under the system temp directory elsewhere.
#[must_use]
pub fn default_ticket_store_path() -> PathBuf {
    let dir = if let Some(program_data) = std::env::var_os("ProgramData") {
        PathBuf::from(program_data)
            .join("CretLink")
            .join("reboot-resume")
    } else {
        std::env::temp_dir().join("cretlink").join("reboot-resume")
    };
    dir.join("tickets.json")
}

/// A concrete [`RebootResumeArmer`] backed by a [`RebootResumeVault`] and a
/// file-backed [`TicketStore`] (ADR-0014, ST-M006-001).
///
/// Generic over the [`AtRestSealer`] so the same arming/persistence logic is
/// exercised in tests with an in-memory ideal sealer and driven in production by
/// the Windows DPAPI sealer.
pub struct VaultArmer<S: AtRestSealer> {
    vault: Mutex<RebootResumeVault<S>>,
    store: TicketStore,
    now_millis: NowMillis,
    fill_random: FillRandom,
}

impl<S: AtRestSealer> fmt::Debug for VaultArmer<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render key material or ticket contents — only the owned store.
        f.debug_struct("VaultArmer")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl<S: AtRestSealer> VaultArmer<S> {
    /// Builds an armer over `sealer` and `store`, rehydrating any tickets a
    /// prior run persisted so the in-memory vault and the on-disk inventory stay
    /// consistent across restarts. A malformed store is treated as empty rather
    /// than blocking a new session; ST-M006-003 owns surfacing such residue.
    ///
    /// Uses the real OS clock and CSPRNG.
    #[must_use]
    pub fn new(sealer: S, store: TicketStore) -> Self {
        let tickets = store.load().unwrap_or_default();
        Self {
            vault: Mutex::new(RebootResumeVault::restore(sealer, tickets)),
            store,
            now_millis: Arc::new(default_now_millis),
            fill_random: Arc::new(default_fill_random),
        }
    }

    /// The number of tickets currently armed (for honest local reporting).
    #[must_use]
    pub fn armed_count(&self) -> usize {
        self.vault.lock().map(|v| v.armed().count()).unwrap_or(0)
    }

    /// Rewrites the on-disk inventory from the vault's current armed set.
    fn persist_locked(&self, vault: &RebootResumeVault<S>) -> Result<(), RebootResumeArmError> {
        let tickets: Vec<ArmedTicket> = vault.armed().cloned().collect();
        self.store
            .save(&tickets)
            .map_err(|_| RebootResumeArmError::Backend)
    }
}

// `RebootResumeArmer` is `Send + Sync`; the vault sealer must therefore be
// shareable across threads (the session runtime holds the armer behind an `Arc`
// and arms from its own task). `DpapiSealer` and the test sealer both satisfy
// this trivially.
impl<S: AtRestSealer + Send + Sync> RebootResumeArmer for VaultArmer<S> {
    fn arm(
        &self,
        handshake_hash: &[u8; 32],
        target: &TargetIdentity,
        lifetime: Duration,
    ) -> Result<ArmOutcome, RebootResumeArmError> {
        // Fresh single-use material from the OS CSPRNG. The nonces are transient
        // derivation inputs only — zeroized as they leave scope.
        let mut ticket_bytes = [0u8; REBOOT_RESUME_TICKET_ID_LEN];
        let mut na_rr = Zeroizing::new([0u8; REBOOT_RESUME_NONCE_LEN]);
        let mut nc_rr = Zeroizing::new([0u8; REBOOT_RESUME_NONCE_LEN]);
        (self.fill_random)(&mut ticket_bytes)?;
        (self.fill_random)(na_rr.as_mut())?;
        (self.fill_random)(nc_rr.as_mut())?;

        // Bind K_rr to the exact consented session transcript (h) and this
        // arming round's nonces (ADR-0014 §2). The agent contributes both nonces
        // over the already-authenticated channel; the console-side derivation and
        // the post-reboot proof belong to ST-M006-002.
        let key = derive_reboot_resume_key(handshake_hash, &na_rr, &nc_rr);

        // Expiry from the real clock; a lifetime beyond i64 milliseconds
        // saturates rather than wrapping into the past.
        let now = (self.now_millis)();
        let lifetime_millis = i64::try_from(lifetime.as_millis()).unwrap_or(i64::MAX);
        let not_after_unix_millis = now.saturating_add(lifetime_millis);

        let ticket_id = TicketId(ticket_bytes);
        let binding = TargetBinding::of(target);

        let mut vault = self
            .vault
            .lock()
            .map_err(|_| RebootResumeArmError::Backend)?;
        vault
            .arm(ticket_id, &key, binding, not_after_unix_millis)
            .map_err(|_| RebootResumeArmError::Backend)?;
        // Persist before reporting success: a ticket the caller believes is armed
        // must already be on disk to survive the reboot. On a write failure roll
        // the in-memory state back so the vault never claims more than the
        // inventory holds.
        if let Err(err) = self.persist_locked(&vault) {
            vault.disarm(&ticket_id);
            return Err(err);
        }

        Ok(ArmOutcome {
            ticket_id: ticket_bytes,
            not_after_unix_millis,
        })
    }

    fn disarm(&self, ticket_id: &RebootResumeTicketId) -> bool {
        let mut vault = match self.vault.lock() {
            Ok(vault) => vault,
            Err(_) => return false,
        };
        let removed = vault.disarm(&TicketId(*ticket_id));
        // Best-effort inventory rewrite. If it fails the record is gone from the
        // live vault but may linger on disk; it stays single-use, target-bound,
        // expiring, and never restores control, and ST-M006-003's inventory sweep
        // reclaims such residue honestly.
        let _ = self.persist_locked(&vault);
        removed
    }
}

/// Real wall clock as Unix milliseconds (UTC), clamped fail-safe: a clock before
/// the epoch or beyond `i64` milliseconds yields a value that only ever shortens
/// a ticket's life, never extends it.
fn default_now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Fills `buf` from the OS CSPRNG, matching the transport pairing nonce source
/// (`rand::rngs::OsRng`). A backend failure fails closed as
/// [`RebootResumeArmError::Backend`] rather than falling back to weaker entropy.
fn default_fill_random(buf: &mut [u8]) -> Result<(), RebootResumeArmError> {
    use rand::TryRngCore;
    use rand::rngs::OsRng;

    OsRng
        .try_fill_bytes(buf)
        .map_err(|_| RebootResumeArmError::Backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cretlink_protocol::SealError;
    use std::sync::atomic::{AtomicU64, Ordering};
    use zeroize::Zeroizing as Z;

    /// A reversible in-memory sealer standing in for DPAPI in tests: it obscures
    /// the plaintext (so a test never asserts on cleartext key bytes) yet round
    /// trips, and rejects tampered input — matching the [`AtRestSealer`]
    /// contract without touching the platform.
    #[derive(Debug)]
    struct IdealSealer;

    const SEAL_MASK: u8 = 0x5a;

    impl AtRestSealer for IdealSealer {
        fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
            let mut out = Vec::with_capacity(plaintext.len() + 1);
            // A one-byte authenticator so a flipped byte is detected on unseal.
            let check = plaintext.iter().fold(0u8, |a, b| a.wrapping_add(*b));
            out.push(check);
            out.extend(plaintext.iter().map(|b| b ^ SEAL_MASK));
            Ok(out)
        }

        fn unseal(&self, sealed: &[u8]) -> Result<Z<Vec<u8>>, SealError> {
            let (check, body) = sealed.split_first().ok_or(SealError::Unrecoverable)?;
            let plain: Vec<u8> = body.iter().map(|b| b ^ SEAL_MASK).collect();
            let recomputed = plain.iter().fold(0u8, |a, b| a.wrapping_add(*b));
            if recomputed != *check {
                return Err(SealError::Unrecoverable);
            }
            Ok(Z::new(plain))
        }
    }

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A unique temp inventory path plus the directory to clean up afterwards.
    fn temp_store() -> (TicketStore, PathBuf) {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("cretlink-rr-{}-{seq}", std::process::id()));
        let path = dir.join("tickets.json");
        (TicketStore::new(path), dir)
    }

    fn target() -> TargetIdentity {
        TargetIdentity {
            hostname: "CUSTOMER-PC".to_string(),
            os_version: "Windows 11 Pro 10.0.26200".to_string(),
            arch: "x86_64".to_string(),
            elevated: true,
            simulated: false,
            agent_version: "0.0.0".to_string(),
        }
    }

    /// A clock fixed at `now`.
    fn fixed_clock(now: i64) -> NowMillis {
        Arc::new(move || now)
    }

    /// A deterministic filler: each call writes an incrementing byte pattern so
    /// successive draws (ticket id, na, nc) differ but are reproducible.
    fn counter_fill() -> FillRandom {
        let counter = Arc::new(AtomicU64::new(1));
        Arc::new(move |buf: &mut [u8]| {
            let base = counter.fetch_add(1, Ordering::Relaxed);
            for (i, slot) in buf.iter_mut().enumerate() {
                *slot = (base as u8).wrapping_add(i as u8);
            }
            Ok(())
        })
    }

    fn test_armer(store: TicketStore, now: i64) -> VaultArmer<IdealSealer> {
        VaultArmer {
            vault: Mutex::new(RebootResumeVault::new(IdealSealer)),
            store,
            now_millis: fixed_clock(now),
            fill_random: counter_fill(),
        }
    }

    #[test]
    fn arm_persists_single_ticket_and_reports_expiry() {
        let (store, dir) = temp_store();
        let armer = test_armer(store.clone(), 1_000_000);

        let outcome = armer
            .arm(&[9u8; 32], &target(), Duration::from_secs(600))
            .expect("arm succeeds with an ideal sealer");

        // Expiry is now + lifetime, in Unix milliseconds.
        assert_eq!(outcome.not_after_unix_millis, 1_000_000 + 600_000);
        assert_eq!(armer.armed_count(), 1);

        // The single ticket is on disk with matching id, target, and expiry.
        let persisted = store.load().expect("inventory loads");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].ticket_id.0, outcome.ticket_id);
        assert_eq!(
            persisted[0].not_after_unix_millis,
            outcome.not_after_unix_millis
        );
        assert_eq!(persisted[0].target, TargetBinding::of(&target()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_material_is_bound_to_the_handshake_hash() {
        // Two arms with identical nonces but different session transcripts must
        // seal different key material — proving K_rr is derived from h.
        let (store_a, dir_a) = temp_store();
        let (store_b, dir_b) = temp_store();
        let armer_a = test_armer(store_a.clone(), 1_000_000);
        let armer_b = test_armer(store_b.clone(), 1_000_000);

        armer_a
            .arm(&[1u8; 32], &target(), Duration::from_secs(60))
            .expect("arm a");
        armer_b
            .arm(&[2u8; 32], &target(), Duration::from_secs(60))
            .expect("arm b");

        let a = store_a.load().expect("load a");
        let b = store_b.load().expect("load b");
        // Same deterministic nonces and ticket id, different h → different seal.
        assert_eq!(a[0].ticket_id, b[0].ticket_id);
        assert_ne!(a[0].sealed_key.as_bytes(), b[0].sealed_key.as_bytes());

        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn disarm_removes_the_persisted_ticket() {
        let (store, dir) = temp_store();
        let armer = test_armer(store.clone(), 5_000);

        let outcome = armer
            .arm(&[7u8; 32], &target(), Duration::from_secs(30))
            .expect("arm");
        assert_eq!(store.load().expect("load").len(), 1);

        assert!(armer.disarm(&outcome.ticket_id), "a present ticket disarms");
        assert_eq!(armer.armed_count(), 0);
        assert!(
            store.load().expect("load").is_empty(),
            "inventory is emptied"
        );

        // A second disarm finds nothing (single use / arm-then-cancel).
        assert!(!armer.disarm(&outcome.ticket_id));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn armed_ticket_survives_a_restart() {
        let (store, dir) = temp_store();
        {
            let armer = test_armer(store.clone(), 2_000);
            armer
                .arm(&[3u8; 32], &target(), Duration::from_secs(120))
                .expect("arm");
        }
        // A fresh armer over the same store rehydrates the persisted ticket —
        // the foundation for surviving a reboot (redemption is ST-M006-002).
        let reloaded = VaultArmer::new(IdealSealer, store);
        assert_eq!(reloaded.armed_count(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rng_failure_fails_closed_and_arms_nothing() {
        let (store, dir) = temp_store();
        let armer = VaultArmer {
            vault: Mutex::new(RebootResumeVault::new(IdealSealer)),
            store: store.clone(),
            now_millis: fixed_clock(0),
            fill_random: Arc::new(|_| Err(RebootResumeArmError::Backend)),
        };

        let result = armer.arm(&[0u8; 32], &target(), Duration::from_secs(60));
        assert_eq!(result, Err(RebootResumeArmError::Backend));
        assert_eq!(armer.armed_count(), 0);
        assert!(store.load().expect("load").is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_missing_file_loads_empty_then_round_trips() {
        let (store, dir) = temp_store();
        assert!(store.load().expect("missing loads empty").is_empty());

        let armer = test_armer(store.clone(), 10);
        armer
            .arm(&[4u8; 32], &target(), Duration::from_secs(1))
            .expect("arm");

        let again = TicketStore::new(store.path().to_path_buf());
        assert_eq!(again.load().expect("round trip").len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn arming_disclosure_names_expiry_single_use_no_control_and_cleanup() {
        // The attended-local disclosure must name every fact ADR-0014 requires
        // before the customer arms (ST-M006-001 verification): the concrete
        // expiry, the single-use nature, that it grants no control by itself,
        // and the cleanup that follows. This guards the disclosed terms against
        // silent drift from the armer's behaviour.
        let text = reboot_resume_arming_disclosure(Duration::from_secs(15 * 60));
        assert!(
            text.contains("15 minutes"),
            "names the concrete expiry: {text}"
        );
        assert!(text.contains("single-use"), "names single-use: {text}");
        assert!(
            text.contains("NO control by itself"),
            "states it grants no control by itself: {text}"
        );
        assert!(
            text.contains("removed once redeemed or expired"),
            "names the cleanup that follows: {text}"
        );
    }
}
