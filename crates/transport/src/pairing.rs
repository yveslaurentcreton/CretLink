//! Noise `XX` pairing and the commit-reveal Session Approval Code round
//! (ST-M002-004, ADR-0003).
//!
//! [`Pairing::perform`] establishes an authenticated, confidential
//! [`SecureChannel`] between two previously unrelated LAN endpoints with no
//! PKI, accounts, or pre-shared secret. It composes established constructions
//! and invents no cryptography (REQ-S-005):
//!
//! 1. A Noise `Noise_XX_25519_ChaChaPoly_SHA256` handshake via `snow`. Both
//!    endpoints generate a **fresh** X25519 static keypair for the session
//!    (plus the ephemerals `snow` creates); no key material is persisted or
//!    reused (REQ-S-007). The support agent is the initiator and dials the
//!    console (the responder).
//! 2. A frozen [prologue](cretlink_protocol::pairing_prologue) committing both
//!    peers to the same protocol version and capability set. A mismatch — a
//!    silent downgrade — changes the prologue on one side and makes the
//!    handshake fail closed (REQ-S-008).
//! 3. A commit-reveal round, run **inside** the encrypted transport, that binds
//!    the Noise handshake hash together with two fresh random nonces into the
//!    human-comparable [`SessionApprovalCode`]
//!    ([`derive_session_approval_code`]). Because the handshake hash differs on
//!    each side of a man-in-the-middle, the two humans see different codes and
//!    the comparison catches the attack (REQ-S-006).
//!
//! The approval-code derivation and the pairing message schema live in
//! `cretlink-protocol`; this module supplies the handshake, the randomness, the
//! constant-time commitment check, and the fail-closed sequencing.

use core::fmt;
use cretlink_protocol::{
    B64Bytes, Message, PairingMessage, ProtocolError, ProtocolVersion, SAC_COMMITMENT_LEN,
    SAC_NONCE_LEN, SessionApprovalCode, derive_session_approval_code, limits, pairing_prologue,
    sac_commitment,
};
use rand::TryRngCore;
use rand::rngs::OsRng;
use snow::Builder;
use snow::params::NoiseParams;
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::frame::{FrameError, read_frame, write_frame};
use crate::secure::{NOISE_MAX_MESSAGE_LEN, SecureChannel, SecureChannelError};

/// The Noise handshake pattern and crypto suite (ADR-0003). A compile-time
/// constant; parsing it can only fail through a programming error.
const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Which side of the Noise `XX` handshake an endpoint drives.
///
/// The support agent is always the [`Role::Initiator`] (it dials, minimizing
/// customer-side inbound firewall changes); the console is the
/// [`Role::Responder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Sends the first handshake message and the commit; the support agent.
    Initiator,
    /// Receives the first handshake message; the console.
    Responder,
}

/// The outcome of a successful [`Pairing::perform`].
pub struct Paired<T> {
    /// The authenticated, confidential channel for all subsequent messages.
    pub channel: SecureChannel<T>,
    /// The six-digit code both humans compare before granting full control
    /// (REQ-F-005, REQ-S-006).
    pub approval_code: SessionApprovalCode,
    /// The peer's freshly generated X25519 static public key for this session.
    /// It is not a durable identity: a fresh key is minted every session
    /// (REQ-S-007), so it is useful only for in-session correlation.
    pub peer_static: [u8; 32],
    /// The 32-byte Noise transcript-binding handshake hash `h`, identical on
    /// both untampered endpoints and different across a man-in-the-middle
    /// (REQ-S-006). The Session Approval Code already binds to it; it is
    /// surfaced here so a consented session can additionally derive the
    /// reboot-resume continuity key `K_rr = HKDF(h, ...)` from the same
    /// transcript root without a second handshake (ADR-0014, ST-M006-001). It
    /// is not the approval code and carries no standing authority; it is
    /// zeroized on drop and kept out of `Debug` so it cannot leak into logs.
    pub handshake_hash: Zeroizing<[u8; 32]>,
}

