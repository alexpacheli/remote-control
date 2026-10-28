//! One WebSocket per device: login by signed challenge, then relaying
//! connection requests between devices that are online.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use hext_crypto::PublicKey;
use hext_signal::{
    ClientMsg, DeviceId, MAX_NAME_LEN, NONCE_LEN, PROTOCOL, Rejection, ServerMsg, auth_message,
};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::db::Db;

/// How long a device has to answer the challenge.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host's user has to answer a request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests a device may make per [`RATE_WINDOW`] (limits ID guessing).
const RATE_LIMIT: usize = 10;
const RATE_WINDOW: Duration = Duration::from_secs(60);
/// Requests waiting on one host at once.
const MAX_PENDING_PER_HOST: usize = 5;
/// The server pings this often; a device silent for [`IDLE_TIMEOUT`] is gone.
const PING_INTERVAL: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Messages queued for one device before more are dropped.
const OUTBOX: usize = 32;

/// Who is online and which requests are waiting.
pub struct State {
    pub db: Db,
    peers: Mutex<HashMap<DeviceId, Peer>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    rng: SystemRandom,
}

/// A logged-in device.
struct Peer {
    /// Tells this login apart from a newer one of the same device.
    session: u64,
    outbox: mpsc::Sender<ServerMsg>,
}

/// A request waiting for the host's answer.
struct Pending {
    viewer: DeviceId,
    viewer_session: u64,
    /// The viewer's own number for it.
    viewer_request: u64,
    host: DeviceId,
    host_session: u64,
    expires: Instant,
}

impl State {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            peers: Mutex::default(),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
            rng: SystemRandom::new(),
        }
    }

    /// Number of devices online.
    pub fn online(&self) -> usize {
        self.peers.lock().expect("lock").len()
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Queues `msg` for `id`'s login `session` (any login if `None`);
    /// false if it is not online.
    fn send_to(&self, id: DeviceId, session: Option<u64>, msg: ServerMsg) -> bool {
        let peers = self.peers.lock().expect("lock");
        match peers.get(&id) {
            Some(peer) if session.is_none_or(|s| s == peer.session) => {
                // A device too slow to read its messages misses some.
                peer.outbox.try_send(msg).is_ok()
            }
            _ => false,
        }
    }

    fn audit(&self, from: DeviceId, to: DeviceId, event: &str) {
        if let Err(e) = self.db.audit(from, to, event) {
            warn!("auditoria: {e}");
        }
    }

    /// Ends requests whose host never answered. Call every second or so.
    pub fn expire(&self, now: Instant) {
        let expired: Vec<(u64, Pending)> = {
            let mut pending = self.pending.lock().expect("lock");
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, p)| p.expires <= now)
                .map(|(&id, _)| id)
                .collect();
            ids.into_iter()
                .filter_map(|id| pending.remove(&id).map(|p| (id, p)))
                .collect()
        };
        for (request, p) in expired {
            self.audit(p.viewer, p.host, "timeout");
            self.send_to(
                p.viewer,
                Some(p.viewer_session),
                ServerMsg::Rejected {
                    request: p.viewer_request,
                    reason: Rejection::Timeout,
                },
            );
            self.send_to(
                p.host,
                Some(p.host_session),
                ServerMsg::Cancelled { request },
            );
        }
    }

    /// A login ended: forget it and the requests it was part of.
    fn leave(&self, id: DeviceId, session: u64) {
        {
            let mut peers = self.peers.lock().expect("lock");
            if peers.get(&id).is_some_and(|p| p.session == session) {
                peers.remove(&id);
            }
        }
        let gone: Vec<(u64, Pending)> = {
            let mut pending = self.pending.lock().expect("lock");
            let ids: Vec<u64> = pending
                .iter()
                .filter(|(_, p)| {
                    (p.viewer, p.viewer_session) == (id, session)
                        || (p.host, p.host_session) == (id, session)
                })
                .map(|(&r, _)| r)
                .collect();
            ids.into_iter()
                .filter_map(|r| pending.remove(&r).map(|p| (r, p)))
                .collect()
        };
        for (request, p) in gone {
            if p.viewer == id {
                self.audit(p.viewer, p.host, "cancelled");
                self.send_to(
                    p.host,
                    Some(p.host_session),
                    ServerMsg::Cancelled { request },
                );
            } else {
                self.audit(p.viewer, p.host, "host_left");
                self.send_to(
                    p.viewer,
                    Some(p.viewer_session),
                    ServerMsg::Rejected {
                        request: p.viewer_request,
                        reason: Rejection::Offline,
                    },
                );
            }
        }
    }
}

