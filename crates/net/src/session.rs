use std::time::Duration;

use hext_proto::frame::{self, MAX_FRAME_LEN};
use hext_proto::{
    Control, Datagram, Hello, PREAMBLE_LEN, PROTOCOL_VERSION, PeerInfo, RejectReason, StreamKind,
    decode_preamble, encode_preamble,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Connection, NetError, Result};

/// Upper bound for the whole handshake, so silent peers cannot hold a host task.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a peer that is about to fail or leave waits for the other side to
/// close, so its last message is delivered instead of discarded.
const LINGER: Duration = Duration::from_secs(2);

/// Largest chunk read from a stream at once.
const READ_CHUNK: usize = 64 * 1024;

/// Sending half of a stream of length-prefixed postcard messages.
pub struct FramedSend {
    send: quinn::SendStream,
    max: usize,
}

impl FramedSend {
    fn new(send: quinn::SendStream, max: usize) -> Self {
        Self { send, max }
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        self.send
            .write_all(&frame::encode_frame(msg, self.max)?)
            .await?;
        Ok(())
    }

    /// Closes our half of the stream. Already-closed streams are fine.
    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }
}

/// Receiving half of a stream of length-prefixed postcard messages.
pub struct FramedRecv {
    recv: quinn::RecvStream,
    max: usize,
    /// Bytes received but not yet consumed as a preamble or frame.
    buf: Vec<u8>,
}

impl FramedRecv {
    fn new(recv: quinn::RecvStream, max: usize) -> Self {
        Self {
            recv,
            max,
            buf: Vec::new(),
        }
    }

    /// Receives the next message.
    ///
    /// Cancel-safe, so it can be a `tokio::select!` branch: bytes of a
    /// partially received frame stay buffered for the next call.
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<T> {
        loop {
            if let Some(msg) = frame::take_frame(&mut self.buf, self.max)? {
                return Ok(msg);
            }
            self.fill().await?;
        }
    }

    async fn recv_bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        while self.buf.len() < N {
            self.fill().await?;
        }
        let bytes: [u8; N] = self.buf[..N].try_into().expect("length checked");
        self.buf.drain(..N);
        Ok(bytes)
    }

    /// Appends the next chunk of the stream to `buf`. Cancel-safe.
    async fn fill(&mut self) -> Result<()> {
        match self.recv.read_chunk(READ_CHUNK, true).await? {
            Some(chunk) => {
                self.buf.extend_from_slice(&chunk.bytes);
                Ok(())
            }
            None => Err(NetError::StreamClosed),
        }
    }
}

/// The reliable, ordered control stream of a session.
pub struct ControlStream {
    send: FramedSend,
    recv: FramedRecv,
}

impl ControlStream {
    fn new((send, recv): (quinn::SendStream, quinn::RecvStream)) -> Self {
        Self {
            send: FramedSend::new(send, MAX_FRAME_LEN),
            recv: FramedRecv::new(recv, MAX_FRAME_LEN),
        }
    }

    pub async fn send(&mut self, msg: &Control) -> Result<()> {
        self.send.send(msg).await
    }

    /// Receives the next message. Cancel-safe (see [`FramedRecv::recv`]).
    pub async fn recv(&mut self) -> Result<Control> {
        self.recv.recv().await
    }

    /// Sends [`Control::Bye`] and waits briefly for the peer to close the
    /// connection, so the Bye is not discarded by our own close.
    pub async fn bye(mut self, conn: &Connection) -> Result<()> {
        self.send(&Control::Bye).await?;
        self.send.finish();
        let _ = tokio::time::timeout(LINGER, conn.closed()).await;
        Ok(())
    }

    async fn send_preamble(&mut self) -> Result<()> {
        self.send
            .send
            .write_all(&encode_preamble(PROTOCOL_VERSION))
            .await?;
        Ok(())
    }

    async fn recv_preamble(&mut self) -> Result<u16> {
        let bytes = self.recv.recv_bytes::<PREAMBLE_LEN>().await?;
        Ok(decode_preamble(bytes)?)
    }
}

