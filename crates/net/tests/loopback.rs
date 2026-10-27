use std::net::SocketAddr;

use hext_net::{
    Endpoint, Identity, NetError, client_endpoint, client_handshake, connect, host_endpoint,
    host_handshake, recv_datagram, send_datagram,
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

fn local_host() -> (Endpoint, SocketAddr, Identity) {
    let identity = Identity::generate().unwrap();
    let endpoint = host_endpoint(SocketAddr::from(([127, 0, 0, 1], 0)), &identity).unwrap();
    let addr = endpoint.local_addr().unwrap();
    (endpoint, addr, identity)
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
    let (host, addr, identity) = local_host();
    let server = tokio::spawn(serve_one(host));

    let client = client_endpoint(addr).unwrap();
    let conn = connect(&client, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
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
async fn wrong_fingerprint_is_refused() {
    let (host, addr, _identity) = local_host();
    tokio::spawn(async move {
        if let Some(incoming) = host.accept().await {
            let _ = incoming.await;
        }
    });

    let other = Identity::generate().unwrap().fingerprint();
    let client = client_endpoint(addr).unwrap();
    let result = connect(&client, addr, Some(other)).await;
    assert!(matches!(result, Err(NetError::Connection(_))), "{result:?}");
}

#[tokio::test]
async fn incompatible_version_is_reported() {
    let (host, addr, identity) = local_host();
    let server = tokio::spawn(serve_one(host));

    let client = client_endpoint(addr).unwrap();
    let conn = connect(&client, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
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
    let (host, addr, identity) = local_host();
    let server = tokio::spawn(serve_one(host));

    let client = client_endpoint(addr).unwrap();
    let conn = connect(&client, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
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
