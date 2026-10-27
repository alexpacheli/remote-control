//! Hext Remote wire protocol.
//!
//! A session uses one QUIC connection with two kinds of traffic:
//! - a reliable, bidirectional **control stream** carrying [`Control`]
//!   messages as length-prefixed frames (see [`frame`]);
//! - unreliable **QUIC datagrams** carrying [`Datagram`] messages for
//!   latency-sensitive traffic (video, cursor, input; ping for now).
//!
//! All messages are serialized with `postcard`.

use serde::{Deserialize, Serialize};

pub mod frame;

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u16 = 1;

/// TLS ALPN identifier negotiated by both peers.
pub const ALPN: &[u8] = b"hext/1";

/// Default UDP port a host listens on for direct (LAN) connections.
pub const DEFAULT_PORT: u16 = 7420;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// Identifies a peer during the handshake.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub protocol_version: u16,
    pub app_version: String,
    pub device_name: String,
    pub os: Os,
}

impl PeerInfo {
    pub fn local(app_version: impl Into<String>, device_name: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            app_version: app_version.into(),
            device_name: device_name.into(),
            os: Os::current(),
        }
    }
}

/// Messages on the reliable control stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// First message, sent by the client.
    Hello(PeerInfo),
    /// Host's answer to an acceptable [`Control::Hello`].
    HelloAck(PeerInfo),
    /// Host refuses the session; the connection is closed afterwards.
    Reject(RejectReason),
    /// Orderly end of session.
    Bye,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    IncompatibleVersion { host_version: u16 },
    Denied,
    Busy,
}

/// Messages sent as unreliable QUIC datagrams.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram {
    /// RTT probe; the receiver echoes it back as [`Datagram::Pong`].
    Ping {
        seq: u32,
        sent_micros: u64,
    },
    Pong {
        seq: u32,
        sent_micros: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid message: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    Ok(postcard::to_stdvec(msg)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_roundtrip() {
        let msgs = [
            Control::Hello(PeerInfo::local("0.1.0", "desk")),
            Control::HelloAck(PeerInfo::local("0.1.0", "laptop")),
            Control::Reject(RejectReason::IncompatibleVersion { host_version: 7 }),
            Control::Bye,
        ];
        for msg in msgs {
            let bytes = encode(&msg).unwrap();
            assert_eq!(decode::<Control>(&bytes).unwrap(), msg);
        }
    }

    #[test]
    fn datagram_roundtrip() {
        let msg = Datagram::Ping {
            seq: 42,
            sent_micros: 1_234_567,
        };
        let bytes = encode(&msg).unwrap();
        assert_eq!(decode::<Datagram>(&bytes).unwrap(), msg);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode::<Control>(&[0xff, 0xff, 0xff]).is_err());
    }
}
