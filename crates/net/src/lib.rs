//! Hext Remote transport over QUIC.
//!
//! The host presents a self-signed certificate; the client authenticates it
//! by pinning the certificate's SHA-256 [`Fingerprint`]. In F3 the
//! fingerprint will come from the signaling server (device keys); in the LAN
//! MVP it is shown by the host and typed into the client.

mod endpoint;
mod identity;
mod session;
mod verify;

pub use endpoint::{client_endpoint, connect, host_endpoint};
pub use identity::{Fingerprint, Identity};
pub use session::{ControlStream, client_handshake, host_handshake, recv_datagram, send_datagram};

pub use quinn::{Connection, Endpoint, Incoming};

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
    Read(#[from] quinn::ReadExactError),
    #[error("datagram: {0}")]
    Datagram(#[from] quinn::SendDatagramError),
    #[error("protocol: {0}")]
    Codec(#[from] CodecError),
    #[error("session rejected by host: {0:?}")]
    Rejected(RejectReason),
    #[error("unexpected message: {0}")]
    Unexpected(String),
}

pub type Result<T, E = NetError> = std::result::Result<T, E>;
