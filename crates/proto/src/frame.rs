//! Length-prefixed framing for the control stream.
//!
//! Each frame is a little-endian `u32` payload length followed by the
//! postcard-encoded message.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{CodecError, decode};

/// Size of the length prefix.
pub const HEADER_LEN: usize = 4;

/// Upper bound for control and input frames.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;

/// Encodes `msg` as a complete frame (header + payload) of at most `max` payload bytes.
pub fn encode_frame<T: Serialize>(msg: &T, max: usize) -> Result<Vec<u8>, CodecError> {
    // Serialize after a placeholder header, so large (video) payloads are
    // written once instead of being copied behind the header.
    let mut out = postcard::to_extend(msg, vec![0; HEADER_LEN])?;
    let len = out.len() - HEADER_LEN;
    check_len(len, max)?;
    out[..HEADER_LEN].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(out)
}

/// Removes and decodes the first complete frame at the start of `buf`,
/// rejecting payloads larger than `max`.
///
/// Returns `Ok(None)` while the frame is still incomplete, leaving `buf`
/// untouched so the caller can append more bytes and try again.
pub fn take_frame<T: DeserializeOwned>(
    buf: &mut Vec<u8>,
    max: usize,
) -> Result<Option<T>, CodecError> {
    let Some(header) = buf.first_chunk::<HEADER_LEN>() else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(*header) as usize;
    check_len(len, max)?;
    let end = HEADER_LEN + len;
    if buf.len() < end {
        return Ok(None);
    }
    let msg = decode(&buf[HEADER_LEN..end])?;
    buf.drain(..end);
    Ok(Some(msg))
}

fn check_len(len: usize, max: usize) -> Result<(), CodecError> {
    if len > max {
        return Err(CodecError::FrameTooLarge { len, max });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Control, PeerInfo};

    #[test]
    fn whole_frame() {
        let mut buf = encode_frame(&Control::Bye, MAX_FRAME_LEN).unwrap();
        assert_eq!(
            take_frame::<Control>(&mut buf, MAX_FRAME_LEN).unwrap(),
            Some(Control::Bye)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn frame_arriving_in_pieces() {
        let msg = Control::HelloAck(PeerInfo::local("0.1.0", "desk"));
        let frame = encode_frame(&msg, MAX_FRAME_LEN).unwrap();
        let mut buf = Vec::new();
        for (i, byte) in frame.iter().enumerate() {
            buf.push(*byte);
            let taken = take_frame::<Control>(&mut buf, MAX_FRAME_LEN).unwrap();
            if i + 1 < frame.len() {
                assert_eq!(taken, None, "frame complete too early at byte {i}");
            } else {
                assert_eq!(taken, Some(msg.clone()));
            }
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn two_frames_in_one_buffer() {
        let mut buf = encode_frame(&Control::Bye, MAX_FRAME_LEN).unwrap();
        let second = Control::HelloAck(PeerInfo::local("0.1.0", "desk"));
        buf.extend(encode_frame(&second, MAX_FRAME_LEN).unwrap());
        assert_eq!(
            take_frame::<Control>(&mut buf, MAX_FRAME_LEN).unwrap(),
            Some(Control::Bye)
        );
        assert_eq!(
            take_frame::<Control>(&mut buf, MAX_FRAME_LEN).unwrap(),
            Some(second)
        );
        assert_eq!(
            take_frame::<Control>(&mut buf, MAX_FRAME_LEN).unwrap(),
            None
        );
    }

    #[test]
    fn oversized_header_is_rejected() {
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_le_bytes().to_vec();
        assert!(matches!(
            take_frame::<Control>(&mut buf, MAX_FRAME_LEN),
            Err(CodecError::FrameTooLarge { .. })
        ));
    }
}