impl<T> fmt::Debug for Paired<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The approval code is deliberately excluded from Debug so it cannot
        // leak into logs (REQ-S-018); it is compared on screen only. The
        // handshake hash is reboot-resume key material (ADR-0014) and is
        // redacted for the same reason.
        f.debug_struct("Paired")
            .field("approval_code", &"<redacted>")
            .field("peer_static", &self.peer_static)
            .field("handshake_hash", &"<redacted>")
            .field("channel", &self.channel)
            .finish()
    }
}

/// Entry point for establishing a secure session (ADR-0003).
#[derive(Debug, Clone, Copy, Default)]
pub struct Pairing;

impl Pairing {
    /// Runs the full pairing over `io`: the Noise `XX` handshake, the
    /// transition into transport mode, and the commit-reveal Session Approval
    /// Code round.
    ///
    /// On success both endpoints hold a live [`SecureChannel`] and, on an
    /// untampered channel, the **same** [`SessionApprovalCode`]. Any failure —
    /// a handshake fault (including a downgrade), a size-bound violation, a
    /// commitment mismatch, an I/O error, or an EOF — returns an `Err` and
    /// yields no channel and no code (fail-closed, REQ-S-009).
    ///
    /// `version` and `capabilities` are hashed into the handshake prologue;
    /// both peers must supply the same set or the handshake fails (REQ-S-008).
    pub async fn perform<T: AsyncRead + AsyncWrite + Unpin>(
        io: T,
        role: Role,
        version: ProtocolVersion,
        capabilities: &[String],
    ) -> Result<Paired<T>, PairingError> {
        let (mut channel, handshake_hash, peer_static) =
            establish_channel(io, role, version, capabilities).await?;
        let approval_code = run_commit_reveal(&mut channel, role, &handshake_hash).await?;

        // Retain `h` for the caller instead of dropping it: a consented session
        // derives the reboot-resume continuity key from this same transcript
        // root (ADR-0014). The value is already zeroize-on-drop.
        Ok(Paired {
            channel,
            approval_code,
            peer_static,
            handshake_hash,
        })
    }
}

/// Runs the capability guard and the Noise `XX` handshake, returning a live
/// [`SecureChannel`] together with the transcript-binding handshake hash and
/// the peer's fresh static key. Split out from [`Pairing::perform`] so the
/// commit-reveal round can be exercised directly by adversarial tests.
async fn establish_channel<T: AsyncRead + AsyncWrite + Unpin>(
    mut io: T,
    role: Role,
    version: ProtocolVersion,
    capabilities: &[String],
) -> Result<(SecureChannel<T>, Zeroizing<[u8; 32]>, [u8; 32]), PairingError> {
    // Refuse to start if our own advertised capabilities exceed protocol
    // limits: the prologue builder would silently clamp them, producing a
    // prologue that misrepresents what we intend to offer — a self-inflicted
    // downgrade. Fail closed instead (REQ-S-008).
    if capabilities.len() > limits::MAX_CAPABILITIES
        || capabilities
            .iter()
            .any(|capability| capability.len() > limits::MAX_CAPABILITY_LEN)
    {
        return Err(PairingError::VersionCapabilityMismatch);
    }

    let prologue = pairing_prologue(version, capabilities);
    let params: NoiseParams = NOISE_PARAMS
        .parse()
        .map_err(PairingError::HandshakeFailed)?;

    // A fresh static keypair for this session only. The private half is copied
    // into the handshake state, then scrubbed from our local copy.
    let builder = Builder::new(params)
        .prologue(&prologue)
        .map_err(PairingError::HandshakeFailed)?;
    let mut keypair = builder
        .generate_keypair()
        .map_err(PairingError::HandshakeFailed)?;
    let builder = builder
        .local_private_key(&keypair.private)
        .map_err(PairingError::HandshakeFailed)?;
    let mut handshake = match role {
        Role::Initiator => builder.build_initiator(),
        Role::Responder => builder.build_responder(),
    }
    .map_err(PairingError::HandshakeFailed)?;
    keypair.private.zeroize();

    // Noise XX message flow: -> e ; <- e,ee,s,es ; -> s,se. Every read is
    // bounded by MAX_FRAME_BYTES before allocation (see the `frame` module).
    let mut scratch = vec![0u8; NOISE_MAX_MESSAGE_LEN];
    match role {
        Role::Initiator => {
            write_handshake(&mut handshake, &mut io, &mut scratch).await?;
            read_handshake(&mut handshake, &mut io).await?;
            write_handshake(&mut handshake, &mut io, &mut scratch).await?;
        }
        Role::Responder => {
            read_handshake(&mut handshake, &mut io).await?;
            write_handshake(&mut handshake, &mut io, &mut scratch).await?;
            read_handshake(&mut handshake, &mut io).await?;
        }
    }

    // Capture the transcript-binding handshake hash before consuming the
    // handshake state. For SHA-256 it is always 32 bytes.
    let handshake_hash: Zeroizing<[u8; 32]> = Zeroizing::new(
        handshake
            .get_handshake_hash()
            .try_into()
            .map_err(|_| PairingError::Internal("handshake hash was not 32 bytes"))?,
    );

    // into_transport_mode re-validates that the handshake finished; a premature
    // transition surfaces as a HandshakeFailed here.
    let transport = handshake
        .into_transport_mode()
        .map_err(PairingError::HandshakeFailed)?;
    let peer_static: [u8; 32] = transport
        .get_remote_static()
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .ok_or(PairingError::Internal(
            "missing remote static key after XX handshake",
        ))?;

    Ok((
        SecureChannel::new(transport, io),
        handshake_hash,
        peer_static,
    ))
}

