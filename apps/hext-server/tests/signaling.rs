//! The server in-process with real WebSocket clients.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hext_crypto::DeviceKey;
use hext_server::{Config, Db, State};
use hext_signal::{Client, ClientError, ClientMsg, DeviceId, PROTOCOL, Rejection, Role, ServerMsg};

/// A server on a random local port.
async fn server() -> (String, Arc<State>) {
    server_with(Config::default()).await
}

async fn server_with(config: Config) -> (String, Arc<State>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let state = Arc::new(State::new(Db::in_memory().unwrap()));
    tokio::spawn(hext_server::serve(listener, state.clone(), config));
    (url, state)
}

/// A plain HTTP request to the server; the status code and body.
async fn http(url: &str, request: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = url.trim_start_matches("ws://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response[9..12].parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_owned();
    (status, body)
}

fn relay_question(token: &str, key: &str) -> String {
    format!(
        "POST /v1/relay-access HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Authorization: Bearer {token}\r\nX-Iroh-Endpoint-Id: {key}\r\nContent-Length: 0\r\n\r\n"
    )
}

#[tokio::test]
async fn only_registered_devices_may_use_the_relay() {
    let config = Config {
        relay_token: Some("s3cr3t".into()),
        ..Default::default()
    };
    let (url, _) = server_with(config).await;
    let known = DeviceKey::generate().unwrap();
    login(&url, &known, "PC").await.close().await.unwrap();
    let known = known.public_key().to_string();
    let stranger = DeviceKey::generate().unwrap().public_key().to_string();

    assert_eq!(
        http(&url, &relay_question("s3cr3t", &known)).await,
        (200, "true".to_owned())
    );
    assert_eq!(
        http(&url, &relay_question("s3cr3t", &stranger)).await.0,
        403
    );
    // Only the relay (which has the secret) may ask.
    assert_eq!(http(&url, &relay_question("wrong", &known)).await.0, 401);
    assert_eq!(
        http(&url, &relay_question("s3cr3t", "nonsense")).await.0,
        400
    );
}

#[tokio::test]
async fn without_a_relay_secret_nobody_is_answered() {
    let (url, _) = server().await;
    let key = DeviceKey::generate().unwrap();
    login(&url, &key, "PC").await;
    let question = relay_question("", &key.public_key().to_string());
    assert_eq!(http(&url, &question).await.0, 404);
}

async fn login(url: &str, key: &DeviceKey, name: &str) -> Client {
    Client::connect(url, key, name, "0.1.0", Role::Host)
        .await
        .unwrap()
}

async fn recv(client: &mut Client) -> ServerMsg {
    tokio::time::timeout(Duration::from_secs(5), client.recv())
        .await
        .expect("a message in time")
        .unwrap()
}

/// Waits until the server has `n` devices online.
async fn wait_online(state: &State, n: usize) {
    for _ in 0..200 {
        if state.online() == n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected {n} online, got {}", state.online());
}

#[tokio::test]
async fn a_device_keeps_its_id() {
    let (url, _) = server().await;
    let key = DeviceKey::generate().unwrap();
    let first = login(&url, &key, "PC").await;
    let id = first.id();
    first.close().await.unwrap();
    let again = login(&url, &key, "PC").await;
    assert_eq!(again.id(), id);
    let other = login(&url, &DeviceKey::generate().unwrap(), "Mac").await;
    assert_ne!(other.id(), id);
}

#[tokio::test]
async fn request_accept_and_decline() {
    let (url, state) = server().await;
    let host_key = DeviceKey::generate().unwrap();
    let mut host = login(&url, &host_key, "Host").await;
    let viewer_key = DeviceKey::generate().unwrap();
    let mut viewer = login(&url, &viewer_key, "Técnico").await;

    viewer
        .send(&ClientMsg::Connect {
            request: 1,
            to: host.id(),
        })
        .await
        .unwrap();
    let ServerMsg::Request {
        request,
        from,
        from_key,
        from_name,
    } = recv(&mut host).await
    else {
        panic!("expected a request");
    };
    assert_eq!(
        (from, from_key, from_name.as_str()),
        (viewer.id(), viewer_key.public_key(), "Técnico")
    );
    host.send(&ClientMsg::Accept {
        request,
        addrs: vec!["192.168.0.10:7420".into()],
        session_code: "s3cr3t".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        recv(&mut viewer).await,
        ServerMsg::Accepted {
            request: 1,
            host_key: host_key.public_key(),
            addrs: vec!["192.168.0.10:7420".into()],
            session_code: "s3cr3t".into(),
        }
    );

    viewer
        .send(&ClientMsg::Connect {
            request: 2,
            to: host.id(),
        })
        .await
        .unwrap();
    let ServerMsg::Request { request, .. } = recv(&mut host).await else {
        panic!("expected a request");
    };
    host.send(&ClientMsg::Reject { request }).await.unwrap();
    assert_eq!(
        recv(&mut viewer).await,
        ServerMsg::Rejected {
            request: 2,
            reason: Rejection::Declined
        }
    );

    assert_eq!(
        audit(&state, host.id(), 4).await,
        ["requested", "accepted", "requested", "declined"]
    );
}

/// The audit events about `to`, once there are `n` (they are written in
/// the background).
async fn audit(state: &State, to: DeviceId, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let events: Vec<String> = state
            .db
            .audit_of(to)
            .unwrap()
            .into_iter()
            .map(|(_, event)| event)
            .collect();
        if events.len() >= n {
            return events;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("audit entries never arrived");
}

#[tokio::test]
async fn offline_and_unknown_ids_look_the_same() {
    let (url, _) = server().await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    let gone_key = DeviceKey::generate().unwrap();
    let gone = login(&url, &gone_key, "Off").await;
    let gone_id = gone.id();
    gone.close().await.unwrap();
    // Whatever ID was never assigned (one in a million chance otherwise).
    let unknown = DeviceId::new(123_456_789).unwrap();
    for (request, to) in [(1, gone_id), (2, unknown)] {
        // The closed one may take a moment to be noticed.
        let mut reason = None;
        for _ in 0..50 {
            viewer
                .send(&ClientMsg::Connect { request, to })
                .await
                .unwrap();
            if let ServerMsg::Rejected { reason: r, .. } = recv(&mut viewer).await {
                reason = Some(r);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reason, Some(Rejection::Offline));
    }
}

#[tokio::test]
async fn cannot_connect_to_itself() {
    let (url, _) = server().await;
    let mut me = login(&url, &DeviceKey::generate().unwrap(), "Me").await;
    let to = me.id();
    me.send(&ClientMsg::Connect { request: 5, to })
        .await
        .unwrap();
    assert_eq!(
        recv(&mut me).await,
        ServerMsg::Rejected {
            request: 5,
            reason: Rejection::SelfConnect
        }
    );
}

#[tokio::test]
async fn guessing_ids_is_rate_limited() {
    let (url, _) = server().await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    let mut reasons = Vec::new();
    for request in 0..12u64 {
        let to = DeviceId::new(200_000_000 + request as u32).unwrap();
        viewer
            .send(&ClientMsg::Connect { request, to })
            .await
            .unwrap();
        if let ServerMsg::Rejected { reason, .. } = recv(&mut viewer).await {
            reasons.push(reason);
        }
    }
    assert_eq!(reasons[..10], [Rejection::Offline; 10]);
    assert_eq!(reasons[10..], [Rejection::TooManyRequests; 2]);
}

#[tokio::test]
async fn reconnecting_does_not_reset_the_limit() {
    let (url, _) = server().await;
    let key = DeviceKey::generate().unwrap();
    let mut last = None;
    for round in 0..2u64 {
        let mut viewer = login(&url, &key, "V").await;
        for i in 0..6u64 {
            let request = round * 10 + i;
            let to = DeviceId::new(300_000_000 + request as u32).unwrap();
            viewer
                .send(&ClientMsg::Connect { request, to })
                .await
                .unwrap();
            if let ServerMsg::Rejected { reason, .. } = recv(&mut viewer).await {
                last = Some(reason);
            }
        }
        viewer.close().await.unwrap();
    }
    // 12 requests in two connections: the last ones are over the limit of 10.
    assert_eq!(last, Some(Rejection::TooManyRequests));
}

#[tokio::test]
async fn one_waiting_request_per_viewer_and_host() {
    let (url, _) = server().await;
    let mut host = login(&url, &DeviceKey::generate().unwrap(), "H").await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    let to = host.id();
    viewer
        .send(&ClientMsg::Connect { request: 1, to })
        .await
        .unwrap();
    assert!(matches!(recv(&mut host).await, ServerMsg::Request { .. }));
    // Asking again while the first waits does not pile up on the host.
    viewer
        .send(&ClientMsg::Connect { request: 2, to })
        .await
        .unwrap();
    assert_eq!(
        recv(&mut viewer).await,
        ServerMsg::Rejected {
            request: 2,
            reason: Rejection::Busy
        }
    );
}

/// Asks `host` from `viewer` and has the host answer; the viewer's answer.
async fn ask(viewer: &mut Client, host: &mut Client, request: u64, accept: bool) -> ServerMsg {
    let to = host.id();
    viewer
        .send(&ClientMsg::Connect { request, to })
        .await
        .unwrap();
    let ServerMsg::Request { request: asked, .. } = recv(host).await else {
        panic!("expected a request");
    };
    let answer = if accept {
        ClientMsg::Accept {
            request: asked,
            addrs: vec![],
            session_code: "c".into(),
        }
    } else {
        ClientMsg::Reject { request: asked }
    };
    host.send(&answer).await.unwrap();
    recv(viewer).await
}

/// Sends a request that the server refuses at once; its reason.
async fn refused(viewer: &mut Client, to: DeviceId, request: u64) -> Rejection {
    viewer
        .send(&ClientMsg::Connect { request, to })
        .await
        .unwrap();
    match recv(viewer).await {
        ServerMsg::Rejected { reason, .. } => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_flooded_host_closes_to_strangers_only() {
    let (url, state) = server().await;
    let mut host = login(&url, &DeviceKey::generate().unwrap(), "H").await;
    let mut usual = login(&url, &DeviceKey::generate().unwrap(), "Técnico").await;
    let mut strangers = Vec::new();
    for name in ["A", "B", "C", "D"] {
        strangers.push(login(&url, &DeviceKey::generate().unwrap(), name).await);
    }
    let to = host.id();

    assert!(matches!(
        ask(&mut usual, &mut host, 1, true).await,
        ServerMsg::Accepted { .. }
    ));
    // One stranger alone cannot close the host: past its share it is the
    // only one refused.
    for request in 0..3 {
        assert!(matches!(
            ask(&mut strangers[0], &mut host, request, false).await,
            ServerMsg::Rejected {
                reason: Rejection::Declined,
                ..
            }
        ));
    }
    assert_eq!(
        refused(&mut strangers[0], to, 3).await,
        Rejection::TooManyRequests
    );
    // Asking again while a request waits does not count either.
    let mut waiting = strangers.remove(1);
    waiting
        .send(&ClientMsg::Connect { request: 0, to })
        .await
        .unwrap();
    let ServerMsg::Request { request: asked, .. } = recv(&mut host).await else {
        panic!("expected a request");
    };
    for request in 1..5 {
        assert_eq!(refused(&mut waiting, to, request).await, Rejection::Busy);
    }
    host.send(&ClientMsg::Reject { request: asked })
        .await
        .unwrap();
    recv(&mut waiting).await;
    // 10 so far (the usual technician's first request included): the next
    // stranger closes the host to strangers.
    for request in 5..7 {
        ask(&mut waiting, &mut host, request, false).await;
    }
    for request in 0..3 {
        ask(&mut strangers[1], &mut host, request, false).await;
    }
    assert_eq!(
        refused(&mut strangers[2], to, 0).await,
        Rejection::TooManyRequests
    );
    // A device the host accepted before still gets through.
    assert!(matches!(
        ask(&mut usual, &mut host, 2, true).await,
        ServerMsg::Accepted { .. }
    ));
    assert!(
        audit(&state, to, 12)
            .await
            .contains(&"host_locked".to_owned())
    );
}

#[tokio::test]
async fn new_devices_per_address_are_limited() {
    let (url, _) = server().await;
    let known = DeviceKey::generate().unwrap();
    login(&url, &known, "K").await.close().await.unwrap();
    for _ in 0..9 {
        login(&url, &DeviceKey::generate().unwrap(), "N").await;
    }
    let refused = Client::connect(
        &url,
        &DeviceKey::generate().unwrap(),
        "N",
        "0.1.0",
        Role::Host,
    )
    .await;
    assert!(
        matches!(refused, Err(ClientError::Server(_))),
        "{:?}",
        refused.err()
    );
    // A device that already has an ID still gets in.
    login(&url, &known, "K").await;
}

#[tokio::test]
async fn a_viewer_leaving_cancels_its_request() {
    let (url, state) = server().await;
    let mut host = login(&url, &DeviceKey::generate().unwrap(), "H").await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    viewer
        .send(&ClientMsg::Connect {
            request: 1,
            to: host.id(),
        })
        .await
        .unwrap();
    let ServerMsg::Request { request, .. } = recv(&mut host).await else {
        panic!("expected a request");
    };
    viewer.close().await.unwrap();
    assert_eq!(recv(&mut host).await, ServerMsg::Cancelled { request });
    wait_online(&state, 1).await;
}

#[tokio::test]
async fn unanswered_requests_time_out() {
    let (url, state) = server().await;
    let mut host = login(&url, &DeviceKey::generate().unwrap(), "H").await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    viewer
        .send(&ClientMsg::Connect {
            request: 3,
            to: host.id(),
        })
        .await
        .unwrap();
    let ServerMsg::Request { request, .. } = recv(&mut host).await else {
        panic!("expected a request");
    };
    // As if the timeout had passed.
    state
        .expire(Instant::now() + hext_server::signal::REQUEST_TIMEOUT)
        .await;
    assert_eq!(
        recv(&mut viewer).await,
        ServerMsg::Rejected {
            request: 3,
            reason: Rejection::Timeout
        }
    );
    assert_eq!(recv(&mut host).await, ServerMsg::Cancelled { request });
    // A late answer goes nowhere.
    host.send(&ClientMsg::Accept {
        request,
        addrs: vec![],
        session_code: String::new(),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn only_the_host_can_answer() {
    let (url, _) = server().await;
    let mut host = login(&url, &DeviceKey::generate().unwrap(), "H").await;
    let mut viewer = login(&url, &DeviceKey::generate().unwrap(), "V").await;
    let mut other = login(&url, &DeviceKey::generate().unwrap(), "X").await;
    viewer
        .send(&ClientMsg::Connect {
            request: 1,
            to: host.id(),
        })
        .await
        .unwrap();
    let ServerMsg::Request { request, .. } = recv(&mut host).await else {
        panic!("expected a request");
    };
    // Another device answering the host's request is ignored.
    other
        .send(&ClientMsg::Accept {
            request,
            addrs: vec!["evil:1".into()],
            session_code: "x".into(),
        })
        .await
        .unwrap();
    host.send(&ClientMsg::Reject { request }).await.unwrap();
    assert_eq!(
        recv(&mut viewer).await,
        ServerMsg::Rejected {
            request: 1,
            reason: Rejection::Declined
        }
    );
}

#[tokio::test]
async fn a_viewer_with_the_same_key_leaves_the_host_alone() {
    let (url, state) = server().await;
    let key = DeviceKey::generate().unwrap();
    let mut host = login(&url, &key, "PC").await;
    let mut viewer = Client::connect(&url, &key, "PC", "0.1.0", Role::Viewer)
        .await
        .unwrap();
    assert_eq!(viewer.id(), host.id());
    wait_online(&state, 2).await;

    // Requests for the ID still reach the host…
    let mut other = login(&url, &DeviceKey::generate().unwrap(), "X").await;
    other
        .send(&ClientMsg::Connect {
            request: 1,
            to: host.id(),
        })
        .await
        .unwrap();
    assert!(matches!(recv(&mut host).await, ServerMsg::Request { .. }));
    // …and the viewer can still ask.
    let to = other.id();
    viewer
        .send(&ClientMsg::Connect { request: 2, to })
        .await
        .unwrap();
    assert!(matches!(recv(&mut other).await, ServerMsg::Request { .. }));

    // A device logged in only as a viewer takes no requests.
    let lone = Client::connect(
        &url,
        &DeviceKey::generate().unwrap(),
        "V",
        "0.1.0",
        Role::Viewer,
    )
    .await
    .unwrap();
    let to = lone.id();
    other
        .send(&ClientMsg::Connect { request: 3, to })
        .await
        .unwrap();
    assert_eq!(
        recv(&mut other).await,
        ServerMsg::Rejected {
            request: 3,
            reason: Rejection::Offline
        }
    );
}

#[tokio::test]
async fn logging_in_again_replaces_the_old_connection() {
    let (url, state) = server().await;
    let key = DeviceKey::generate().unwrap();
    let mut old = login(&url, &key, "PC").await;
    let new = login(&url, &key, "PC").await;
    assert_eq!(old.id(), new.id());
    assert!(matches!(old.recv().await, Err(ClientError::Server(_))));
    wait_online(&state, 1).await;
}

/// A raw WebSocket, to send what a well-behaved client would not.
mod raw {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    pub type Ws = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    pub async fn connect(url: &str) -> Ws {
        let (ws, _) = tokio_tungstenite::connect_async(format!("{url}/v1/signal"))
            .await
            .unwrap();
        ws
    }

    pub async fn recv_text(ws: &mut Ws) -> Option<String> {
        while let Some(msg) = ws.next().await {
            match msg.ok()? {
                Message::Text(text) => return Some(text.to_string()),
                Message::Close(_) => return None,
                _ => {}
            }
        }
        None
    }

    pub async fn send_text(ws: &mut Ws, text: String) {
        let _ = ws.send(Message::text(text)).await;
    }
}

#[tokio::test]
async fn a_wrong_signature_is_refused() {
    let (url, state) = server().await;
    let mut ws = raw::connect(&url).await;
    let challenge = raw::recv_text(&mut ws).await.unwrap();
    let ServerMsg::Challenge { nonce, .. } = serde_json::from_str(&challenge).unwrap() else {
        panic!("expected a challenge");
    };
    let key = DeviceKey::generate().unwrap();
    // Signed some other challenge: a replayed login.
    let mut other = nonce;
    other[0] ^= 1;
    let hello = ClientMsg::Hello {
        protocol: PROTOCOL,
        public_key: key.public_key(),
        signature: key.sign(&hext_signal::auth_message(&other)),
        device_name: "X".into(),
        app_version: "0".into(),
        accepts_requests: true,
    };
    raw::send_text(&mut ws, hello.to_json()).await;
    let reply: ServerMsg = serde_json::from_str(&raw::recv_text(&mut ws).await.unwrap()).unwrap();
    assert!(matches!(reply, ServerMsg::Error { .. }), "{reply:?}");
    assert!(raw::recv_text(&mut ws).await.is_none(), "connection closed");
    assert_eq!(state.online(), 0);
}

#[tokio::test]
async fn messages_before_login_close_the_connection() {
    let (url, state) = server().await;
    let mut ws = raw::connect(&url).await;
    raw::recv_text(&mut ws).await.unwrap(); // the challenge
    raw::send_text(
        &mut ws,
        ClientMsg::Connect {
            request: 1,
            to: DeviceId::new(482_913_557).unwrap(),
        }
        .to_json(),
    )
    .await;
    assert!(raw::recv_text(&mut ws).await.is_none());
    raw::send_text(&mut ws, "{".into()).await;
    assert_eq!(state.online(), 0);
}

#[tokio::test]
async fn health() {
    let (url, _) = server().await;
    let addr = url.trim_start_matches("ws://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("ok"));
}
