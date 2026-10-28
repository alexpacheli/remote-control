//! `hext-server`: gives each device its 9-digit ID, knows who is online and
//! relays connection requests between devices (signaling). It never sees a
//! session's content: devices connect to each other directly (or through a
//! relay), end-to-end encrypted with their own keys.
//!
//! It speaks plain HTTP; TLS is done in front of it (Caddy), so devices
//! reach it at `wss://…`.

pub mod db;
pub mod signal;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{State as Extract, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use hext_signal::SIGNAL_PATH;
use tokio::net::TcpListener;

pub use db::Db;
pub use signal::State;

/// Largest message a device may send (they are all small).
const MAX_MESSAGE: usize = 16 * 1024;

/// The HTTP routes: `/health` and the signaling WebSocket.
pub fn router(state: Arc<State>) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(SIGNAL_PATH, get(upgrade))
        .with_state(state)
}

async fn upgrade(ws: WebSocketUpgrade, Extract(state): Extract<Arc<State>>) -> impl IntoResponse {
    ws.max_message_size(MAX_MESSAGE)
        .max_frame_size(MAX_MESSAGE)
        .on_upgrade(move |socket| async move { signal::handle(socket, &state).await })
}

/// Serves on `listener` until the task is dropped.
pub async fn serve(listener: TcpListener, state: Arc<State>) -> std::io::Result<()> {
    let expiry = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                state.expire(Instant::now());
            }
        })
    };
    let served = axum::serve(listener, router(state)).await;
    expiry.abort();
    served
}
