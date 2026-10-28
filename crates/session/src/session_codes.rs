//! One-time codes: when the host's user accepts a viewer (through the
//! server), the viewer gets a code that opens one session, instead of the
//! access code.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

use crate::codes_match;

/// How long the viewer has to connect after being accepted.
pub const SESSION_CODE_LIFETIME: Duration = Duration::from_secs(120);

/// Codes waiting to be used; cheap to clone (shared).
#[derive(Debug, Clone, Default)]
pub struct SessionCodes(Arc<Mutex<Vec<(String, Instant)>>>);

impl SessionCodes {
    /// A new code (20 random digits), valid once within
    /// [`SESSION_CODE_LIFETIME`].
    pub fn issue(&self) -> String {
        let mut bytes = [0u8; 16];
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("system random numbers");
        let code = format!("{:020}", u128::from_le_bytes(bytes) % 10u128.pow(20));
        let mut codes = self.0.lock().expect("lock");
        codes.retain(|(_, issued)| issued.elapsed() < SESSION_CODE_LIFETIME);
        codes.push((code.clone(), Instant::now()));
        code
    }

    /// Whether `given` is a live code; it is then used up.
    pub fn redeem(&self, given: &str) -> bool {
        self.redeem_at(given, Instant::now())
    }

    fn redeem_at(&self, given: &str, now: Instant) -> bool {
        let mut codes = self.0.lock().expect("lock");
        codes.retain(|(_, issued)| now.saturating_duration_since(*issued) < SESSION_CODE_LIFETIME);
        // Compare against every code (constant time each), then remove.
        let found = codes
            .iter()
            .enumerate()
            .fold(None, |found, (i, (code, _))| {
                let matched = codes_match(code, given);
                found.or(matched.then_some(i))
            });
        found.map(|i| codes.remove(i)).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_opens_one_session() {
        let codes = SessionCodes::default();
        let code = codes.issue();
        assert_eq!(code.len(), 20);
        assert!(!codes.redeem("12345"));
        assert!(!codes.redeem(""));
        assert!(codes.redeem(&code));
        assert!(!codes.redeem(&code), "used up");
    }

    #[test]
    fn codes_are_independent() {
        let codes = SessionCodes::default();
        let (a, b) = (codes.issue(), codes.issue());
        assert_ne!(a, b);
        assert!(codes.redeem(&b));
        assert!(codes.redeem(&a));
    }

    #[test]
    fn old_codes_expire() {
        let codes = SessionCodes::default();
        let code = codes.issue();
        assert!(!codes.redeem_at(&code, Instant::now() + SESSION_CODE_LIFETIME));
    }
}
