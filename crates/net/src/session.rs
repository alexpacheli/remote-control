use std::time::Duration;

use hext_proto::frame;
use hext_proto::{
    Control, Datagram, PREAMBLE_LEN, PROTOCOL_VERSION, PeerInfo, decode_preamble, encode_preamble,
};

use crate::{Connection, NetError, Result};

/// Upper bound for the whole handshake, so silent peers cannot hold a host task.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a peer that is about to fail or leave waits for the other side to
/// close, so its last message is delivered instead of discarded.
const LINGER: Duration = Duration::from_secs(2);

/// Largest chunk read from the control stream at once.
const READ_CHUNK: usize = 64 * 1024;

/// The reliable, ordered control stream of a session.
pub struct ControlStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    /// Bytes received but not yet consumed as a preamble or frame.
    buf: Vec<u8>,
}

impl ControlStream {
    fn new((send, recv): (quinn::SendStream, quinn::RecvStream)) -> Self {
        Self {
            send,
            recv,
            buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, msg: &Control) -> Result<()> {
        self.send.write_all(&frame::encode_frame(msg)?).await?;
        Ok(())
    }

    /// Receives the next message.
    ///
    /// Cancel-safe, so it can be a `tokio::select!` branch: bytes of a
    /// partially received frame stay buffered for the next call.
    pub async fn recv(&mut self) -> Result<Control> {
        loop {
            if let Some(msg) = frame::take_frame(&mut self.buf)? {
                return Ok(msg);
            }
            self.fill().await?;
        }
    }

    /// Sends [`Control::Bye`] and waits briefly for the peer to close the
    /// connection, so the Bye is not discarded by our own close.
    pub async fn bye(mut self, conn: &Connection) -> Result<()> {
        self.send(&Control::Bye).await?;
        // Already-closed streams are fine: the peer may have left first.
        let _ = self.send.finish();
        let _ = tokio::time::timeout(LINGER, conn.closed()).await;
        Ok(())
    }

    async fn send_preamble(&mut self) -> Result<()> {
        self.send
            .write_all(&encode_preamble(PROTOCOL_VERSION))
            .await?;
        Ok(())
    }

    async fn recv_preamble(&mut self) -> Result<u16> {
        while self.buf.len() < PREAMBLE_LEN {
            self.fill().await?;
        }
        let bytes: [u8; PREAMBLE_LEN] =
            self.buf[..PREAMBLE_LEN].try_into().expect("length checked");
        self.buf.drain(..PREAMBLE_LEN);
        Ok(decode_preamble(bytes)?)
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

/// Client side: opens the control stream and introduces itself.
pub async fn client_handshake(
    conn: &Connection,
    local: PeerInfo,
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
        control.send(&Control::Hello(local)).await?;
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
pub async fn host_handshake(
    conn: &Connection,
    local: PeerInfo,
) -> Result<(ControlStream, PeerInfo)> {
    with_timeout(async {
        let mut control = ControlStream::new(conn.accept_bi().await?);
        let remote = control.recv_preamble().await?;
        // Always answer with our version, so the client can report a mismatch.
        control.send_preamble().await?;
        if remote != PROTOCOL_VERSION {
            let _ = control.send.finish();
            let _ = tokio::time::timeout(LINGER, conn.closed()).await;
            return Err(NetError::IncompatibleVersion {
                local: PROTOCOL_VERSION,
                remote,
            });
        }
        let client = match control.recv().await? {
            Control::Hello(client) => client,
            other => return Err(NetError::Unexpected(format!("{other:?} instead of Hello"))),
        };
        control.send(&Control::HelloAck(local)).await?;
        Ok((control, client))
    })
    .await
}

async fn with_timeout<T>(fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, fut)
        .await
        .map_err(|_| NetError::Timeout)?
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
