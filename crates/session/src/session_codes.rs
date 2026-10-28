//! One-time codes: when the host's user accepts a viewer (through the
//! server), the viewer gets a code that opens one session, instead of the
//! access code.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_crypto::PublicKey;
use ring::rand::{SecureRandom, SystemRandom};

use crate::codes_match;

/// How long the viewer has to connect after being accepted.
pub const SESSION_CODE_LIFETIME: Duration = Duration::from_secs(120);

/// A code, the viewer it was issued to, and when.
type Issued = (String, PublicKey, Instant);

/// Codes waiting to be used; cheap to clone (shared).
#[derive(Debug, Clone, Default)]
pub struct SessionCodes(Arc<Mutex<Vec<Issued>>>);

impl SessionCodes {
    /// A new code (20 random digits) for the viewer with key `viewer`,
    /// valid once within [`SESSION_CODE_LIFETIME`].
    pub fn issue(&self, viewer: PublicKey) -> String {
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("system random numbers");
        let code = format!("{:020}", u128::from_le_bytes(bytes) % 10u128.pow(20));
        let mut codes = self.0.lock().expect("lock");
        codes.retain(|(_, _, issued)| issued.elapsed() < SESSION_CODE_LIFETIME);
        codes.push((code.clone(), viewer, Instant::now()));
        code
    }

    /// Whether `given` is a live code issued to `viewer`; it is then used up.
    pub fn redeem(&self, given: &str, viewer: &PublicKey) -> bool {
        self.redeem_at(given, viewer, Instant::now())
    }

    fn redeem_at(&self, given: &str, viewer: &PublicKey, now: Instant) -> bool {
        let mut codes = self.0.lock().expect("lock");
        codes.retain(|(_, _, issued)| {
            now.saturating_duration_since(*issued) < SESSION_CODE_LIFETIME
        });
        // Compare against every code (constant time each), then remove.
        let found = codes
            .iter()
            .enumerate()
            .fold(None, |found, (i, (code, owner, _))| {
                let matched = codes_match(code, given) & (owner == viewer);
                found.or(matched.then_some(i))
            });
        found.map(|i| codes.remove(i)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hext_crypto::DeviceKey;

    fn key() -> PublicKey {
        DeviceKey::generate().unwrap().public_key()
    }

    #[test]
    fn a_code_opens_one_session() {
        let codes = SessionCodes::default();
        let viewer = key();
        let code = codes.issue(viewer);
        assert_eq!(code.len(), 20);
        assert!(!codes.redeem("12345", &viewer));
        assert!(!codes.redeem("", &viewer));
        assert!(codes.redeem(&code, &viewer));
        assert!(!codes.redeem(&code, &viewer), "used up");
    }

    #[test]
    fn only_the_viewer_it_was_issued_to() {
        let codes = SessionCodes::default();
        let viewer = key();
        let code = codes.issue(viewer);
        assert!(!codes.redeem(&code, &key()), "someone else with the code");
        assert!(codes.redeem(&code, &viewer));
    }

    #[test]
    fn codes_are_independent() {
        let codes = SessionCodes::default();
        let viewer = key();
        let (a, b) = (codes.issue(viewer), codes.issue(viewer));
        assert_ne!(a, b);
        assert!(codes.redeem(&b, &viewer));
        assert!(codes.redeem(&a, &viewer));
    }

    #[test]
    fn old_codes_expire() {
        let codes = SessionCodes::default();
        let viewer = key();
        let code = codes.issue(viewer);
        assert!(!codes.redeem_at(&code, &viewer, Instant::now() + SESSION_CODE_LIFETIME));
    }
}
