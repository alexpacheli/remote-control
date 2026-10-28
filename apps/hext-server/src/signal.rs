//! One WebSocket per device: login by signed challenge, then relaying
//! connection requests between devices that are online.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use hext_crypto::PublicKey;
use hext_signal::{
    ClientMsg, DeviceId, MAX_NAME_LEN, NONCE_LEN, PROTOCOL, Rejection, ServerMsg, auth_message,
};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::db::Db;
use crate::limits::RateLimiter;

/// How long a device has to answer the challenge.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a host's user has to answer a request.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Requests waiting on one host at once (from different viewers: a viewer
/// has at most one waiting per host).
const MAX_PENDING_PER_HOST: usize = 5;
/// The server pings this often; a device silent for [`IDLE_TIMEOUT`] is gone.
const PING_INTERVAL: Duration = Duration::from_secs(20);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Messages queued for one device, and how long to wait for room.
const OUTBOX: usize = 32;
const OUTBOX_WAIT: Duration = Duration::from_secs(2);

/// Connection requests per device and per IP address: guessing IDs is slow.
const CONNECTS_PER_DEVICE: usize = 10;
const CONNECTS_PER_IP: usize = 30;
const CONNECT_WINDOW: Duration = Duration::from_secs(60);
/// Logins per IP address, and new devices (IDs handed out) per IP address.
const LOGINS_PER_IP: usize = 30;
const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const NEW_DEVICES_PER_IP: usize = 10;
const NEW_DEVICE_WINDOW: Duration = Duration::from_secs(3600);

/// Who is online, which requests are waiting, and the rate limits.
pub struct State {
    pub db: Arc<Db>,
    /// Hosts online, by ID: where requests for that ID go.
    hosts: Mutex<HashMap<DeviceId, HostLogin>>,
    /// Every login (hosts and viewers), by session.
    outboxes: Mutex<HashMap<u64, mpsc::Sender<ServerMsg>>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    rng: SystemRandom,
    audit: mpsc::UnboundedSender<(DeviceId, DeviceId, &'static str)>,
    connects_by_device: RateLimiter<DeviceId>,
    connects_by_ip: RateLimiter<IpAddr>,
    logins_by_ip: RateLimiter<IpAddr>,
    new_devices_by_ip: RateLimiter<IpAddr>,
}

/// A device logged in as a host (it takes requests).
struct HostLogin {
    /// Tells this login apart from a newer one of the same device.
    session: u64,
    /// Dropped when a newer host login of the same device replaces this
    /// one, which ends it.
    _kick: oneshot::Sender<()>,
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
        let db = Arc::new(db);
        // Audit entries are written in order, off the async threads.
        let (audit, mut entries) = mpsc::unbounded_channel();
        let writer = db.clone();
        std::thread::spawn(move || {
            while let Some((from, to, event)) = entries.blocking_recv() {
                if let Err(e) = writer.audit(from, to, event) {
                    warn!("auditoria: {e}");
                }
            }
        });
        Self {
            db,
            hosts: Mutex::default(),
            outboxes: Mutex::default(),
            pending: Mutex::default(),
            next_id: AtomicU64::new(1),
            rng: SystemRandom::new(),
            audit,
            connects_by_device: RateLimiter::new(CONNECTS_PER_DEVICE, CONNECT_WINDOW),
            connects_by_ip: RateLimiter::new(CONNECTS_PER_IP, CONNECT_WINDOW),
            logins_by_ip: RateLimiter::new(LOGINS_PER_IP, LOGIN_WINDOW),
            new_devices_by_ip: RateLimiter::new(NEW_DEVICES_PER_IP, NEW_DEVICE_WINDOW),
        }
    }

