//! Hext Remote transport, over [`iroh`] (QUIC).
//!
//! Each endpoint's identity is its device key: a viewer connects to a host
//! by the host's key (and its addresses), and both ends prove they hold
//! their keys in the handshake. `iroh` opens a direct path when the
//! networks allow it (hole punching) and otherwise goes through the relay;
//! either way the session is end-to-end encrypted between the two devices.

mod endpoint;
mod session;
pub mod timer;
pub mod video;

pub use endpoint::{
    EndpointConfig, PathInfo, addr_from_strings, addr_to_strings, bind, connect, endpoint_id, path,
    remote_key,
};
pub use session::{
    ControlStream, FramedRecv, FramedSend, accept_uni, client_handshake, host_handshake, open_uni,
    recv_datagram, send_datagram,
};

pub use iroh::endpoint::{Connection, ConnectionError, Incoming, VarInt};
pub use iroh::{Endpoint, EndpointAddr, RelayUrl};

use hext_proto::{CodecError, RejectReason};
use iroh::endpoint::{ReadError, SendDatagramError, WriteError};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("abrir a conexão de rede: {0}")]
    Bind(String),
    #[error("conectar: {0}")]
    Connect(String),
    #[error("connection: {0}")]
    Connection(#[from] ConnectionError),
    #[error("handshake: {0}")]
    Handshake(#[from] iroh::endpoint::ConnectingError),
    #[error("write: {0}")]
    Write(#[from] WriteError),
    #[error("read: {0}")]
    Read(#[from] ReadError),
    #[error("datagram: {0}")]
    Datagram(#[from] SendDatagramError),
    #[error("protocol: {0}")]
    Codec(#[from] CodecError),
    #[error("device key: {0}")]
    DeviceKey(#[from] hext_crypto::CryptoError),
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
