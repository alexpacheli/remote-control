use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A device's 9-digit ID, e.g. `482 913 557`: what a person types to reach
/// it. Assigned by the server at random (never in sequence), tied to the
/// device's public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct DeviceId(u32);

impl DeviceId {
    pub const MIN: u32 = 100_000_000;
    pub const MAX: u32 = 999_999_999;

    /// The ID with this number, if it has 9 digits.
    pub fn new(n: u32) -> Option<Self> {
        (Self::MIN..=Self::MAX).contains(&n).then_some(Self(n))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for DeviceId {
    type Error = ParseDeviceIdError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        Self::new(n).ok_or(ParseDeviceIdError)
    }
}

impl From<DeviceId> for u32 {
    fn from(id: DeviceId) -> u32 {
        id.0
    }
}

/// Grouped in threes: `482 913 557`.
impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.0;
        write!(
            f,
            "{:03} {:03} {:03}",
            n / 1_000_000,
            n / 1_000 % 1_000,
            n % 1_000
        )
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ID inválido: são 9 dígitos, como 482 913 557")]
pub struct ParseDeviceIdError;

/// Accepts spaces, dashes and dots between digits: `482913557`,
/// `482 913 557`, `482-913-557`.
impl FromStr for DeviceId {
    type Err = ParseDeviceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits: String = s
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '.'))
            .collect();
        if digits.len() != 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseDeviceIdError);
        }
        digits
            .parse()
            .ok()
            .and_then(Self::new)
            .ok_or(ParseDeviceIdError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_parse() {
        let id = DeviceId::new(482_913_557).unwrap();
        assert_eq!(id.to_string(), "482 913 557");
        for text in ["482913557", "482 913 557", "482-913-557", " 482.913.557 "] {
            assert_eq!(text.parse::<DeviceId>(), Ok(id), "{text}");
        }
        assert_eq!(
            DeviceId::new(100_000_007).unwrap().to_string(),
            "100 000 007"
        );
    }

    #[test]
    fn only_nine_digits() {
        for text in [
            "",
            "12345678",
            "1234567890",
            "012345678",
            "48291355a",
            "+48291355",
        ] {
            assert!(text.parse::<DeviceId>().is_err(), "{text}");
        }
        assert!(DeviceId::new(99_999_999).is_none());
        assert!(DeviceId::new(1_000_000_000).is_none());
        assert!(serde_json::from_str::<DeviceId>("12").is_err());
        assert_eq!(
            serde_json::to_string(&DeviceId::new(482_913_557)).unwrap(),
            "482913557"
        );
    }
}