/// A 32-byte secret pairing nonce, erased on drop (REQ-S-007). The initiator's
/// `na` must stay secret until the reveal. This holds the locally generated
/// nonce; the peer's received nonce is wrapped in [`Zeroizing`] at the call
/// site, so both nonces are scrubbed once the approval code is derived.
#[derive(Zeroize, ZeroizeOnDrop)]
struct SecretNonce([u8; SAC_NONCE_LEN]);

impl SecretNonce {
    /// Draws a fresh nonce from the operating-system CSPRNG.
    fn random() -> Result<Self, PairingError> {
        let mut bytes = [0u8; SAC_NONCE_LEN];
        let mut rng = OsRng;
        rng.try_fill_bytes(&mut bytes)
            .map_err(|_| PairingError::Rng)?;
        Ok(Self(bytes))
    }
}

/// Writes one Noise handshake message as a length-prefixed frame.
async fn write_handshake<T: AsyncWrite + Unpin>(
    handshake: &mut snow::HandshakeState,
    io: &mut T,
    scratch: &mut [u8],
) -> Result<(), PairingError> {
    let n = handshake
        .write_message(&[], scratch)
        .map_err(PairingError::HandshakeFailed)?;
    write_frame(io, &scratch[..n], limits::MAX_FRAME_BYTES)
        .await
        .map_err(PairingError::from_frame)
}

/// Reads one Noise handshake message from a length-prefixed frame. The
/// (empty) payload is discarded; only the handshake state transition matters.
async fn read_handshake<T: AsyncRead + Unpin>(
    handshake: &mut snow::HandshakeState,
    io: &mut T,
) -> Result<(), PairingError> {
    let message = read_frame(io, limits::MAX_FRAME_BYTES)
        .await
        .map_err(PairingError::from_frame)?;
    // The plaintext is never longer than the ciphertext frame, so the frame
    // length is a safe output-buffer size.
    let mut payload = vec![0u8; message.len()];
    handshake
        .read_message(&message, &mut payload)
        .map_err(PairingError::HandshakeFailed)?;
    Ok(())
}