/// Client side: opens the control stream and introduces itself.
pub async fn client_handshake(
    conn: &Connection,
    hello: Hello,
) -> Result<(ControlStream, PeerInfo)> {
    with_timeout(async {
        let mut control = ControlStream::new(conn.open_bi().await?);
        control.send_preamble().await?;
        let remote = control.recv_preamble().await?;
        if remote != PROTOCOL_VERSION {
            conn.close(0u32.into(), b"incompatible version");
            return Err(NetError::IncompatibleVersion {
                local: PROTOCOL_VERSION,
                remote,
            });
        }
        control.send(&Control::Hello(hello)).await?;
        match control.recv().await? {
            Control::HelloAck(host) => Ok((control, host)),
            Control::Reject(reason) => {
                conn.close(0u32.into(), b"rejected");
                Err(NetError::Rejected(reason))
            }
            other => Err(NetError::Unexpected(format!(
                "{other:?} instead of HelloAck"
            ))),
        }
    })
    .await
}

/// Host side: accepts the control stream and validates the client.
///
/// `validate` decides whether the [`Hello`] is admitted; a rejection is sent
/// to the client before the error is returned.
pub async fn host_handshake(
    conn: &Connection,
    local: PeerInfo,
    validate: impl FnOnce(&Hello) -> Result<(), RejectReason>,
) -> Result<(ControlStream, Hello)> {
    with_timeout(async {
        let mut control = ControlStream::new(conn.accept_bi().await?);
        let remote = control.recv_preamble().await?;
        // Always answer with our version, so the client can report a mismatch.
        control.send_preamble().await?;
        if remote != PROTOCOL_VERSION {
            control.send.finish();
            let _ = tokio::time::timeout(LINGER, conn.closed()).await;
            return Err(NetError::IncompatibleVersion {
                local: PROTOCOL_VERSION,
                remote,
            });
        }
        let hello = match control.recv().await? {
            Control::Hello(hello) => hello,
            other => return Err(NetError::Unexpected(format!("{other:?} instead of Hello"))),
        };
        if let Err(reason) = validate(&hello) {
            control.send(&Control::Reject(reason)).await?;
            control.send.finish();
            let _ = tokio::time::timeout(LINGER, conn.closed()).await;
            return Err(NetError::Rejected(reason));
        }
        control.send(&Control::HelloAck(local)).await?;
        Ok((control, hello))
    })
    .await
}

async fn with_timeout<T>(fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .map_err(|_| NetError::Timeout)?
}

/// Opens a unidirectional stream of the given kind.
pub async fn open_uni(conn: &Connection, kind: StreamKind) -> Result<FramedSend> {
    let mut send = conn.open_uni().await?;
    send.write_all(&[kind as u8]).await?;
    Ok(FramedSend::new(send, kind.max_frame_len()))
}

/// Accepts the next unidirectional stream opened by the peer.
pub async fn accept_uni(conn: &Connection) -> Result<(StreamKind, FramedRecv)> {
    let recv = conn.accept_uni().await?;
    let mut framed = FramedRecv::new(recv, frame::MAX_FRAME_LEN);
    let [byte] = framed.recv_bytes::<1>().await?;
    let kind = StreamKind::from_byte(byte)
        .ok_or_else(|| NetError::Unexpected(format!("unknown stream kind {byte}")))?;
    framed.max = kind.max_frame_len();
    Ok((kind, framed))
}

pub fn send_datagram(conn: &Connection, msg: &Datagram) -> Result<()> {
    conn.send_datagram(hext_proto::encode(msg)?.into())?;
    Ok(())
}

/// Receives the next datagram. A [`NetError::Codec`] error means only that
/// datagram was malformed; the connection is still usable.
pub async fn recv_datagram(conn: &Connection) -> Result<Datagram> {
    let bytes = conn.read_datagram().await?;
    Ok(hext_proto::decode(&bytes)?)
}
