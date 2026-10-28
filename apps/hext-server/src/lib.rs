//! `hext-server`: gives each device its 9-digit ID, knows who is online and
//! relays connection requests between devices (signaling). It never sees a
//! session's content: devices connect to each other directly (or through a
//! relay), end-to-end encrypted with their own keys.
//!
//! It speaks plain HTTP; TLS is done in front of it (Caddy), so devices
//! reach it at `wss://…`. It also tells our `iroh-relay` which devices may
//! use it (`/v1/relay-access`): only registered ones.

pub mod db;
mod limits;
pub mod signal;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{ConnectInfo, State as Extract, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use hext_crypto::PublicKey;
use hext_signal::SIGNAL_PATH;
use tokio::net::TcpListener;

pub use db::Db;
pub use signal::State;

/// Largest message a device may send (they are all small).
const MAX_MESSAGE: usize = 16 * 1024;

/// Path the relay asks whether a device may use it.
pub const RELAY_ACCESS_PATH: &str = "/v1/relay-access";

/// How the server is reached.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Behind a proxy (Caddy) on the same machine: a device's address is
    /// the last one in `X-Forwarded-For`, which the proxy sets, not the
    /// connection's (always the proxy).
    pub behind_proxy: bool,
    /// The secret our `iroh-relay` presents when asking about a device
    /// (`Authorization: Bearer …`). `None`: the question is not answered.
    pub relay_token: Option<String>,
}

#[derive(Clone)]
struct App {
    state: Arc<State>,
    config: Config,
}

/// The HTTP routes: `/health`, the signaling WebSocket and the relay's
/// access check.
fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(SIGNAL_PATH, get(upgrade))
        .route(RELAY_ACCESS_PATH, post(relay_access))
        .with_state(app)
}

/// `iroh-relay`'s access hook: may the device in `X-Iroh-Endpoint-Id` (its
/// public key, in hex) use the relay? Yes (`200 true`) if it has an ID, so
/// the relay serves our devices and nobody else.
async fn relay_access(
    headers: HeaderMap,
    Extract(app): Extract<App>,
) -> (StatusCode, &'static str) {
    let Some(token) = &app.config.relay_token else {
        return (StatusCode::NOT_FOUND, "");
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !same_secret(presented, token) {
        return (StatusCode::UNAUTHORIZED, "");
    }
    let Some(key) = headers
        .get("x-iroh-endpoint-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<PublicKey>().ok())
    else {
        return (StatusCode::BAD_REQUEST, "");
    };
    let db = app.state.db.clone();
    match tokio::task::spawn_blocking(move || db.is_registered(&key)).await {
        Ok(Ok(true)) => (StatusCode::OK, "true"),
        Ok(Ok(false)) => (StatusCode::FORBIDDEN, "false"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, ""),
    }
}

async fn upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Extract(app): Extract<App>,
) -> impl IntoResponse {
    let ip = client_ip(&app.config, peer, &headers);
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move { signal::handle(socket, &app.state, ip).await })
}

/// Compares secrets in constant time (no early exit on the first difference).
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The device's address, for the per-address limits.
fn client_ip(config: &Config, peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    let forwarded = || {
        let header = headers.get("x-forwarded-for")?.to_str().ok()?;
        header.rsplit(',').next()?.trim().parse().ok()
    };
    if config.behind_proxy {
        forwarded().unwrap_or(peer.ip())
    } else {
        peer.ip()
    }
}

/// Serves on `listener` until the task is dropped.
pub async fn serve(
    listener: TcpListener,
    state: Arc<State>,
    config: Config,
) -> std::io::Result<()> {
    let expiry = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                state.expire(Instant::now()).await;
            }
        })
    };
    let app = router(App { state, config });
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await;
    expiry.abort();
    served
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "ab"));
        assert!(!same_secret("", "a"));
    }

    #[test]
    fn the_proxy_decides_the_address() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6, 203.0.113.7".parse().unwrap());
        let proxied = Config {
            behind_proxy: true,
            ..Default::default()
        };
        assert_eq!(
            client_ip(&proxied, peer, &headers).to_string(),
            "203.0.113.7"
        );
        // Without a proxy the header is the device's own claim: ignored.
        assert_eq!(client_ip(&Config::default(), peer, &headers), peer.ip());
        assert_eq!(client_ip(&proxied, peer, &HeaderMap::new()), peer.ip());
    }
}
