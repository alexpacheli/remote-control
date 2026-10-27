use std::fmt;
use std::str::FromStr;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::Result;

/// SHA-256 of a DER certificate, used to pin the host's identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(cert: &CertificateDer<'_>) -> Self {
        let digest = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Self(bytes)
    }
}

/// Uppercase hex in groups of four, e.g. `1A2B:3C4D:…`.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex = hex::encode_upper(self.0);
        for (i, chunk) in hex.as_bytes().chunks(4).enumerate() {
            if i > 0 {
                f.write_str(":")?;
            }
            // Hex output is ASCII.
            f.write_str(std::str::from_utf8(chunk).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid fingerprint: expected 64 hex digits (separators ':' and spaces allowed)")]
pub struct ParseFingerprintError;

impl FromStr for Fingerprint {
    type Err = ParseFingerprintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits: String = s
            .chars()
            .filter(|c| !matches!(c, ':' | ' ' | '-'))
            .collect();
        let mut bytes = [0; 32];
        hex::decode_to_slice(digits, &mut bytes).map_err(|_| ParseFingerprintError)?;
        Ok(Self(bytes))
    }
}

/// A host's TLS certificate and private key.
pub struct Identity {
    pub(crate) cert: CertificateDer<'static>,
    pub(crate) key: PrivatePkcs8KeyDer<'static>,
    fingerprint: Fingerprint,
}

impl Identity {
    /// Generates a fresh self-signed identity. It lives only as long as the
    /// process; persistent device keys arrive in F3.
    pub fn generate() -> Result<Self> {
        let certified = rcgen::generate_simple_self_signed(vec!["hext".to_owned()])?;
        let cert = certified.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let fingerprint = Fingerprint::of(&cert);
        Ok(Self {
            cert,
            key,
            fingerprint,
        })
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    pub(crate) fn key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(self.key.clone_key())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_parse_roundtrip() {
        let fp = Identity::generate().unwrap().fingerprint();
        let text = fp.to_string();
        assert_eq!(text.len(), 64 + 15);
        assert_eq!(text.parse::<Fingerprint>().unwrap(), fp);
        assert_eq!(
            text.replace(':', "")
                .to_lowercase()
                .parse::<Fingerprint>()
                .unwrap(),
            fp
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!("abcd".parse::<Fingerprint>().is_err());
        assert!("zz".repeat(32).parse::<Fingerprint>().is_err());
    }
}
