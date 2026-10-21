//! Relay admission control — authentication *for abuse control only, never for
//! session authority* (ST-M007-004, ADR-0016 §4).
//!
//! A publicly reachable relay must be able to say **who may use the forwarding
//! service** so it does not become an anonymous abuse service (REQ-S-021). That
//! is the *only* thing an admission credential decides. It is strictly
//! orthogonal to CretLink session authority:
//!
//! - holding a relay credential grants **zero** control over any session, and
//! - lacking session authority is **never** repaired by holding one.
//!
//! All session trust still comes from the end-to-end Noise handshake and the
//! human-compared Session Approval Code (ADR-0003), which fail closed
//! (REQ-S-009). This module never touches a session key, a ticket, or any
//! plaintext; it only maps a presented bearer credential to a **non-secret**
//! accountability label ([`ClientId`]) the operator may log (REQ-S-017).
//!
//! # Policies
//!
//! [`AdmissionPolicy`] covers the deployment shapes ADR-0016 names:
//!
//! - [`Open`](AdmissionPolicy::Open) — a self-hosted operator who wants no
//!   credential; every client is admitted under its own peer label. Rate limits
//!   and quotas still apply, keyed by that label.
//! - [`SharedToken`](AdmissionPolicy::SharedToken) — one shared secret admits
//!   under one fixed label (a small self-hosted or family deployment).
//! - [`Allowlist`](AdmissionPolicy::Allowlist) — a managed deployment issues a
//!   distinct bearer token per account, each mapped to its own label.
//!
//! # Fail-closed and secret handling
//!
//! A missing or wrong credential **denies** admission ([`AdmissionError`]); it
//! never partially admits and never leaks whether a near-miss occurred. Tokens
//! are compared in constant time, zeroized on drop, and redacted from [`Debug`],
//! so a credential can never reach a log (REQ-N-017, REQ-N-015).
//!

use core::fmt;

use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroize;

/// A non-secret, log-safe label identifying an admitted client for rate
/// limiting, quotas, and accountability. It is **never** the credential itself.
///
/// For [`AdmissionPolicy::Open`] it is the caller-supplied peer label (for
/// example a remote address); for the token policies it is the operator-assigned
/// label bound to the credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    /// Wraps an accountability label. The label is metadata the operator may log
    /// (REQ-S-017); it must never carry a secret.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self(label.into())
    }

    /// The label as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A secret bearer credential presented to the relay for admission. It is
/// compared in constant time, zeroized on drop, and redacted from [`Debug`], so
/// it never lands in a log (REQ-N-017). It authenticates *use of the forwarder*
/// and confers no session authority whatsoever.
#[derive(Clone)]
pub struct AdmissionToken(Vec<u8>);

impl AdmissionToken {
    /// Wraps raw credential bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Wraps a credential given as text.
    ///
    /// Named `from_text` rather than `from_str` so it is never confused with
    /// [`std::str::FromStr`]: this construction is infallible and returns the
    /// token directly, not a `Result`.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self(text.as_bytes().to_vec())
    }

    /// Borrows the raw credential bytes so a dialer can serialise them into the
    /// rendezvous preamble ([`crate::preamble`]). The secret stays owned by this
    /// zeroizing type — this only lends a read-only view for the length-prefixed
    /// write, mirroring [`crate::ticket::RendezvousTicket::as_bytes`]. It never
    /// widens the redaction surface: [`Debug`] stays redacted and the bytes are
    /// still zeroized on drop. Callers must not log the returned slice.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Constant-time equality. The credential length is treated as a public
    /// parameter (a bearer token has a fixed issued length): a length mismatch
    /// returns `false` without comparing content, and equal-length credentials
    /// are compared with no early exit that could reveal how many bytes matched.
    #[must_use]
    fn ct_matches(&self, other: &AdmissionToken) -> bool {
        if self.0.len() != other.0.len() {
            return false;
        }
        self.0.ct_eq(&other.0).into()
    }
}

impl Drop for AdmissionToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Redacted: an admission credential must never reach a log.
impl fmt::Debug for AdmissionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdmissionToken(<redacted>)")
    }
}

/// How the relay decides whether a connecting client may use the forwarder. See
/// the [module docs](self).
pub enum AdmissionPolicy {
    /// No credential required. Every client is admitted under its own peer
    /// label; rate limits and quotas still apply per label.
    Open,
    /// One shared secret admits under one fixed [`ClientId`].
    SharedToken {
        /// The shared secret every admitted client must present.
        token: AdmissionToken,
        /// The label all holders of the shared secret are accounted under.
        client: ClientId,
    },
    /// A managed allowlist: each issued token maps to its own [`ClientId`].
    Allowlist(Vec<(AdmissionToken, ClientId)>),
}

/// Why admission was denied. Both variants *deny*; they are distinguished only
/// so the relay can return an actionable, secret-free error (REQ-N-015).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AdmissionError {
    /// The policy requires a credential and none was presented.
    #[error("the relay requires an admission credential; none was presented")]
    CredentialRequired,
    /// A credential was presented but did not match any admitted client.
    #[error("the presented admission credential was not accepted")]
    CredentialRejected,
}

