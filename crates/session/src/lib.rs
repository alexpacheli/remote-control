//! Session pipelines on top of the transport.
//!
//! - [`host::serve`]: capture → encode → video stream, input stream → injector.
//! - [`client::start`]: video stream → decode → caller's callback, and the
//!   caller's input events → input stream.

pub mod client;
pub mod host;
mod session_codes;
mod stats;

pub use session_codes::SessionCodes;
pub use stats::Stats;

use hext_capture::CaptureError;
use hext_codec::CodecError;
use hext_net::NetError;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("{0}")]
    Protocol(String),
}

pub type Result<T, E = SessionError> = std::result::Result<T, E>;

/// Access codes are shown grouped ("482 913 557"); compare them without
/// separators and in constant time.
pub fn codes_match(expected: &str, given: &str) -> bool {
    let normalize = |s: &str| -> Vec<u8> {
        s.bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
            .collect()
    };
    hext_crypto::constant_time_eq(&normalize(expected), &normalize(given))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_codes() {
        assert!(codes_match("482913557", "482 913 557"));
        assert!(codes_match("482913557", "482-913-557"));
        assert!(!codes_match("482913557", "482913558"));
        assert!(!codes_match("482913557", "48291355"));
        assert!(!codes_match("482913557", ""));
    }
}
