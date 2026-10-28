use std::net::SocketAddr;
use std::time::Duration;

use hext_crypto::{DeviceKey, PublicKey};
use iroh::endpoint::{IdleTimeout, QuicTransportConfig, presets};
use iroh::{EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey};

use crate::{Connection, Endpoint, NetError, Result};

/// Video datagrams waiting to be read: room for a few large keyframes.
const DATAGRAM_RECEIVE_BUFFER: usize = 4 * 1024 * 1024;

/// Video datagrams waiting to be sent. Small, so that when the network
/// can't keep up the host waits (and skips screen updates) instead of
/// queueing seconds of video: ~200 ms at 10 Mbps.
const DATAGRAM_SEND_BUFFER: usize = 256 * 1024;

/// A peer silent this long (keep-alives included) is gone.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How an endpoint reaches and is reached by others.
#[derive(Debug, Clone, Default)]
pub struct EndpointConfig {
    /// Relay for when no direct path opens (and to coordinate opening one).
    /// `None`: direct only (LAN, tests).
    pub relay: Option<RelayUrl>,
    /// Local address to listen on (e.g. a fixed port for connecting by IP);
    /// `None`: every interface, a random port.
    pub bind: Option<SocketAddr>,
    /// Accept sessions (a host). A viewer only connects.
    pub accept: bool,
}

/// An endpoint whose identity is this device's key: peers authenticate it
/// by that key (its ID in `iroh`), and it authenticates them by theirs.
pub async fn bind(key: &DeviceKey, config: &EndpointConfig) -> Result<Endpoint> {
    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            IdleTimeout::try_from(IDLE_TIMEOUT).expect("10 s fits a VarInt"),
        ))
        .datagram_receive_buffer_size(Some(DATAGRAM_RECEIVE_BUFFER))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        .build();
    // Minimal: no address lookup through third parties; peers learn each
    // other's addresses from our own signaling.
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(SecretKey::from_bytes(key.seed()))
        .transport_config(transport)
        .relay_mode(match &config.relay {
            Some(relay) => RelayMode::custom([relay.clone()]),
            None => RelayMode::Disabled,
        });
    if config.accept {
        builder = builder.alpns(vec![hext_proto::ALPN.to_vec()]);
    }
    if let Some(bind) = config.bind {
        builder = builder
            .bind_addr(bind)
            .map_err(|e| NetError::Unexpected(format!("endereço {bind}: {e}")))?;
    }
    builder
        .bind()
        .await
        .map_err(|e| NetError::Bind(e.to_string()))
}

/// Connects to the host `addr`, which must prove it holds the key in it.
pub async fn connect(endpoint: &Endpoint, addr: EndpointAddr) -> Result<Connection> {
    endpoint
        .connect(addr, hext_proto::ALPN)
        .await
        .map_err(|e| NetError::Connect(e.to_string()))
}

/// The device key a connection's peer proved it holds.
pub fn remote_key(conn: &Connection) -> PublicKey {
    PublicKey::from_bytes(*conn.remote_id().as_bytes())
}

/// The `iroh` ID of a device key.
pub fn endpoint_id(key: &PublicKey) -> Result<EndpointId> {
    EndpointId::from_bytes(key.as_bytes())
        .map_err(|_| NetError::Connect("a chave informada não é uma chave válida".into()))
}

/// Where a host can be reached, as text for signaling: `relay <url>` and
/// `ip <addr>` entries (short, so they fit the server's limits).
pub fn addr_to_strings(addr: &EndpointAddr) -> Vec<String> {
    let relays = addr.relay_urls().map(|url| format!("relay {url}"));
    let ips = addr.ip_addrs().map(|ip| format!("ip {ip}"));
    relays.chain(ips).collect()
}

/// The host with `key` at the addresses from [`addr_to_strings`] (unknown
/// entries are skipped).
pub fn addr_from_strings(key: &PublicKey, entries: &[String]) -> Result<EndpointAddr> {
    let mut addr = EndpointAddr::new(endpoint_id(key)?);
    for entry in entries {
        match entry.split_once(' ') {
            Some(("relay", url)) => {
                if let Ok(url) = url.parse::<RelayUrl>() {
                    addr = addr.with_relay_url(url);
                }
            }
            Some(("ip", ip)) => {
                if let Ok(ip) = ip.parse::<SocketAddr>() {
                    addr = addr.with_ip_addr(ip);
                }
            }
            _ => {}
        }
    }
    Ok(addr)
}

/// The path a connection is using right now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathInfo {
    /// Through the relay (otherwise direct, peer to peer).
    pub relay: bool,
    pub rtt: Duration,
}

/// The connection's selected path, if one is.
pub fn path(conn: &Connection) -> Option<PathInfo> {
    let paths = conn.paths();
    let selected = paths.iter().find(|p| p.is_selected())?;
    Some(PathInfo {
        relay: selected.is_relay(),
        rtt: selected.rtt(),
    })
}
