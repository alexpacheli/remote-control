//! Hext Remote transport over QUIC.
//!
//! The host presents a self-signed certificate; the client authenticates it
//! by pinning the certificate's SHA-256 [`Fingerprint`]. In F3 the
//! fingerprint will come from the signaling server (device keys); in the LAN
//! MVP it is shown by the host and typed into the client.

mod endpoint;
mod identity;
mod session;
mod timer;
mod verify;
pub mod video;

pub use endpoint::{client_endpoint, connect, host_endpoint};
pub use identity::{Fingerprint, Identity};
pub use session::{
    ControlStream, FramedRecv, FramedSend, accept_uni, client_handshake, host_handshake, open_uni,
    recv_datagram, send_datagram,
};

pub use quinn::{Connection, ConnectionError, Endpoint, Incoming, VarInt};

use hext_proto::{CodecError, RejectReason};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("connect: {0}")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] quinn::ConnectionError),
    #[error("write: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("read: {0}")]
    Read(#[from] quinn::ReadError),
    #[error("datagram: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("protocol: {0}")]
    Codec(#[from] CodecError),
    #[error("session rejected by host: {0:?}")]
    Rejected(RejectReason),
    #[error("incompatible protocol version: ours is {local}, the peer's is {remote}")]
    IncompatibleVersion { local: u16, remote: u16 },
    #[error("control stream closed by peer")]
    StreamClosed,
    #[error("handshake timed out")]
    Timeout,
    #[error("unexpected message: {0}")]
    Unexpected(String),
}

impl NetError {
    /// The peer ended the session on purpose (closed the connection or its
    /// streams), as opposed to a failure.
    pub fn is_peer_closed(&self) -> bool {
        use quinn::{ConnectionError, ReadError, SendDatagramError, WriteError};
        let closed = |e: &ConnectionError| matches!(e, ConnectionError::ApplicationClosed(_));
        match self {
            NetError::StreamClosed => true,
            NetError::Connection(e) => closed(e),
            NetError::Read(ReadError::ConnectionLost(e)) => closed(e),
            NetError::Write(WriteError::ConnectionLost(e)) => closed(e),
            NetError::Datagram(SendDatagramError::ConnectionLost(e)) => closed(e),
            _ => false,
        }
    }
}

pub type Result<T, E = NetError> = std::result::Result<T, E>;