/// A logged-in device's view of its own connection.
struct Session {
    id: DeviceId,
    session: u64,
    key: PublicKey,
    name: String,
    /// When its recent requests were made.
    recent: VecDeque<Instant>,
}

/// Serves one device's WebSocket until it leaves.
pub async fn handle(socket: WebSocket, state: &State) {
    let (mut sink, mut stream) = socket.split();

    let mut nonce = [0; NONCE_LEN];
    if state.rng.fill(&mut nonce).is_err() {
        return;
    }
    if !send(
        &mut sink,
        &ServerMsg::Challenge {
            protocol: PROTOCOL,
            nonce,
        },
    )
    .await
    {
        return;
    }
    let hello = tokio::time::timeout(LOGIN_TIMEOUT, next_msg(&mut stream)).await;
    let (key, name, version) = match hello {
        Ok(Some(Ok(ClientMsg::Hello {
            protocol,
            public_key,
            signature,
            device_name,
            app_version,
        }))) => {
            if protocol != PROTOCOL {
                let message = format!(
                    "versão do protocolo de sinalização incompatível: a do servidor é {PROTOCOL}, a sua é {protocol}"
                );
                send(&mut sink, &ServerMsg::Error { message }).await;
                return;
            }
            if !public_key.verify(&auth_message(&nonce), &signature) {
                let message = "assinatura inválida".to_owned();
                send(&mut sink, &ServerMsg::Error { message }).await;
                return;
            }
            (public_key, clean(&device_name), clean(&app_version))
        }
        // Silence, garbage or another message first: no login.
        _ => return,
    };
    let id = match state.db.login(&key, &name, &version) {
        Ok(id) => id,
        Err(e) => {
            warn!("banco de dados: {e}");
            let message = "erro no servidor".to_owned();
            send(&mut sink, &ServerMsg::Error { message }).await;
            return;
        }
    };

    let session = state.next_id();
    let (outbox, mut inbox) = mpsc::channel(OUTBOX);
    let replaced = state
        .peers
        .lock()
        .expect("lock")
        .insert(id, Peer { session, outbox });
    if let Some(old) = replaced {
        // The same device logged in again (it reconnected): the old
        // connection is closed.
        let _ = old.outbox.try_send(ServerMsg::Error {
            message: "este computador conectou de novo em outra conexão".to_owned(),
        });
    }
    info!("online: {id} ({name}, {version})");

    let mut me = Session {
        id,
        session,
        key,
        name: name.clone(),
        recent: VecDeque::new(),
    };
    if send(&mut sink, &ServerMsg::Welcome { id }).await {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let mut heard = Instant::now();
        loop {
            tokio::select! {
                incoming = stream.next() => {
                    heard = Instant::now();
                    let msg = match incoming {
                        Some(Ok(Message::Text(text))) => serde_json::from_str::<ClientMsg>(&text),
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                        Some(Ok(_)) => continue, // pings, pongs
                    };
                    let reply = match msg {
                        Ok(msg) => on_message(state, &mut me, msg),
                        Err(_) => Some(ServerMsg::Error { message: "mensagem inválida".to_owned() }),
                    };
                    if let Some(reply) = reply {
                        let fatal = matches!(reply, ServerMsg::Error { .. });
                        if !send(&mut sink, &reply).await || fatal {
                            break;
                        }
                    }
                }
                Some(msg) = inbox.recv() => {
                    let fatal = matches!(msg, ServerMsg::Error { .. });
                    if !send(&mut sink, &msg).await || fatal {
                        break;
                    }
                }
                _ = ping.tick() => {
                    if heard.elapsed() > IDLE_TIMEOUT
                        || sink.send(Message::Ping(Default::default())).await.is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
    state.leave(id, session);
    info!("offline: {id}");
    let _ = sink.close().await;
}

type Sink = futures_util::stream::SplitSink<WebSocket, Message>;

/// Sends `msg`; false once the connection is gone.
async fn send(sink: &mut Sink, msg: &ServerMsg) -> bool {
    sink.send(Message::text(msg.to_json())).await.is_ok()
}

/// Handles a message from a logged-in device; returns its direct reply.
fn on_message(state: &State, me: &mut Session, msg: ClientMsg) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Hello { .. } => Some(ServerMsg::Error {
            message: "já conectado".to_owned(),
        }),
        ClientMsg::Connect { request, to } => {
            let reject = |reason| Some(ServerMsg::Rejected { request, reason });
            let now = Instant::now();
            while me
                .recent
                .front()
                .is_some_and(|&t| now.duration_since(t) > RATE_WINDOW)
            {
                me.recent.pop_front();
            }
            if me.recent.len() >= RATE_LIMIT {
                state.audit(me.id, to, "rate_limited");
                return reject(Rejection::TooManyRequests);
            }
            me.recent.push_back(now);
            if to == me.id {
                return reject(Rejection::SelfConnect);
            }
            let Some(host_session) = state
                .peers
                .lock()
                .expect("lock")
                .get(&to)
                .map(|p| p.session)
            else {
                state.audit(me.id, to, "offline");
                return reject(Rejection::Offline);
            };
            let server_request = state.next_id();
            {
                let mut pending = state.pending.lock().expect("lock");
                if pending.values().filter(|p| p.host == to).count() >= MAX_PENDING_PER_HOST {
                    drop(pending);
                    state.audit(me.id, to, "busy");
                    return reject(Rejection::Busy);
                }
                pending.insert(
                    server_request,
                    Pending {
                        viewer: me.id,
                        viewer_session: me.session,
                        viewer_request: request,
                        host: to,
                        host_session,
                        expires: now + REQUEST_TIMEOUT,
                    },
                );
            }
            let asked = state.send_to(
                to,
                Some(host_session),
                ServerMsg::Request {
                    request: server_request,
                    from: me.id,
                    from_key: me.key,
                    from_name: me.name.clone(),
                },
            );
            if !asked {
                state.pending.lock().expect("lock").remove(&server_request);
                state.audit(me.id, to, "offline");
                return reject(Rejection::Offline);
            }
            state.audit(me.id, to, "requested");
            debug!("{} pede para conectar em {to}", me.id);
            None
        }
        ClientMsg::Accept { request, addrs } => {
            let p = take_mine(state, me, request)?;
            state.audit(p.viewer, p.host, "accepted");
            state.send_to(
                p.viewer,
                Some(p.viewer_session),
                ServerMsg::Accepted {
                    request: p.viewer_request,
                    host_key: me.key,
                    addrs: addrs.into_iter().take(16).map(|a| clean(&a)).collect(),
                },
            );
            None
        }
        ClientMsg::Reject { request } => {
            let p = take_mine(state, me, request)?;
            state.audit(p.viewer, p.host, "declined");
            state.send_to(
                p.viewer,
                Some(p.viewer_session),
                ServerMsg::Rejected {
                    request: p.viewer_request,
                    reason: Rejection::Declined,
                },
            );
            None
        }
    }
}

/// Removes a request addressed to this login (answers to anything else are
/// ignored: they expired or were never ours).
fn take_mine(state: &State, me: &Session, request: u64) -> Option<Pending> {
    let mut pending = state.pending.lock().expect("lock");
    let mine = pending
        .get(&request)
        .is_some_and(|p| (p.host, p.host_session) == (me.id, me.session));
    mine.then(|| pending.remove(&request)).flatten()
}

async fn next_msg(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Option<Result<ClientMsg, serde_json::Error>> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(text)) => return Some(serde_json::from_str(&text)),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// A name or address as given by a device: no control characters, not too
/// long (it is shown to other people).
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_owned()
}
