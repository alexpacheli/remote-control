//! Hext Remote wire protocol.
//!
//! A session uses one QUIC connection with two kinds of traffic:
//! - a reliable, bidirectional **control stream** that starts with a
//!   [preamble](encode_preamble) from each side and then carries [`Control`]
//!   messages as length-prefixed frames (see [`frame`]);
//! - **unidirectional streams**, each starting with a [`StreamKind`] byte:
//!   video frames (host → client) and input events (client → host);
//! - unreliable **QUIC datagrams** carrying [`Datagram`] messages (ping).
//!
//! All messages are serialized with `postcard`. In F1 video travels on a
//! reliable stream; datagrams with FEC replace it once tuned over the internet.

use serde::{Deserialize, Serialize};

pub mod frame;

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u16 = 3;

/// TLS ALPN identifier. It carries no version: versions are compared through
/// the preamble, so mismatched peers still connect and can report why they fail.
pub const ALPN: &[u8] = b"hext";

/// Size of the control stream preamble.
pub const PREAMBLE_LEN: usize = 4;

const PREAMBLE_MAGIC: [u8; 2] = *b"HX";

/// Each side opens the control stream with `b"HX"` followed by its protocol
/// version (`u16` little-endian). This layout never changes, so peers of any
/// version can detect a mismatch before decoding versioned messages.
pub fn encode_preamble(version: u16) -> [u8; PREAMBLE_LEN] {
    let v = version.to_le_bytes();
    [PREAMBLE_MAGIC[0], PREAMBLE_MAGIC[1], v[0], v[1]]
}

/// Returns the protocol version announced in a preamble.
pub fn decode_preamble(bytes: [u8; PREAMBLE_LEN]) -> Result<u16, CodecError> {
    if bytes[..2] != PREAMBLE_MAGIC {
        return Err(CodecError::BadPreamble);
    }
    Ok(u16::from_le_bytes([bytes[2], bytes[3]]))
}

/// Default UDP port a host listens on for direct (LAN) connections.
pub const DEFAULT_PORT: u16 = 7420;

/// Upper bound for one encoded video frame on the video stream.
pub const MAX_VIDEO_FRAME_LEN: usize = 16 * 1024 * 1024;

/// First byte of every unidirectional stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StreamKind {
    /// Host → client, carries [`VideoFrame`]s.
    Video = 1,
    /// Client → host, carries [`InputEvent`]s.
    Input = 2,
}

impl StreamKind {
    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Video),
            2 => Some(Self::Input),
            _ => None,
        }
    }

    /// Largest frame accepted on a stream of this kind.
    pub const fn max_frame_len(self) -> usize {
        match self {
            Self::Video => MAX_VIDEO_FRAME_LEN,
            Self::Input => frame::MAX_FRAME_LEN,
        }
    }
}

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
    pub app_version: String,
    pub device_name: String,
    pub os: Os,
}

impl PeerInfo {
    pub fn local(app_version: impl Into<String>, device_name: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
            device_name: device_name.into(),
            os: Os::current(),
        }
    }
}

/// The client's introduction.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub peer: PeerInfo,
    /// Code shown by the host; it must match before anything is shared.
    pub access_code: String,
}

/// Messages on the reliable control stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Control {
    /// First message after the preamble, sent by the client.
    Hello(Hello),
    /// Host's answer to an acceptable [`Control::Hello`].
    HelloAck(PeerInfo),
    /// Host refuses the session; the connection is closed afterwards.
    Reject(RejectReason),
    /// Client asks for a fresh keyframe (start of stream, decode error).
    RequestKeyframe,
    /// Orderly end of session.
    Bye,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Wrong access code.
    WrongCode,
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

/// One encoded H.264 access unit on the video stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    /// Time spent capturing (copying) this frame on the host.
    pub capture_micros: u32,
    /// Time spent encoding this frame on the host.
    pub encode_micros: u32,
    /// Encoded by the GPU (shown in the viewer's stats).
    pub hardware: bool,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Input from the viewer, injected on the host.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Absolute position, normalized to `0.0..=1.0` over the shared display.
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll in notches (lines). Positive `dy` scrolls up, positive `dx` right.
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// PC set-1 scancode; `0xE0xx` marks extended keys.
    Key {
        scancode: u16,
        pressed: bool,
    },
    /// Release every key and button still held (viewer lost focus).
    ReleaseAll,
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("invalid message: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("not a Hext Remote control stream (bad preamble)")]
    BadPreamble,
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
            Control::Hello(Hello {
                peer: PeerInfo::local("0.1.0", "desk"),
                access_code: "123456789".into(),
            }),
            Control::RequestKeyframe,
            Control::HelloAck(PeerInfo::local("0.1.0", "laptop")),
            Control::Reject(RejectReason::Busy),
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
    fn video_and_input_roundtrip() {
        let frame = VideoFrame {
            seq: 7,
            width: 1920,
            height: 1080,
            keyframe: true,
            capture_micros: 1200,
            encode_micros: 4800,
            hardware: true,
            data: vec![0, 0, 0, 1, 0x67, 0xff],
        };
        assert_eq!(
            decode::<VideoFrame>(&encode(&frame).unwrap()).unwrap(),
            frame
        );

        let input = InputEvent::Key {
            scancode: 0xE04B,
            pressed: true,
        };
        assert_eq!(
            decode::<InputEvent>(&encode(&input).unwrap()).unwrap(),
            input
        );
    }

    #[test]
    fn stream_kinds() {
        for kind in [StreamKind::Video, StreamKind::Input] {
            assert_eq!(StreamKind::from_byte(kind as u8), Some(kind));
        }
        assert_eq!(StreamKind::from_byte(0), None);
    }

    #[test]
    fn preamble_roundtrip() {
        assert_eq!(decode_preamble(encode_preamble(513)).unwrap(), 513);
        assert!(matches!(
            decode_preamble(*b"GET "),
            Err(CodecError::BadPreamble)
        ));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(decode::<Control>(&[0xff, 0xff, 0xff]).is_err());
    }
}
