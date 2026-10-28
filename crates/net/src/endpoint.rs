use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::crypto::CryptoProvider;

use crate::verify::PinnedVerifier;
use crate::{Connection, Endpoint, Fingerprint, Identity, NetError, Result};

/// SNI sent by clients. Hosts are authenticated by fingerprint, not by name.
const SERVER_NAME: &str = "hext";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Video datagrams waiting to be read: room for a few large keyframes.
const DATAGRAM_RECEIVE_BUFFER: usize = 4 * 1024 * 1024;

/// Video datagrams waiting to be sent. Small, so that when the network
/// can't keep up the host waits (and skips screen updates) instead of
/// queueing seconds of video: ~200 ms at 10 Mbps.
const DATAGRAM_SEND_BUFFER: usize = 256 * 1024;

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(10)).expect("10s fits a VarInt"),
    ));
    t.datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER));
    t.datagram_send_buffer_size(DATAGRAM_SEND_BUFFER);
    Arc::new(t)
}

/// Endpoint that accepts sessions, bound to `bind`.
pub fn host_endpoint(bind: SocketAddr, identity: &Identity) -> Result<Endpoint> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.clone()], identity.key_der())?;
    tls.alpn_protocols = vec![hext_proto::ALPN.to_vec()];

    let crypto =
        QuicServerConfig::try_from(tls).map_err(|e| NetError::Unexpected(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    Ok(Endpoint::server(config, bind)?)
}

/// Endpoint for outgoing sessions to `remote`, on an ephemeral port of the
/// same address family (dual-stack sockets are not the default everywhere).
pub fn client_endpoint(remote: SocketAddr) -> Result<Endpoint> {
    let bind: SocketAddr = match remote {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    Ok(Endpoint::client(bind)?)
}

/// Connects to a host, accepting it only if its certificate matches `pin`.
pub async fn connect(
    endpoint: &Endpoint,
    addr: SocketAddr,
    pin: Option<Fingerprint>,
) -> Result<Connection> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier::new(pin, provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![hext_proto::ALPN.to_vec()];

    let crypto =
        QuicClientConfig::try_from(tls).map_err(|e| NetError::Unexpected(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    Ok(endpoint.connect_with(config, addr, SERVER_NAME)?.await?)
}
