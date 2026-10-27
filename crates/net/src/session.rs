use std::time::Duration;

use hext_proto::frame::{self, HEADER_LEN};
use hext_proto::{Control, Datagram, PROTOCOL_VERSION, PeerInfo, RejectReason};

use crate::{Connection, NetError, Result};

/// How long a host keeps a rejected connection open so the reason arrives.
const REJECT_LINGER: Duration = Duration::from_secs(2);

/// The reliable, ordered control stream of a session.
pub struct ControlStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}

impl ControlStream {
    pub async fn send(&mut self, msg: &Control) -> Result<()> {
        self.send.write_all(&frame::encode_frame(msg)?).await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<Control> {
        let mut header = [0; HEADER_LEN];
        self.recv.read_exact(&mut header).await?;
        let mut payload = vec![0; frame::payload_len(header)?];
        self.recv.read_exact(&mut payload).await?;
        Ok(frame::decode_payload(&payload)?)
    }

    /// Sends [`Control::Bye`] and closes our half of the stream.
    pub async fn bye(mut self) -> Result<()> {
        self.send(&Control::Bye).await?;
        // Already-closed streams are fine: the peer may have left first.
        let _ = self.send.finish();
        Ok(())
    }
}

/// Client side: opens the control stream and introduces itself.
pub async fn client_handshake(
    conn: &Connection,
    local: PeerInfo,
) -> Result<(ControlStream, PeerInfo)> {
    let (send, recv) = conn.open_bi().await?;
    let mut control = ControlStream { send, recv };
    control.send(&Control::Hello(local)).await?;
    match control.recv().await? {
        Control::HelloAck(host) => Ok((control, host)),
        Control::Reject(reason) => {
            // Lets the host stop waiting for us (see `host_handshake`).
            conn.close(0u32.into(), b"rejected");
            Err(NetError::Rejected(reason))
        }
        other => Err(NetError::Unexpected(format!(
            "{other:?} instead of HelloAck"
        ))),
    }
}

/// Host side: accepts the control stream and validates the client's hello.
pub async fn host_handshake(
    conn: &Connection,
    local: PeerInfo,
) -> Result<(ControlStream, PeerInfo)> {
    let (send, recv) = conn.accept_bi().await?;
    let mut control = ControlStream { send, recv };
    let client = match control.recv().await? {
        Control::Hello(client) => client,
        other => return Err(NetError::Unexpected(format!("{other:?} instead of Hello"))),
    };
    if client.protocol_version != PROTOCOL_VERSION {
        let reason = RejectReason::IncompatibleVersion {
            host_version: PROTOCOL_VERSION,
        };
        control.send(&Control::Reject(reason)).await?;
        let _ = control.send.finish();
        // Dropping the connection now could discard the Reject before it is
        // delivered; give the client a moment to read it and close.
        let _ = tokio::time::timeout(REJECT_LINGER, conn.closed()).await;
        return Err(NetError::Rejected(reason));
    }
    control.send(&Control::HelloAck(local)).await?;
    Ok((control, client))
}

pub fn send_datagram(conn: &Connection, msg: &Datagram) -> Result<()> {
    conn.send_datagram(hext_proto::encode(msg)?.into())?;
    Ok(())
}

pub async fn recv_datagram(conn: &Connection) -> Result<Datagram> {
    let bytes = conn.read_datagram().await?;
    Ok(hext_proto::decode(&bytes)?)
}
