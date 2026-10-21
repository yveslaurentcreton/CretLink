//! Pure-Rust self-signed leaf generation for tests only.
//!
//! The wss verification suites need a server certificate and matching key, but
//! CretLink commits no key material (REQ-N-017). This helper mints a fresh
//! self-signed P-256 leaf at runtime with the RustCrypto stack (`x509-cert`
//! builder + `p256` signer), independent of the shipped TLS provider and with
//! no committed secrets. It is compiled only under the `test-cert` feature and
//! is never reachable from a shipped binary.
//!
//! The leaf is deliberately built with [`Profile::Manual`] and explicit
//! end-entity extensions rather than [`Profile::Root`]: a root/CA profile sets
//! `BasicConstraints.ca = true`, which rustls correctly rejects at handshake as
//! `CaUsedAsEndEntity`.

use std::str::FromStr;
use std::time::Duration;

use const_oid::db::rfc5280::ID_KP_SERVER_AUTH;
use p256::ecdsa::{DerSignature, SigningKey};
use p256::pkcs8::EncodePrivateKey;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::der::Encode;
use x509_cert::der::asn1::Ia5String;
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::{BasicConstraints, ExtendedKeyUsage, SubjectAltName};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::SubjectPublicKeyInfoOwned;
use x509_cert::time::Validity;

use crate::WssError;

/// One hour of validity is ample for a test that runs in milliseconds.
const TEST_CERT_TTL: Duration = Duration::from_secs(3600);

/// Mints a fresh self-signed server leaf for `dns_name`, returning the
/// certificate (DER) and its private key (PKCS#8 DER) ready for
/// [`crate::server_config`]. Pin the same certificate with [`crate::pinned_roots`]
/// on the client to complete the trust path.
///
/// # Errors
///
/// Returns [`WssError::TestCert`] if key generation or certificate encoding
/// fails, which in practice only happens on a malformed `dns_name`.
pub fn self_signed(
    dns_name: &str,
) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), WssError> {
    let map = |e: &dyn std::fmt::Display| WssError::TestCert(e.to_string());

    let signer = SigningKey::random(&mut rand_core::OsRng);
    let key_der = signer
        .to_pkcs8_der()
        .map_err(|e| map(&e))?
        .as_bytes()
        .to_vec();
    let spki = SubjectPublicKeyInfoOwned::from_key(*signer.verifying_key()).map_err(|e| map(&e))?;

    let profile = Profile::Manual { issuer: None };
    let serial = SerialNumber::from(1u32);
    let validity = Validity::from_now(TEST_CERT_TTL).map_err(|e| map(&e))?;
    let subject = Name::from_str(&format!("CN={dns_name}")).map_err(|e| map(&e))?;

    let mut builder = CertificateBuilder::new(profile, serial, validity, subject, spki, &signer)
        .map_err(|e| map(&e))?;
    builder
        .add_extension(&BasicConstraints {
            ca: false,
            path_len_constraint: None,
        })
        .map_err(|e| map(&e))?;
    let san = Ia5String::new(dns_name).map_err(|e| map(&e))?;
    builder
        .add_extension(&SubjectAltName(vec![GeneralName::DnsName(san)]))
        .map_err(|e| map(&e))?;
    builder
        .add_extension(&ExtendedKeyUsage(vec![ID_KP_SERVER_AUTH]))
        .map_err(|e| map(&e))?;

    let cert = builder.build::<DerSignature>().map_err(|e| map(&e))?;
    let cert_der = CertificateDer::from(cert.to_der().map_err(|e| map(&e))?);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    Ok((cert_der, key))
}