    /// Number of logins (hosts and viewers).
    pub fn online(&self) -> usize {
        self.outboxes.lock().expect("lock").len()
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Runs a database call off the async threads.
    async fn db<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Db) -> rusqlite::Result<T> + Send + 'static,
    ) -> rusqlite::Result<T> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || call(&db))
            .await
            .expect("database task")
    }

    fn audit(&self, from: DeviceId, to: DeviceId, event: &'static str) {
        let _ = self.audit.send((from, to, event));
    }

    /// Delivers `msg` to login `session` (of device `_id`); false if it is
    /// gone (or so stuck it cannot take messages).
    async fn send_to(&self, _id: DeviceId, session: u64, msg: ServerMsg) -> bool {
        let Some(outbox) = self.outboxes.lock().expect("lock").get(&session).cloned() else {
            return false;
        };
        matches!(
            tokio::time::timeout(OUTBOX_WAIT, outbox.send(msg)).await,
            Ok(Ok(()))
        )
    }

    /// Ends requests whose host never answered, and forgets old rate-limit
    /// entries. Call every second or so.
    pub async fn expire(&self, now: Instant) {
        let expired = self.take_pending(|p| p.expires <= now);
        for (request, p) in expired {
            self.audit(p.viewer, p.host, "timeout");
            let reject = ServerMsg::Rejected {
                request: p.viewer_request,
                reason: Rejection::Timeout,
            };
            self.send_to(p.viewer, p.viewer_session, reject).await;
            self.send_to(p.host, p.host_session, ServerMsg::Cancelled { request })
                .await;
        }
        self.connects_by_device.prune(now);
        self.connects_by_ip.prune(now);
        self.logins_by_ip.prune(now);
        self.new_devices_by_ip.prune(now);
    }

    /// Removes and returns the waiting requests that match.
    fn take_pending(&self, matches: impl Fn(&Pending) -> bool) -> Vec<(u64, Pending)> {
        let mut pending = self.pending.lock().expect("lock");
        let ids: Vec<u64> = pending
            .iter()
            .filter(|(_, p)| matches(p))
            .map(|(&id, _)| id)
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id).map(|p| (id, p)))
            .collect()
    }

    /// A login ended: forget it and the requests it was part of.
    async fn leave(&self, id: DeviceId, session: u64) {
        self.outboxes.lock().expect("lock").remove(&session);
        {
            let mut hosts = self.hosts.lock().expect("lock");
            if hosts.get(&id).is_some_and(|h| h.session == session) {
                hosts.remove(&id);
            }
        }
        let gone = self.take_pending(|p| {
            (p.viewer, p.viewer_session) == (id, session)
                || (p.host, p.host_session) == (id, session)
        });
        for (request, p) in gone {
            if p.viewer == id {
                self.audit(p.viewer, p.host, "cancelled");
                self.send_to(p.host, p.host_session, ServerMsg::Cancelled { request })
                    .await;
            } else {
                self.audit(p.viewer, p.host, "host_left");
                let reject = ServerMsg::Rejected {
                    request: p.viewer_request,
                    reason: Rejection::Offline,
                };
                self.send_to(p.viewer, p.viewer_session, reject).await;
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
    ip: IpAddr,
    /// Its last request was refused for the rate limit (audited once).
    limited: bool,
}

type Sink = futures_util::stream::SplitSink<WebSocket, Message>;

/// Sends `msg`; false once the connection is gone.
async fn send(sink: &mut Sink, msg: &ServerMsg) -> bool {
    sink.send(Message::text(msg.to_json())).await.is_ok()
}

async fn refuse(sink: &mut Sink, message: &str) {
    let message = message.to_owned();
    send(sink, &ServerMsg::Error { message }).await;
    let _ = sink.close().await;
}

/// Serves one device's WebSocket, from `ip`, until it leaves.
pub async fn handle(socket: WebSocket, state: &State, ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    if !state.logins_by_ip.allow(&ip, Instant::now()) {
        refuse(
            &mut sink,
            "muitas conexões a partir deste endereço; tente de novo em alguns minutos",
        )
        .await;
        return;
    }

    let mut nonce = [0; NONCE_LEN];
    if state.rng.fill(&mut nonce).is_err() {
        return;
    }
    let challenge = ServerMsg::Challenge {
        protocol: PROTOCOL,
        nonce,
    };
    if !send(&mut sink, &challenge).await {
        return;
    }
    let hello = tokio::time::timeout(LOGIN_TIMEOUT, next_msg(&mut stream)).await;
    let Ok(Some(Ok(ClientMsg::Hello {
        protocol,
        public_key: key,
        signature,
        device_name,
        app_version,
        accepts_requests,
    }))) = hello
    else {
        // Silence, garbage or another message first: no login.
        return;
    };
    if protocol != PROTOCOL {
        let message = format!(
            "versão do protocolo de sinalização incompatível: a do servidor é {PROTOCOL}, a sua é {protocol}"
        );
        refuse(&mut sink, &message).await;
        return;
    }
    if !key.verify(&auth_message(&nonce), &signature) {
        refuse(&mut sink, "assinatura inválida").await;
        return;
    }
    let (name, version) = (clean(&device_name), clean(&app_version));

    let id = match login(state, key, &name, &version, ip).await {
        Ok(id) => id,
        Err(message) => {
            refuse(&mut sink, message).await;
            return;
        }
    };

    let session = state.next_id();
    let (outbox, mut inbox) = mpsc::channel(OUTBOX);
    let (kick, mut kicked) = oneshot::channel::<()>();
    state.outboxes.lock().expect("lock").insert(session, outbox);
    // A viewer on the same device (same key) does not disturb its host.
    let mut _not_a_host = None;
    if accepts_requests {
        // A newer host login of the same device (it reconnected) replaces
        // this one: dropping the old entry drops its `_kick`, which ends it.
        let host = HostLogin {
            session,
            _kick: kick,
        };
        state.hosts.lock().expect("lock").insert(id, host);
    } else {
        _not_a_host = Some(kick);
    }
    let role = if accepts_requests { "host" } else { "técnico" };
    info!("online: {id} ({name}, {version}, {role})");

    let mut me = Session {
        id,
        session,
        key,
        name,
        ip,
        limited: false,
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
                        Ok(msg) => on_message(state, &mut me, msg).await,
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
                    if !send(&mut sink, &msg).await {
                        break;
                    }
                }
                _ = &mut kicked => {
                    let message = "este computador conectou de novo em outra conexão".to_owned();
                    send(&mut sink, &ServerMsg::Error { message }).await;
                    break;
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
    state.leave(id, session).await;
    info!("offline: {id}");
    let _ = sink.close().await;
}

/// The device's ID: known, or newly assigned within the per-IP limit.
async fn login(
    state: &State,
    key: PublicKey,
    name: &str,
    version: &str,
    ip: IpAddr,
) -> Result<DeviceId, &'static str> {
    let failed = |e: rusqlite::Error| {
        warn!("banco de dados: {e}");
        "erro no servidor"
    };
    let (n, v) = (name.to_owned(), version.to_owned());
    if let Some(id) = state
        .db(move |db| db.find(&key, &n, &v))
        .await
        .map_err(failed)?
    {
        return Ok(id);
    }
    if !state.new_devices_by_ip.allow(&ip, Instant::now()) {
        return Err("muitos computadores novos a partir deste endereço; tente de novo mais tarde");
    }
    let (n, v) = (name.to_owned(), version.to_owned());
    state
        .db(move |db| db.register(&key, &n, &v))
        .await
        .map_err(failed)
}

/// Handles a message from a logged-in device; returns its direct reply.
async fn on_message(state: &State, me: &mut Session, msg: ClientMsg) -> Option<ServerMsg> {
    match msg {
        ClientMsg::Hello { .. } => Some(ServerMsg::Error {
            message: "já conectado".to_owned(),
        }),
        ClientMsg::Connect { request, to } => connect(state, me, request, to)
            .await
            .err()
            .map(|reason| ServerMsg::Rejected { request, reason }),
        ClientMsg::Accept {
            request,
            addrs,
            session_code,
        } => {
            let p = take_mine(state, me, request)?;
            state.audit(p.viewer, p.host, "accepted");
            let accepted = ServerMsg::Accepted {
                request: p.viewer_request,
                host_key: me.key,
                addrs: addrs.iter().take(16).map(|a| clean(a)).collect(),
                session_code: clean(&session_code),
            };
            state.send_to(p.viewer, p.viewer_session, accepted).await;
            None
        }
        ClientMsg::Reject { request } => {
            let p = take_mine(state, me, request)?;
            state.audit(p.viewer, p.host, "declined");
            let declined = ServerMsg::Rejected {
                request: p.viewer_request,
                reason: Rejection::Declined,
            };
            state.send_to(p.viewer, p.viewer_session, declined).await;
            None
        }
    }
}

/// Asks host `to` about a viewer's request; the error is the answer to
/// send back at once.
async fn connect(
    state: &State,
    me: &mut Session,
    request: u64,
    to: DeviceId,
) -> Result<(), Rejection> {
    let now = Instant::now();
    if !state.connects_by_device.allow(&me.id, now) || !state.connects_by_ip.allow(&me.ip, now) {
        // Audited once per burst, not once per refused message.
        if !std::mem::replace(&mut me.limited, true) {
            state.audit(me.id, to, "rate_limited");
        }
        return Err(Rejection::TooManyRequests);
    }
    me.limited = false;
    if to == me.id {
        return Err(Rejection::SelfConnect);
    }
    let host_session = state
        .hosts
        .lock()
        .expect("lock")
        .get(&to)
        .map(|h| h.session);
    let Some(host_session) = host_session else {
        state.audit(me.id, to, "offline");
        return Err(Rejection::Offline);
    };
    let server_request = state.next_id();
    {
        let mut pending = state.pending.lock().expect("lock");
        let waiting = pending.values().filter(|p| p.host == to);
        let (mine, all) = waiting.fold((0, 0), |(mine, all), p| {
            (mine + usize::from(p.viewer == me.id), all + 1)
        });
        if mine > 0 || all >= MAX_PENDING_PER_HOST {
            drop(pending);
            state.audit(me.id, to, "busy");
            return Err(Rejection::Busy);
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
    let ask = ServerMsg::Request {
        request: server_request,
        from: me.id,
        from_key: me.key,
        from_name: me.name.clone(),
    };
    if !state.send_to(to, host_session, ask).await {
        state.pending.lock().expect("lock").remove(&server_request);
        state.audit(me.id, to, "offline");
        return Err(Rejection::Offline);
    }
    state.audit(me.id, to, "requested");
    debug!("{} pede para conectar em {to}", me.id);
    Ok(())
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

/// A name or address as given by a device, made safe to show to other
/// people: no control or invisible characters (which could reorder or
/// disguise the text), not too long.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() && !invisible(c))
        .take(MAX_NAME_LEN)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Unicode format characters: bidirectional overrides, zero-width marks.
fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean("  Suporte\u{202E}gpj.exe "), "Suportegpj.exe");
        assert_eq!(clean("a\u{200B}b\u{0007}c\nd"), "abcd");
        assert_eq!(clean("Técnico Ç"), "Técnico Ç");
        assert_eq!(clean(&"x".repeat(100)).len(), MAX_NAME_LEN);
    }
}
