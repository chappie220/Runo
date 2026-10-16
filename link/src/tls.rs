//! TLS 1.3 with mutual authentication and no CA: each side shows a
//! self-signed certificate for its Ed25519 identity, and is known by that
//! key. The device accepts any client key here and decides afterwards
//! whether the key is trusted (or may pair); a client accepts the device's
//! key only when it is the one pinned at pairing.

use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, aws_lc_rs};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme,
};

use crate::identity::Identity;

/// The name a client asks for; the device's certificate is never checked
/// against it.
pub const SERVER_NAME: &str = "runo.invalid";
/// The exporter label for values bound to one TLS session.
const EXPORTER_LABEL: &[u8] = b"EXPORTER-runo-link";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    aws_lc_rs::default_provider().signature_verification_algorithms
}

/// The Ed25519 public key in a certificate (DER), if it has one.
pub fn certificate_key(cert: &[u8]) -> Option<[u8; 32]> {
    // Certificate ::= SEQUENCE { tbsCertificate, ... }
    let (certificate, _) = der_element(cert, 0x30)?;
    let (tbs, _) = der_element(certificate, 0x30)?;
    let mut rest = tbs;
    // [0] version, optional.
    if rest.first() == Some(&0xa0) {
        rest = der_element(rest, 0xa0)?.1;
    }
    // serialNumber, signature, issuer, validity, subject.
    for tag in [0x02, 0x30, 0x30, 0x30, 0x30] {
        rest = der_element(rest, tag)?.1;
    }
    let (spki, _) = der_element(rest, 0x30)?;
    // AlgorithmIdentifier { id-Ed25519 (1.3.101.112) }, BIT STRING with no
    // unused bits.
    const PREFIX: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];
    let key = spki.strip_prefix(PREFIX)?;
    key.try_into().ok()
}

/// The contents of the DER element at the start of `input` (which must
/// have `tag`) and what follows it.
fn der_element(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&first, rest) = input.split_first()?;
    if first != tag {
        return None;
    }
    let (&length, mut rest) = rest.split_first()?;
    let length = if length < 0x80 {
        usize::from(length)
    } else {
        let count = usize::from(length & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let mut value = 0usize;
        for byte in &rest[..count] {
            value = value << 8 | usize::from(*byte);
        }
        rest = &rest[count..];
        value
    };
    if rest.len() < length {
        return None;
    }
    Some(rest.split_at(length))
}

fn key_of(cert: &CertificateDer<'_>) -> Result<[u8; 32], Error> {
    certificate_key(cert.as_ref())
        .ok_or_else(|| Error::General("the peer's certificate has no Ed25519 key".into()))
}

/// The device side: a client must show a certificate with an Ed25519 key;
/// which keys are welcome is decided after the handshake.
#[derive(Debug)]
struct AnyClientKey {
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AnyClientKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        key_of(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// The client side: the device's key must be `pinned`; with none (only
/// while pairing) any Ed25519 key is taken, and the client keeps the one it
/// saw if the pairing succeeds.
#[derive(Debug)]
struct PinnedDevice {
    pinned: Option<[u8; 32]>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedDevice {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let key = key_of(end_entity)?;
        match self.pinned {
            Some(pinned) if pinned != key => Err(Error::General(format!(
                "this is not the paired device: it is {} (expected {})",
                crate::device_id(&key),
                crate::device_id(&pinned)
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("TLS 1.2 is not used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub fn server_config(identity: &Identity) -> Result<Arc<ServerConfig>> {
    let verifier = Arc::new(AnyClientKey {
        algorithms: algorithms(),
    });
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS 1.3")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![identity.certificate()?], identity.private_key())
        .context("the device certificate")?;
    Ok(Arc::new(config))
}

/// For a client with `identity`, to a device whose key is `pinned` (none
/// while pairing).
pub fn client_config(identity: &Identity, pinned: Option<[u8; 32]>) -> Result<Arc<ClientConfig>> {
    let verifier = Arc::new(PinnedDevice {
        pinned,
        algorithms: algorithms(),
    });
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![identity.certificate()?], identity.private_key())
        .context("the client certificate")?;
    Ok(Arc::new(config))
}

/// The peer's key, once the handshake is done.
pub fn peer_key(connection: &rustls::CommonState) -> Option<[u8; 32]> {
    certificate_key(connection.peer_certificates()?.first()?.as_ref())
}

/// 32 bytes both ends of this TLS session share and nobody else knows
/// (RFC 8446 exporter): it ties a pairing code to this session.
pub fn session_secret<D>(connection: &rustls::ConnectionCommon<D>) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    connection
        .export_keying_material(&mut out, EXPORTER_LABEL, None)
        .context("TLS exporter")?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_read_from_a_certificate() {
        let identity = Identity::generate().unwrap();
        let cert = identity.certificate().unwrap();
        assert_eq!(certificate_key(cert.as_ref()), Some(identity.public_key()));
        assert_eq!(certificate_key(&cert.as_ref()[..40]), None);
        assert_eq!(certificate_key(b""), None);
    }
}