/// Runs the commit-reveal Session Approval Code round over the encrypted
/// transport (ADR-0003). Ordering is enforced strictly: any out-of-sequence or
/// malformed pairing message, or a reveal that fails the constant-time
/// commitment check, fails closed as [`PairingError::CommitmentMismatch`].
async fn run_commit_reveal<T: AsyncRead + AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    role: Role,
    handshake_hash: &[u8; 32],
) -> Result<SessionApprovalCode, PairingError> {
    match role {
        Role::Initiator => {
            let na = SecretNonce::random()?;
            let commitment = sac_commitment(&na.0);
            send_pairing(
                channel,
                PairingMessage::Commit {
                    commitment: B64Bytes::from(commitment.to_vec()),
                },
            )
            .await?;

            // Wrap the peer's nonce so it is scrubbed on drop like our own `na`.
            let nc = Zeroizing::new(expect_nonce(recv_pairing(channel).await?)?);

            send_pairing(
                channel,
                PairingMessage::Reveal {
                    na: B64Bytes::from(na.0.to_vec()),
                },
            )
            .await?;

            Ok(derive_session_approval_code(handshake_hash, &na.0, &nc))
        }
        Role::Responder => {
            // The first pairing message received MUST be the commit; anything
            // else is an ordering violation and fails closed.
            let commitment = expect_commit(recv_pairing(channel).await?)?;

            let nc = SecretNonce::random()?;
            send_pairing(
                channel,
                PairingMessage::Nonce {
                    nc: B64Bytes::from(nc.0.to_vec()),
                },
            )
            .await?;

            // Wrap the peer's nonce so it is scrubbed on drop like our own `nc`.
            let na = Zeroizing::new(expect_reveal(recv_pairing(channel).await?)?);

            // Verify the reveal against the commitment in constant time before
            // deriving any code (REQ-S-006).
            let recomputed = sac_commitment(&na);
            if !bool::from(recomputed[..].ct_eq(&commitment[..])) {
                return Err(PairingError::CommitmentMismatch);
            }

            Ok(derive_session_approval_code(handshake_hash, &na, &nc.0))
        }
    }
}

/// Sends one pairing message over the encrypted transport.
async fn send_pairing<T: AsyncWrite + Unpin>(
    channel: &mut SecureChannel<T>,
    pairing: PairingMessage,
) -> Result<(), PairingError> {
    channel
        .send(&Message::Pairing { pairing })
        .await
        .map_err(PairingError::Channel)
}

/// Receives one message and requires it to be a pairing message.
async fn recv_pairing<T: AsyncRead + Unpin>(
    channel: &mut SecureChannel<T>,
) -> Result<PairingMessage, PairingError> {
    match channel.recv().await.map_err(PairingError::Channel)? {
        Message::Pairing { pairing } => Ok(pairing),
        // A non-pairing message during the SAC round is a protocol violation;
        // fail closed like any other commit-reveal deviation.
        _ => Err(PairingError::CommitmentMismatch),
    }
}

/// Extracts the 32-byte commitment from an expected [`PairingMessage::Commit`].
fn expect_commit(pairing: PairingMessage) -> Result<[u8; SAC_COMMITMENT_LEN], PairingError> {
    match pairing {
        PairingMessage::Commit { commitment } => {
            fixed_bytes(&commitment).ok_or(PairingError::CommitmentMismatch)
        }
        _ => Err(PairingError::CommitmentMismatch),
    }
}

/// Extracts the responder's 32-byte nonce from an expected [`PairingMessage::Nonce`].
fn expect_nonce(pairing: PairingMessage) -> Result<[u8; SAC_NONCE_LEN], PairingError> {
    match pairing {
        PairingMessage::Nonce { nc } => fixed_bytes(&nc).ok_or(PairingError::CommitmentMismatch),
        _ => Err(PairingError::CommitmentMismatch),
    }
}

/// Extracts the initiator's revealed 32-byte nonce from an expected
/// [`PairingMessage::Reveal`].
fn expect_reveal(pairing: PairingMessage) -> Result<[u8; SAC_NONCE_LEN], PairingError> {
    match pairing {
        PairingMessage::Reveal { na } => fixed_bytes(&na).ok_or(PairingError::CommitmentMismatch),
        _ => Err(PairingError::CommitmentMismatch),
    }
}

