use std::time::{Duration, Instant};

use hext_crypto::DeviceKey;
use hext_crypto::pake::{HostExchange, Verifier};
use hext_net::video::{Event, Reassembler, VideoSender};
use hext_net::{
    Admission, Endpoint, EndpointAddr, EndpointConfig, NetError, PasswordLogin, bind,
    client_handshake, connect, host_handshake, recv_datagram, remote_key, send_datagram,
};
use hext_proto::{
    Control, Datagram, Hello, PROTOCOL_VERSION, PeerInfo, RejectReason, decode_preamble,
    encode_preamble,
};
use zeroize::Zeroizing;

const CODE: &str = "123456789";

fn hello(code: &str) -> Hello {
    Hello {
        peer: PeerInfo::local("test", "client"),
        access_code: code.to_owned(),
        resume: None,
        asks: Default::default(),
        audio: false,
    }
}

fn check_code(hello: &Hello) -> Result<Admission, RejectReason> {
    if hello.access_code == CODE {
        Ok(Admission::Allow(hext_proto::Permissions::CONTROL))
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

/// A frame that loses packets on the way is completed by resending them,
/// with no keyframe.
#[tokio::test]
async fn lost_packets_are_resent() {
    let (host_end, addr) = local_host().await;
    let (viewer_end, _) = viewer().await;
    let accept = tokio::spawn(async move {
        let conn = host_end.accept().await.unwrap().await.unwrap();
        (host_end, conn)
    });
    let viewer = connect(&viewer_end, addr).await.unwrap();
    let (_host_end, host) = accept.await.unwrap();
    let frame = |seq, keyframe| hext_proto::VideoFrame {
        seq,
        display: 0,
        width: 64,
        height: 64,
        keyframe,
        capture_micros: 1,
        encode_micros: 1,
        hardware: false,
        data: vec![seq as u8; 60_000],
    };
    let (requests, mut resends) = tokio::sync::mpsc::channel(4);
    let sender = tokio::spawn(async move {
        let mut sender = VideoSender::new(80_000_000);
        sender.send(&host, &frame(0, true), &mut resends).await?;
        sender.send(&host, &frame(1, false), &mut resends).await?;
        while let Some(request) = resends.recv().await {
            sender.resend(&host, &request).await?;
        }
        Ok::<_, hext_net::NetError>(host)
    });

    let mut reassembler = Reassembler::new();
    let mut delivered = Vec::new();
    // Frame 1 loses more packets than FEC can rebuild.
    let (mut seen, mut asked) = (0, false);
    while delivered.len() < 2 {
        let datagram = tokio::time::timeout(Duration::from_millis(30), recv_datagram(&viewer));
        let events = match datagram.await {
            Ok(Ok(Datagram::Video(packet))) => {
                let first_pass = packet.frame == 1 && !asked;
                seen += u32::from(first_pass);
                if first_pass && seen > 12 {
                    Vec::new()
                } else {
                    reassembler.push(packet, Instant::now())
                }
            }
            Ok(other) => panic!("{other:?}"),
            Err(_) => reassembler.tick(Instant::now()),
        };
        for event in events {
            match event {
                Event::Frame(frame) => delivered.push(frame.seq),
                Event::Resend(request) => {
                    asked = true;
                    requests.send(request).await.unwrap();
                }
                Event::Lost => panic!("given up on"),
            }
        }
    }
    assert_eq!(delivered, vec![0, 1]);
    assert_eq!(reassembler.counters.resent, 1);
    assert_eq!(reassembler.counters.lost, 0);
    drop(requests);
    sender.await.unwrap().unwrap();
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
    let (control, host_info, _) = client_handshake(&conn, hello(CODE), None).await.unwrap();
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
    // Refused in the handshake, not left hanging.
    assert!(matches!(result, Ok(Err(_))), "{result:?}");
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

/// `msg` as a control stream carries it.
fn framed(msg: &Control) -> Vec<u8> {
    hext_proto::frame::encode_frame(msg, 1 << 16).unwrap()
}

/// The wrong guess of someone who waits to send it and then stays, hoping the
/// deadline runs out before the host writes it down.
#[tokio::test]
async fn a_wrong_password_is_reported_even_at_the_deadline() {
    let (host, addr) = local_host().await;
    let host_key = DeviceKey::generate().unwrap().public_key();
    let verifier = Verifier::new("senha certa").unwrap();
    let server = tokio::spawn(async move {
        let conn = host.accept().await.expect("endpoint open").await?;
        let handshake = host_handshake(&conn, PeerInfo::local("test", "host"), |_| {
            Ok(Admission::Password {
                permissions: hext_proto::Permissions::CONTROL,
                verifier,
                host: host_key,
            })
        })
        .await;
        handshake.map(|_| ())
    });

    let (client, _) = viewer().await;
    let conn = connect(&client, addr).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&encode_preamble(PROTOCOL_VERSION))
        .await
        .unwrap();
    send.write_all(&framed(&Control::Hello(hello(CODE))))
        .await
        .unwrap();
    let mut reply = [0; 4];
    recv.read_exact(&mut reply).await.unwrap();
    assert_eq!(decode_preamble(reply).unwrap(), PROTOCOL_VERSION);

    // Close to the deadline, with less time left than the host lingers.
    tokio::time::sleep(Duration::from_millis(8500)).await;
    send.write_all(&framed(&Control::PasswordProof {
        message: vec![0; 33],
        proof: vec![0; 32],
    }))
    .await
    .unwrap();

    assert!(matches!(
        server.await.unwrap(),
        Err(NetError::Rejected(RejectReason::WrongPassword))
    ));
    drop(conn);
}

/// The viewer's answer to a host that asks for the password.
fn login(key: &DeviceKey) -> PasswordLogin {
    PasswordLogin {
        password: Zeroizing::new("senha certa".to_owned()),
        viewer: key.public_key(),
    }
}

/// With the password, a host that lets the viewer in without asking for it
/// never proved it knows it: it could be anyone the server pointed at.
#[tokio::test]
async fn a_host_that_does_not_ask_for_the_password_is_refused() {
    let (host, addr) = local_host().await;
    let server = tokio::spawn(async move {
        let conn = host.accept().await.expect("endpoint open").await?;
        let handshake = host_handshake(&conn, PeerInfo::local("test", "host"), check_code).await;
        // Kept open until the viewer leaves: the ack must reach it.
        conn.closed().await;
        handshake.map(drop)
    });
    let (client, key) = viewer().await;
    let conn = connect(&client, addr).await.unwrap();
    let result = client_handshake(&conn, hello(CODE), Some(login(&key))).await;
    assert!(
        matches!(result, Err(NetError::PasswordNotAsked)),
        "{:?}",
        result.err()
    );
    let _ = server.await;
}

/// Nor one that asks, but lets the viewer in without confirming it knew the
/// password.
#[tokio::test]
async fn a_host_that_does_not_confirm_the_password_is_refused() {
    let (host, addr) = local_host().await;
    let host_key = DeviceKey::generate().unwrap().public_key();
    let (client, key) = viewer().await;
    let viewer_key = key.public_key();
    let server = tokio::spawn(async move {
        let conn = host.accept().await.expect("endpoint open").await.unwrap();
        let (mut send, _recv) = conn.accept_bi().await.unwrap();
        // It does not know the password: another one stands in.
        let verifier = Verifier::new("outra senha").unwrap();
        let (_, message) = HostExchange::start(&verifier, &viewer_key, &host_key);
        let mut out = encode_preamble(PROTOCOL_VERSION).to_vec();
        out.extend(framed(&Control::PasswordChallenge {
            salt: *verifier.salt(),
            message,
        }));
        out.extend(framed(&Control::HelloAck {
            peer: PeerInfo::local("test", "host"),
            permissions: hext_proto::Permissions::CONTROL,
        }));
        send.write_all(&out).await.unwrap();
        conn.closed().await;
    });
    let conn = connect(&client, addr).await.unwrap();
    let result = client_handshake(&conn, hello(CODE), Some(login(&key))).await;
    assert!(
        matches!(result, Err(NetError::HostNotConfirmed)),
        "{:?}",
        result.err()
    );
    server.await.unwrap();
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
    let result = client_handshake(&conn, hello("000000000"), None).await;
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
