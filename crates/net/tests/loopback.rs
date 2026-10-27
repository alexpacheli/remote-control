use std::net::SocketAddr;

use hext_net::{
    Endpoint, Identity, NetError, client_endpoint, client_handshake, connect, host_endpoint,
    host_handshake, recv_datagram, send_datagram,
};
use hext_proto::{Control, Datagram, PeerInfo, RejectReason};

fn local_host() -> (Endpoint, SocketAddr, Identity) {
    let identity = Identity::generate().unwrap();
    let endpoint = host_endpoint(SocketAddr::from(([127, 0, 0, 1], 0)), &identity).unwrap();
    let addr = endpoint.local_addr().unwrap();
    (endpoint, addr, identity)
}

/// Accepts one session and echoes pings until the client says bye.
async fn serve_one(endpoint: Endpoint) -> Result<PeerInfo, NetError> {
    let conn = endpoint.accept().await.expect("endpoint open").await?;
    let (mut control, client) = host_handshake(&conn, PeerInfo::local("test", "host")).await?;
    loop {
        tokio::select! {
            msg = control.recv() => {
                assert_eq!(msg?, Control::Bye);
                return Ok(client);
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
async fn handshake_and_ping() {
    let (host, addr, identity) = local_host();
    let server = tokio::spawn(serve_one(host));

    let client = client_endpoint().unwrap();
    let conn = connect(&client, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
    let (control, host_info) = client_handshake(&conn, PeerInfo::local("test", "client"))
        .await
        .unwrap();
    assert_eq!(host_info.device_name, "host");

    let ping = Datagram::Ping {
        seq: 1,
        sent_micros: 99,
    };
    send_datagram(&conn, &ping).unwrap();
    let pong = recv_datagram(&conn).await.unwrap();
    assert_eq!(
        pong,
        Datagram::Pong {
            seq: 1,
            sent_micros: 99
        }
    );

    control.bye().await.unwrap();
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
    let client = client_endpoint().unwrap();
    let result = connect(&client, addr, Some(other)).await;
    assert!(matches!(result, Err(NetError::Connection(_))), "{result:?}");
}

#[tokio::test]
async fn incompatible_version_is_rejected() {
    let (host, addr, identity) = local_host();
    let server = tokio::spawn(serve_one(host));

    let client = client_endpoint().unwrap();
    let conn = connect(&client, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
    let mut hello = PeerInfo::local("test", "client");
    hello.protocol_version = 999;
    let result = client_handshake(&conn, hello).await;
    assert!(
        matches!(
            result,
            Err(NetError::Rejected(RejectReason::IncompatibleVersion { .. }))
        ),
        "{:?}",
        result.err()
    );
    assert!(matches!(server.await.unwrap(), Err(NetError::Rejected(_))));
}
