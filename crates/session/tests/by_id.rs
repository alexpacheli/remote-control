//! Connecting by ID end to end: the signaling server in-process, a host
//! that accepts through it, and a viewer that pins the host's device key and
//! opens a session with the one-time code.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hext_codec::{DecoderPreference, EncoderSettings};
use hext_crypto::DeviceKey;
use hext_net::{EndpointConfig, NetError};
use hext_proto::{Hello, PeerInfo, RejectReason};
use hext_session::host::HostConfig;
use hext_session::{SessionCodes, SessionError, client, host};
use hext_signal::{Client, ClientMsg, Role, ServerMsg};

#[tokio::test(flavor = "multi_thread")]
async fn accepted_by_id_then_streams() {
    // The signaling server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = format!("ws://{}", listener.local_addr().unwrap());
    let state = Arc::new(hext_server::State::new(
        hext_server::Db::in_memory().unwrap(),
    ));
    tokio::spawn(hext_server::serve(listener, state, Default::default()));

    // The host: a QUIC endpoint with its device identity, logged in.
    let host_key = DeviceKey::generate().unwrap();
    let config = EndpointConfig {
        accept: true,
        ..Default::default()
    };
    let endpoint = hext_net::bind(&host_key, &config).await.unwrap();
    let host_addrs = hext_net::addr_to_strings(&endpoint.addr());
    let codes = SessionCodes::default();
    let config = Arc::new(HostConfig {
        peer: PeerInfo::local("test", "host"),
        access_code: "999888777".into(),
        session_codes: codes.clone(),
        display: 0,
        test_pattern: true,
        encoder: EncoderSettings::default(),
    });
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let config = config.clone();
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    let _ = host::serve(conn, config).await;
                }
            });
        }
    });
    let mut host_signal = Client::connect(&server, &host_key, "Host", "test", Role::Host)
        .await
        .unwrap();
    let host_id = host_signal.id();
    // The host's user says yes.
    let answering = tokio::spawn(async move {
        let ServerMsg::Request {
            request,
            from_name,
            from_key,
            ..
        } = host_signal.recv().await.unwrap()
        else {
            panic!("expected a request");
        };
        assert_eq!(from_name, "Técnico");
        host_signal
            .send(&ClientMsg::Accept {
                request,
                addrs: host_addrs,
                session_code: codes.issue(from_key),
            })
            .await
            .unwrap();
        host_signal
    });

    // The viewer asks by ID.
    let viewer_key = DeviceKey::generate().unwrap();
    let mut viewer_signal = Client::connect(&server, &viewer_key, "Técnico", "test", Role::Viewer)
        .await
        .unwrap();
    let access = viewer_signal
        .request_access(host_id)
        .await
        .unwrap()
        .unwrap();
    let _host_signal = answering.await.unwrap();
    assert_eq!(access.host_key, host_key.public_key());
    let addr = hext_net::addr_from_strings(&access.host_key, &access.addrs).unwrap();

    let hello = |code: &str| Hello {
        peer: PeerInfo::local("test", "viewer"),
        access_code: code.to_owned(),
    };
    let client_endpoint = hext_net::bind(&viewer_key, &EndpointConfig::default())
        .await
        .unwrap();
    let conn = hext_net::connect(&client_endpoint, addr.clone())
        .await
        .unwrap();
    let decoded = Arc::new(AtomicU32::new(0));
    let counter = decoded.clone();
    let session = client::start(
        conn,
        hello(&access.session_code),
        DecoderPreference::Software,
        move |_| {
            counter.fetch_add(1, Ordering::Relaxed);
        },
    )
    .await
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while decoded.load(Ordering::Relaxed) < 10 {
        assert!(tokio::time::Instant::now() < deadline, "no video");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.close().await.unwrap();

    // The code opened one session only.
    let conn = hext_net::connect(&client_endpoint, addr.clone())
        .await
        .unwrap();
    let again = client::start(
        conn,
        hello(&access.session_code),
        DecoderPreference::Software,
        |_| {},
    )
    .await;
    assert!(
        matches!(
            again,
            Err(SessionError::Net(NetError::Rejected(
                RejectReason::WrongCode
            )))
        ),
        "{:?}",
        again.err()
    );

    // And a viewer told someone else's key would not connect.
    let impostor = DeviceKey::generate().unwrap().public_key();
    let wrong = hext_net::addr_from_strings(&impostor, &access.addrs).unwrap();
    let attempt = tokio::time::timeout(
        Duration::from_secs(15),
        hext_net::connect(&client_endpoint, wrong),
    )
    .await;
    assert!(!matches!(attempt, Ok(Ok(_))));
}
