//! A client pinning the host by fingerprint or by device key, over loopback.

use std::net::SocketAddr;

use hext_crypto::DeviceKey;
use hext_net::{HostPin, Identity};

/// A host that accepts connections until the test ends; its address.
fn host(identity: &Identity) -> SocketAddr {
    let endpoint =
        hext_net::host_endpoint(SocketAddr::from(([127, 0, 0, 1], 0)), identity).unwrap();
    let addr = endpoint.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let _ = incoming.await;
        }
    });
    addr
}

async fn connects(addr: SocketAddr, pin: HostPin) -> bool {
    let endpoint = hext_net::client_endpoint(addr).unwrap();
    hext_net::connect(&endpoint, addr, pin).await.is_ok()
}

#[tokio::test]
async fn pins_by_fingerprint_or_device_key() {
    let key = DeviceKey::generate().unwrap();
    let identity = Identity::from_device_key(&key).unwrap();
    let addr = host(&identity);
    let other = DeviceKey::generate().unwrap();

    assert!(connects(addr, HostPin::Key(key.public_key())).await);
    assert!(connects(addr, HostPin::Fingerprint(identity.fingerprint())).await);
    assert!(connects(addr, HostPin::None).await);

    assert!(!connects(addr, HostPin::Key(other.public_key())).await);
    let wrong = Identity::from_device_key(&other).unwrap().fingerprint();
    assert!(!connects(addr, HostPin::Fingerprint(wrong)).await);
}