impl AdmissionPolicy {
    /// Decides whether the `presented` credential may use the forwarder,
    /// returning the non-secret [`ClientId`] to account it under. `peer` is the
    /// caller-supplied addressing label used as the identity under
    /// [`Open`](Self::Open).
    ///
    /// This grants **no session authority** — only permission to be forwarded,
    /// subject to rate limits and quotas.
    ///
    /// # Errors
    ///
    /// [`AdmissionError::CredentialRequired`] or
    /// [`AdmissionError::CredentialRejected`] — see each variant. Both deny.
    pub fn admit(
        &self,
        presented: Option<&AdmissionToken>,
        peer: &str,
    ) -> Result<ClientId, AdmissionError> {
        match self {
            Self::Open => Ok(ClientId::new(peer)),
            Self::SharedToken { token, client } => match presented {
                None => Err(AdmissionError::CredentialRequired),
                Some(p) if token.ct_matches(p) => Ok(client.clone()),
                Some(_) => Err(AdmissionError::CredentialRejected),
            },
            Self::Allowlist(entries) => {
                let Some(p) = presented else {
                    return Err(AdmissionError::CredentialRequired);
                };
                // Compare against every entry with no early exit, so the timing
                // does not reveal which entry (if any) matched. The first match
                // wins; a non-match reveals nothing about any token's content.
                let mut matched: Option<ClientId> = None;
                for (token, client) in entries {
                    if token.ct_matches(p) && matched.is_none() {
                        matched = Some(client.clone());
                    }
                }
                matched.ok_or(AdmissionError::CredentialRejected)
            }
        }
    }
}

/// Manual `Debug` that redacts every embedded credential.
impl fmt::Debug for AdmissionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open => f.write_str("AdmissionPolicy::Open"),
            Self::SharedToken { client, .. } => f
                .debug_struct("AdmissionPolicy::SharedToken")
                .field("client", client)
                .field("token", &"<redacted>")
                .finish(),
            Self::Allowlist(entries) => f
                .debug_struct("AdmissionPolicy::Allowlist")
                .field(
                    "clients",
                    &entries.iter().map(|(_, c)| c).collect::<Vec<_>>(),
                )
                .field("tokens", &"<redacted>")
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn open_admits_every_peer_under_its_own_label() {
        let policy = AdmissionPolicy::Open;
        assert_eq!(
            policy.admit(None, "203.0.113.7").unwrap(),
            ClientId::new("203.0.113.7")
        );
        // A stray credential is simply ignored under Open.
        let stray = AdmissionToken::from_text("whatever");
        assert_eq!(
            policy.admit(Some(&stray), "198.51.100.9").unwrap(),
            ClientId::new("198.51.100.9")
        );
    }

    #[test]
    fn shared_token_requires_and_matches_the_secret() {
        let policy = AdmissionPolicy::SharedToken {
            token: AdmissionToken::from_text("s3cret-shared"),
            client: ClientId::new("shared"),
        };
        // Missing credential is denied, not admitted anonymously.
        assert_eq!(
            policy.admit(None, "peer").unwrap_err(),
            AdmissionError::CredentialRequired
        );
        // Wrong credential is rejected.
        let wrong = AdmissionToken::from_text("s3cret-wrong");
        assert_eq!(
            policy.admit(Some(&wrong), "peer").unwrap_err(),
            AdmissionError::CredentialRejected
        );
        // Right credential admits under the fixed label, regardless of peer.
        let right = AdmissionToken::from_text("s3cret-shared");
        assert_eq!(
            policy.admit(Some(&right), "peer").unwrap(),
            ClientId::new("shared")
        );
    }

    #[test]
    fn allowlist_maps_each_token_to_its_own_client() {
        let policy = AdmissionPolicy::Allowlist(vec![
            (
                AdmissionToken::from_text("tok-alice"),
                ClientId::new("alice"),
            ),
            (AdmissionToken::from_text("tok-bob"), ClientId::new("bob")),
        ]);
        assert_eq!(
            policy
                .admit(Some(&AdmissionToken::from_text("tok-bob")), "peer")
                .unwrap(),
            ClientId::new("bob")
        );
        assert_eq!(
            policy
                .admit(Some(&AdmissionToken::from_text("tok-alice")), "peer")
                .unwrap(),
            ClientId::new("alice")
        );
        // An unknown token is rejected; a missing one is required.
        assert_eq!(
            policy
                .admit(Some(&AdmissionToken::from_text("tok-eve")), "peer")
                .unwrap_err(),
            AdmissionError::CredentialRejected
        );
        assert_eq!(
            policy.admit(None, "peer").unwrap_err(),
            AdmissionError::CredentialRequired
        );
    }

    #[test]
    fn different_length_credentials_never_match() {
        let policy = AdmissionPolicy::SharedToken {
            token: AdmissionToken::from_text("short"),
            client: ClientId::new("shared"),
        };
        let long = AdmissionToken::from_text("a-much-longer-credential");
        assert_eq!(
            policy.admit(Some(&long), "peer").unwrap_err(),
            AdmissionError::CredentialRejected
        );
    }

    #[test]
    fn debug_redacts_every_credential() {
        let token = AdmissionToken::from_text("top-secret");
        assert_eq!(format!("{token:?}"), "AdmissionToken(<redacted>)");

        let shared = AdmissionPolicy::SharedToken {
            token: AdmissionToken::from_text("top-secret"),
            client: ClientId::new("shared"),
        };
        let rendered = format!("{shared:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(!rendered.contains("top-secret"), "{rendered}");

        let allow = AdmissionPolicy::Allowlist(vec![(
            AdmissionToken::from_text("top-secret"),
            ClientId::new("alice"),
        )]);
        let rendered = format!("{allow:?}");
        assert!(rendered.contains("alice"), "{rendered}");
        assert!(!rendered.contains("top-secret"), "{rendered}");
    }
}
