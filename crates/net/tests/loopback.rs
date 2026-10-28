use std::time::Duration;

use hext_crypto::DeviceKey;
use hext_net::{
    Endpoint, EndpointAddr, EndpointConfig, NetError, bind, client_handshake, connect,
    host_handshake, recv_datagram, remote_key, send_datagram,
};
use hext_proto::{
    Control, Datagram, Hello, PROTOCOL_VERSION, PeerInfo, RejectReason, decode_preamble,
    encode_preamble,
};

const CODE: &str = "123456789";

fn hello(code: &str) -> Hello {
    Hello {
        peer: PeerInfo::local("test", "client"),
        access_code: code.to_owned(),
    }
}

fn check_code(hello: &Hello) -> Result<(), RejectReason> {
    if hello.access_code == CODE {
        Ok(())
    } else {
        Err(RejectReason::WrongCode)
    }
}

/// A host on this machine (direct only, no relay) and its address.
async fn local_host() -> (Endpoint, EndpointAddr) {
    let key = DeviceKey::generate().unwrap();
    let config = EndpointConfig {
        accept: true,
        ..Default::default()
    };
    let endpoint = bind(&key, &config).await.unwrap();
    let addr = endpoint.addr();
    (endpoint, addr)
}

/// A viewer's endpoint, and its key.
async fn viewer() -> (Endpoint, DeviceKey) {
    let key = DeviceKey::generate().unwrap();
    (bind(&key, &EndpointConfig::default()).await.unwrap(), key)
}

/// Accepts one session and echoes pings until the client says bye.
async fn serve_one(endpoint: Endpoint) -> Result<PeerInfo, NetError> {
    let conn = endpoint.accept().await.expect("endpoint open").await?;
    let (mut control, hello) =
        host_handshake(&conn, PeerInfo::local("test", "host"), check_code).await?;
    loop {
        tokio::select! {
            msg = control.recv() => {
                assert_eq!(msg?, Control::Bye);
                conn.close(0u32.into(), b"bye");
                return Ok(hello.peer);
            }
            dg = recv_datagram(&conn) => {
                if let Datagram::Ping { seq, sent_micros } = dg? {
                    send_datagram(&conn, &Datagram::Pong { seq, sent_micros })?;
                }
            }
        }
    }
}

#[tokio::test]
async fn handshake_ping_and_bye() {
    let (host, addr) = local_host().await;
    let host_key = hext_crypto::PublicKey::from_bytes(*addr.id.as_bytes());
    let server = tokio::spawn(serve_one(host));

    let (client, _) = viewer().await;
    let conn = connect(&client, addr).await.unwrap();
    // The host proved it holds the key we dialed.
    assert_eq!(remote_key(&conn), host_key);
    let (control, host_info) = client_handshake(&conn, hello(CODE)).await.unwrap();
    assert_eq!(host_info.device_name, "host");

    // Many pings while the host also waits on the control stream exercise
    // the cancel-safety of `ControlStream::recv` inside `select!`.
    for seq in 0..50 {
        send_datagram(
            &conn,
            &Datagram::Ping {
                seq,
                sent_micros: 99,
            },
        )
        .unwrap();
        let pong = recv_datagram(&conn).await.unwrap();
        assert_eq!(
            pong,
            Datagram::Pong {
                seq,
                sent_micros: 99
            }
        );
    }

    // The Bye must reach the host before the connection closes.
    control.bye(&conn).await.unwrap();
    let client_info = server.await.unwrap().unwrap();
    assert_eq!(client_info.device_name, "client");
}

#[tokio::test]
async fn a_host_without_the_expected_key_is_refused() {
    let (host, addr) = local_host().await;
    tokio::spawn(async move {
        if let Some(incoming) = host.accept().await {
            let _ = incoming.await;
        }
    });

    // The host's addresses, but someone else's key: an impostor there.
    let other = DeviceKey::generate().unwrap().public_key();
    let wrong = hext_net::addr_from_strings(&other, &hext_net::addr_to_strings(&addr)).unwrap();
    let (client, _) = viewer().await;
    let result = tokio::time::timeout(Duration::from_secs(15), connect(&client, wrong)).await;
    assert!(matches!(result, Ok(Err(_)) | Err(_)), "{result:?}");
    assert!(!matches!(result, Ok(Ok(_))));
}

#[tokio::test]
async fn the_host_sees_the_viewers_key() {
    let (host, addr) = local_host().await;
    let seen = tokio::spawn(async move {
        let conn = host.accept().await.unwrap().await.unwrap();
        remote_key(&conn)
    });
    let (client, key) = viewer().await;
    let _conn = connect(&client, addr).await.unwrap();
    assert_eq!(seen.await.unwrap(), key.public_key());
}

#[test]
fn addresses_travel_as_text() {
    let key = DeviceKey::generate().unwrap().public_key();
    let entries = vec![
        "relay https://relay.example.com/".to_owned(),
        "ip 192.168.0.10:7420".to_owned(),
        "ip [2001:db8::1]:7420".to_owned(),
        "something new".to_owned(),
    ];
    let addr = hext_net::addr_from_strings(&key, &entries).unwrap();
    assert_eq!(addr.ip_addrs().count(), 2);
    assert_eq!(addr.relay_urls().count(), 1);
    let mut back = hext_net::addr_to_strings(&addr);
    back.sort();
    let mut expected = entries[..3].to_vec();
    expected.sort();
    assert_eq!(back, expected);
    assert!(back.iter().all(|e| e.len() <= 64));
}

#[tokio::test]
async fn incompatible_version_is_reported() {
    let (host, addr) = local_host().await;
    let server = tokio::spawn(serve_one(host));

    let (client, _) = viewer().await;
    let conn = connect(&client, addr).await.unwrap();
    // Speak as a future client: only the preamble layout is shared.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&encode_preamble(999)).await.unwrap();
    let mut reply = [0; 4];
    recv.read_exact(&mut reply).await.unwrap();
    assert_eq!(decode_preamble(reply).unwrap(), PROTOCOL_VERSION);
    conn.close(0u32.into(), b"incompatible version");

    assert!(matches!(
        server.await.unwrap(),
        Err(NetError::IncompatibleVersion { remote: 999, .. })
    ));
}

#[tokio::test]
async fn wrong_access_code_is_rejected() {
    let (host, addr) = local_host().await;
    let server = tokio::spawn(serve_one(host));

    let (client, _) = viewer().await;
    let conn = connect(&client, addr).await.unwrap();
    let result = client_handshake(&conn, hello("000000000")).await;
    assert!(
        matches!(result, Err(NetError::Rejected(RejectReason::WrongCode))),
        "{:?}",
        result.err()
    );
    assert!(matches!(
        server.await.unwrap(),
        Err(NetError::Rejected(RejectReason::WrongCode))
    ));
}
