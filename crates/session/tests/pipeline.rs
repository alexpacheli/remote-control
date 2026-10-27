//! Host (test pattern) → QUIC → client decode, over loopback.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use hext_codec::EncoderSettings;
use hext_net::Identity;
use hext_proto::{Hello, InputEvent, PeerInfo};
use hext_session::host::HostConfig;
use hext_session::{SessionError, client, host};

const CODE: &str = "111222333";

async fn start_host() -> (
    SocketAddr,
    Identity,
    tokio::task::JoinHandle<Result<PeerInfo, SessionError>>,
) {
    let identity = Identity::generate().unwrap();
    let endpoint =
        hext_net::host_endpoint(SocketAddr::from(([127, 0, 0, 1], 0)), &identity).unwrap();
    let addr = endpoint.local_addr().unwrap();
    let config = Arc::new(HostConfig {
        peer: PeerInfo::local("test", "host"),
        access_code: CODE.into(),
        display: 0,
        test_pattern: true,
        encoder: EncoderSettings::default(),
    });
    let server = tokio::spawn(async move {
        let conn = endpoint.accept().await.unwrap().await.unwrap();
        let result = host::serve(conn, config).await;
        endpoint.wait_idle().await;
        result
    });
    (addr, identity, server)
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_and_decodes_video() {
    let (addr, identity, server) = start_host().await;

    let endpoint = hext_net::client_endpoint(addr).unwrap();
    let conn = hext_net::connect(&endpoint, addr, Some(identity.fingerprint()))
        .await
        .unwrap();
    let decoded = Arc::new(AtomicU32::new(0));
    let counter = decoded.clone();
    let session = client::start(
        conn,
        Hello {
            peer: PeerInfo::local("test", "client"),
            access_code: "111 222 333".into(),
        },
        move |frame| {
            assert_eq!((frame.yuv.width, frame.yuv.height), (1280, 720));
            counter.fetch_add(1, Ordering::Relaxed);
        },
    )
    .await
    .unwrap();
    assert_eq!(session.host.device_name, "host");

    // Input reaches the host (logged there on this OS) without breaking the session.
    session.send_input(InputEvent::MouseMove { x: 0.5, y: 0.5 });
    session.send_input(InputEvent::ReleaseAll);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while decoded.load(Ordering::Relaxed) < 20 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "only {} frames decoded",
            decoded.load(Ordering::Relaxed)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    session.close().await.unwrap();
    let client = server.await.unwrap().unwrap();
    assert_eq!(client.device_name, "client");
}