/// Converts a base64 field to a fixed 32-byte array, rejecting any other length.
fn fixed_bytes(bytes: &B64Bytes) -> Option<[u8; SAC_NONCE_LEN]> {
    <[u8; SAC_NONCE_LEN]>::try_from(bytes.as_bytes()).ok()
}

/// A pairing failure. Every variant denies the session (fail-closed,
/// REQ-S-009): a failed [`Pairing::perform`] yields no channel and no code.
#[derive(Debug, Error)]
pub enum PairingError {
    /// The Noise handshake failed. This is also how a **downgrade** surfaces:
    /// a mismatched prologue (a different protocol version or capability set)
    /// makes the authenticated handshake fail with a decryption error that is
    /// cryptographically indistinguishable from any other handshake fault, so
    /// it is reported here rather than as a distinct variant (REQ-S-008).
    #[error("noise handshake failed")]
    HandshakeFailed(#[source] snow::Error),

    /// A locally offered capability set or version cannot be faithfully
    /// represented in the prologue (it exceeds protocol limits), so pairing is
    /// refused before it starts rather than silently downgraded.
    #[error("protocol version or capability set could not be represented (possible downgrade)")]
    VersionCapabilityMismatch,

    /// The commit-reveal round did not complete as specified: a message arrived
    /// out of order, a pairing field had the wrong length, or the revealed
    /// nonce did not match the commitment. Fails closed (REQ-S-006).
    #[error("session approval commitment did not match the reveal")]
    CommitmentMismatch,

    /// A frame exceeded its size bound, detected before allocation.
    #[error("pairing frame of {len} bytes exceeds the {max}-byte limit")]
    Oversized {
        /// The declared or offered length in bytes.
        len: usize,
        /// The enforced ceiling in bytes.
        max: usize,
    },

    /// The operating-system CSPRNG failed to produce a pairing nonce.
    #[error("failed to draw a random pairing nonce")]
    Rng,

    /// An I/O failure on the underlying stream during pairing.
    #[error("pairing I/O error")]
    Io(#[source] std::io::Error),

    /// A pairing message failed to decode or validate.
    #[error("pairing message decode failed")]
    Decode(#[source] ProtocolError),

    /// The peer closed the stream during pairing.
    #[error("connection closed during pairing")]
    Eof,

    /// A failure on the encrypted transport while running the commit-reveal
    /// round (for example the channel was poisoned by a tampered frame).
    #[error("secure channel failed during pairing")]
    Channel(#[source] SecureChannelError),

    /// An internal invariant was violated. Unreachable on the Noise `XX`
    /// completion path; present so no reachable path panics.
    #[error("internal pairing invariant violated: {0}")]
    Internal(&'static str),
}

impl PairingError {
    fn from_frame(error: FrameError) -> Self {
        match error {
            FrameError::Io(io) => Self::Io(io),
            FrameError::Oversized { len, max } => Self::Oversized { len, max },
            FrameError::Eof => Self::Eof,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cretlink_protocol::PROTOCOL_V1;
    use tokio::io::{DuplexStream, duplex};

    // Compile-time proof that the secret nonce holder erases its bytes on drop
    // (REQ-S-007). If `SecretNonce` ever stops being `ZeroizeOnDrop` this fails
    // to compile.
    fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}

    #[test]
    fn secret_nonce_is_zeroize_on_drop() {
        assert_zeroize_on_drop::<SecretNonce>();
    }

    #[test]
    fn fixed_bytes_rejects_wrong_length() {
        assert!(fixed_bytes(&B64Bytes::from(vec![0u8; 31])).is_none());
        assert!(fixed_bytes(&B64Bytes::from(vec![0u8; 33])).is_none());
        assert!(fixed_bytes(&B64Bytes::from(vec![0u8; 32])).is_some());
    }

    #[test]
    fn expect_helpers_reject_wrong_variant() {
        let nonce = PairingMessage::Nonce {
            nc: B64Bytes::from(vec![0u8; 32]),
        };
        assert!(matches!(
            expect_commit(nonce),
            Err(PairingError::CommitmentMismatch)
        ));
        let commit = PairingMessage::Commit {
            commitment: B64Bytes::from(vec![0u8; 32]),
        };
        assert!(matches!(
            expect_reveal(commit),
            Err(PairingError::CommitmentMismatch)
        ));
    }

    /// Establishes two channels over an in-memory duplex by running only the
    /// handshake (not the commit-reveal round), so a test can drive the SAC
    /// round manually — including as a cheating initiator.
    async fn handshake_pair() -> (
        (SecureChannel<DuplexStream>, Zeroizing<[u8; 32]>),
        (SecureChannel<DuplexStream>, Zeroizing<[u8; 32]>),
    ) {
        let (a, b) = duplex(4096);
        let caps = vec!["exec".to_string()];
        let (initiator, responder) = tokio::join!(
            establish_channel(a, Role::Initiator, PROTOCOL_V1, &caps),
            establish_channel(b, Role::Responder, PROTOCOL_V1, &caps),
        );
        let (ch_a, h_a, _peer_a) = initiator.expect("initiator handshake");
        let (ch_b, h_b, _peer_b) = responder.expect("responder handshake");
        ((ch_a, h_a), (ch_b, h_b))
    }

    // Both sides of an untampered handshake derive the identical transcript
    // hash; it is the shared root the approval code binds to (REQ-S-006).
    #[tokio::test]
    async fn handshake_hash_is_shared() {
        let ((_ch_a, h_a), (_ch_b, h_b)) = handshake_pair().await;
        assert_eq!(*h_a, *h_b);
    }

    // A reveal whose SHA-256 does not equal the earlier commitment is rejected
    // by the responder's constant-time check before any code is derived.
    #[tokio::test]
    async fn commitment_cheat_reveal_mismatch_fails_closed() {
        let ((mut ch_a, _h_a), (mut ch_b, h_b)) = handshake_pair().await;
        let na = [1u8; SAC_NONCE_LEN];
        let na_lie = [2u8; SAC_NONCE_LEN];
        let commitment = sac_commitment(&na);

        let cheat = async {
            send_pairing(
                &mut ch_a,
                PairingMessage::Commit {
                    commitment: B64Bytes::from(commitment.to_vec()),
                },
            )
            .await?;
            let _nc = expect_nonce(recv_pairing(&mut ch_a).await?)?;
            // Reveal a different nonce than was committed to.
            send_pairing(
                &mut ch_a,
                PairingMessage::Reveal {
                    na: B64Bytes::from(na_lie.to_vec()),
                },
            )
            .await?;
            Ok::<(), PairingError>(())
        };
        let honest = run_commit_reveal(&mut ch_b, Role::Responder, &h_b);
        let (cheat_res, honest_res) = tokio::join!(cheat, honest);

        cheat_res.expect("cheating initiator still sends valid frames");
        // The commitment check is an application-level guard in the SAC round,
        // not a transport fault, so it fails closed by returning an error and
        // deriving no code; `perform` then drops the channel. (The channel
        // itself is not poisoned — the reveal frame decrypted fine.)
        assert!(matches!(honest_res, Err(PairingError::CommitmentMismatch)));
    }

    // The responder requires the commit first; a nonce (or anything else) sent
    // before the commit is an ordering violation that fails closed.
    #[tokio::test]
    async fn commit_reveal_ordering_violation_fails_closed() {
        let ((mut ch_a, _h_a), (mut ch_b, h_b)) = handshake_pair().await;
        let bad = async {
            send_pairing(
                &mut ch_a,
                PairingMessage::Nonce {
                    nc: B64Bytes::from(vec![0u8; SAC_NONCE_LEN]),
                },
            )
            .await
        };
        let honest = run_commit_reveal(&mut ch_b, Role::Responder, &h_b);
        let (_bad, honest_res) = tokio::join!(bad, honest);
        assert!(matches!(honest_res, Err(PairingError::CommitmentMismatch)));
    }
}
