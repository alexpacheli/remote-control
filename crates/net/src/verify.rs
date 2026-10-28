use std::sync::Arc;

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use hext_crypto::PublicKey;

use crate::Fingerprint;

/// What identifies the host a client expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPin {
    /// Any host: the connection is encrypted, but not authenticated. Only
    /// meant for LAN testing.
    None,
    /// The certificate's fingerprint, as the host shows it.
    Fingerprint(Fingerprint),
    /// The host's device key (from the server, remembered per ID).
    Key(PublicKey),
}

impl From<Option<Fingerprint>> for HostPin {
    fn from(pin: Option<Fingerprint>) -> Self {
        pin.map_or(Self::None, Self::Fingerprint)
    }
}

/// The DER `SubjectPublicKeyInfo` of an Ed25519 key (RFC 8410).
fn ed25519_spki(key: &PublicKey) -> Vec<u8> {
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(key.as_bytes());
    spki
}

/// Whether the certificate's own key (parsed, not searched for) is `key`.
/// The handshake then proves the host holds its private half.
pub(crate) fn certifies(cert: &CertificateDer<'_>, key: &PublicKey) -> bool {
    webpki::EndEntityCert::try_from(cert)
        .is_ok_and(|cert| cert.subject_public_key_info().as_ref() == ed25519_spki(key))
}

/// Accepts the host certificate only if it matches the pin.
#[derive(Debug)]
pub(crate) struct PinnedVerifier {
    pin: HostPin,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub(crate) fn new(pin: HostPin, provider: Arc<CryptoProvider>) -> Self {
        Self { pin, provider }
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let ok = match self.pin {
            HostPin::None => true,
            HostPin::Fingerprint(pin) => Fingerprint::of(end_entity) == pin,
            HostPin::Key(key) => certifies(end_entity, &key),
        };
        if ok {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
