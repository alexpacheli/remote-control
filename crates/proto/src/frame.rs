//! Length-prefixed framing for the control stream.
//!
//! Each frame is a little-endian `u32` payload length followed by the
//! postcard-encoded message.

use serde::{Deserialize, Serialize};

use crate::{CodecError, decode, encode};

/// Size of the length prefix.
pub const HEADER_LEN: usize = 4;

/// Upper bound for a control frame; larger payloads (files) go on their own streams.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Encodes `msg` as a complete frame (header + payload).
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, CodecError> {
    let payload = encode(msg)?;
    check_len(payload.len())?;
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// Parses a frame header, returning the payload length that follows it.
pub fn payload_len(header: [u8; HEADER_LEN]) -> Result<usize, CodecError> {
    let len = u32::from_le_bytes(header) as usize;
    check_len(len)?;
    Ok(len)
}

/// Decodes a frame payload (without its header).
pub fn decode_payload<'a, T: Deserialize<'a>>(payload: &'a [u8]) -> Result<T, CodecError> {
    decode(payload)
}

fn check_len(len: usize) -> Result<(), CodecError> {
    if len > MAX_FRAME_LEN {
        return Err(CodecError::FrameTooLarge {
            len,
            max: MAX_FRAME_LEN,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Control;

    #[test]
    fn frame_roundtrip() {
        let frame = encode_frame(&Control::Bye).unwrap();
        let len = payload_len(frame[..HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - HEADER_LEN);
        let msg: Control = decode_payload(&frame[HEADER_LEN..]).unwrap();
        assert_eq!(msg, Control::Bye);
    }

    #[test]
    fn oversized_header_is_rejected() {
        let header = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes();
        assert!(matches!(
            payload_len(header),
            Err(CodecError::FrameTooLarge { .. })
        ));
    }
}
