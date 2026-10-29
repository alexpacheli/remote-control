//! Device identity: one Ed25519 key per device, created on first run and
//! kept on disk ([`KeyStore`]).
//!
//! The key is what other devices and the server know this device by: the
//! QUIC certificate is derived from it (so its fingerprint stays the same
//! between runs), the server will tie the 9-digit ID to its public key and
//! authenticate the device by a signed challenge, and with `iroh` the key is
//! the node's ID. The private key never leaves the device.

mod protect;
mod store;

use std::fmt;
use std::str::FromStr;

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use zeroize::Zeroizing;

pub use store::{FileKeyStore, KeyStore, config_dir, load_or_create};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("o sistema não forneceu números aleatórios seguros")]
    Random,
    #[error("chave inválida")]
    InvalidKey,
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error(
        "{0}: o arquivo da chave deste computador está danificado. Restaure um backup, ou apague o \
         arquivo para criar uma identidade nova (os outros computadores verão este como um \
         computador diferente)"
    )]
    Corrupt(std::path::PathBuf),
    #[error(
        "{path}: o Windows não abriu a chave deste computador ({source}). Ela só abre para o \
         usuário do Windows que a criou: se o arquivo veio de outro usuário ou computador, ou se a \
         senha do Windows foi redefinida, entre com esse usuário ou restaure um backup. Apagar o \
         arquivo cria uma identidade nova (os outros computadores verão este como um computador \
         diferente, com outro ID)"
    )]
    Sealed {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("sem pasta para guardar a chave deste computador: defina {0}")]
    NoConfigDir(&'static str),
}

pub type Result<T, E = CryptoError> = std::result::Result<T, E>;

/// Compares secrets (codes, tokens) in constant time: no early exit at the
/// first difference, which would leak how much of a guess was right.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Bytes in an Ed25519 seed (the private key).
pub const SEED_LEN: usize = 32;

/// A device's Ed25519 key pair.
pub struct DeviceKey {
    seed: Zeroizing<[u8; SEED_LEN]>,
    pair: Ed25519KeyPair,
}

impl DeviceKey {
    /// A new random key.
    pub fn generate() -> Result<Self> {
        let mut seed = Zeroizing::new([0; SEED_LEN]);
        SystemRandom::new()
            .fill(seed.as_mut())
            .map_err(|_| CryptoError::Random)?;
        Self::from_seed(&seed)
    }

    /// The key whose private half is `seed`.
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Result<Self> {
        let pair =
            Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self {
            seed: Zeroizing::new(*seed),
            pair,
        })
    }

    pub fn public_key(&self) -> PublicKey {
        let mut bytes = [0; 32];
        bytes.copy_from_slice(self.pair.public_key().as_ref());
        PublicKey(bytes)
    }

    /// Signs `message` (e.g. a challenge from the server).
    pub fn sign(&self, message: &[u8]) -> Signature {
        let mut bytes = [0; 64];
        bytes.copy_from_slice(self.pair.sign(message).as_ref());
        Signature(bytes)
    }

    /// The private key, for libraries that take raw Ed25519 seeds (`iroh`).
    /// Keep it out of logs and off the network.
    pub fn seed(&self) -> &[u8; SEED_LEN] {
        &self.seed
    }

    /// The key as PKCS#8 v2 DER (RFC 8410), for TLS libraries.
    pub fn pkcs8_der(&self) -> Zeroizing<Vec<u8>> {
        // SEQUENCE { version 1, AlgorithmIdentifier { id-Ed25519 },
        //   OCTET STRING { OCTET STRING seed }, [1] { BIT STRING public key } }
        const PREFIX: [u8; 16] = [
            0x30, 0x53, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        const PUBLIC: [u8; 5] = [0xa1, 0x23, 0x03, 0x21, 0x00];
        let mut der = Zeroizing::new(Vec::with_capacity(85));
        der.extend_from_slice(&PREFIX);
        der.extend_from_slice(self.seed.as_ref());
        der.extend_from_slice(&PUBLIC);
        der.extend_from_slice(&self.public_key().0);
        der
    }
}

/// Shows only the public key: the private key never goes into logs.
impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// A device's public key: what identifies it to others.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Whether `signature` is this key's signature of `message`.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> bool {
        UnparsedPublicKey::new(&ED25519, self.0)
            .verify(message, &signature.0)
            .is_ok()
    }
}

/// Lowercase hex, 64 digits (the same text as an `iroh` node ID).
impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({self})")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid public key: expected 64 hex digits")]
pub struct ParsePublicKeyError;

impl FromStr for PublicKey {
    type Err = ParsePublicKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(s.trim(), &mut bytes).map_err(|_| ParsePublicKeyError)?;
        Ok(Self(bytes))
    }
}

/// An Ed25519 signature.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Signature([u8; 64]);

impl Signature {
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(self) -> [u8; 64] {
        self.0
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({})", hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_verify_only_for_their_key_and_message() {
        let key = DeviceKey::generate().unwrap();
        let other = DeviceKey::generate().unwrap();
        let sig = key.sign(b"challenge");
        assert!(key.public_key().verify(b"challenge", &sig));
        assert!(!key.public_key().verify(b"challenge!", &sig));
        assert!(!other.public_key().verify(b"challenge", &sig));
        assert_ne!(key.public_key(), other.public_key());
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }

    #[test]
    fn same_seed_same_key() {
        let key = DeviceKey::generate().unwrap();
        let again = DeviceKey::from_seed(key.seed()).unwrap();
        assert_eq!(key.public_key(), again.public_key());
        // Ed25519 signatures are deterministic.
        assert_eq!(key.sign(b"x"), again.sign(b"x"));
    }

    #[test]
    fn rfc8032_test_vector() {
        // RFC 8032, section 7.1, TEST 1.
        let seed: [u8; 32] =
            hex::decode("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
                .unwrap()
                .try_into()
                .unwrap();
        let key = DeviceKey::from_seed(&seed).unwrap();
        assert_eq!(
            key.public_key().to_string(),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        assert_eq!(
            hex::encode(key.sign(b"").to_bytes()),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn pkcs8_is_accepted_by_ring() {
        let key = DeviceKey::generate().unwrap();
        let der = key.pkcs8_der();
        assert_eq!(der.len(), 85);
        let parsed = Ed25519KeyPair::from_pkcs8(&der).unwrap();
        assert_eq!(parsed.public_key().as_ref(), key.public_key().as_bytes());
    }

    #[test]
    fn public_key_text_roundtrip() {
        let key = DeviceKey::generate().unwrap().public_key();
        let text = key.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<PublicKey>().unwrap(), key);
        assert!("abcd".parse::<PublicKey>().is_err());
    }

    #[test]
    fn debug_hides_the_private_key() {
        let key = DeviceKey::generate().unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains(&hex::encode(key.seed())));
        assert!(debug.contains(&key.public_key().to_string()));
    }
}
