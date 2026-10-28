use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use hext_codec::{EncoderPreference, EncoderSettings};
use hext_net::{Identity, NetError};
use hext_proto::{PeerInfo, RejectReason};
use hext_session::host::{self, HostConfig};
use hext_session::{SessionCodes, SessionError};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::{APP_VERSION, CodecChoice, HostArgs, device_name, exe_name};

/// Delay after a wrong access code, so guessing is limited to one try per second.
const WRONG_CODE_PENALTY: Duration = Duration::from_secs(1);

pub async fn run(args: HostArgs) -> Result<()> {
    let key = Arc::new(crate::device_key(args.device_key.as_deref())?);
    let identity = Identity::from_device_key(&key)?;
    let endpoint = hext_net::host_endpoint(args.listen, &identity)?;
    let code = match args.code {
        Some(code) => code,
        None => random_code()?,
    };
    // An all-whitespace code would normalize to empty and admit anyone.
    if code.chars().filter(|c| c.is_ascii_digit()).count() < 4 {
        anyhow::bail!("o código de acesso precisa de ao menos 4 dígitos");
    }
    if args.test_pattern {
        warn!("transmitindo a imagem de teste, não a tela (--test-pattern)");
    } else {
        // Keep running either way: Accessibility takes effect for the next
        // session; Screen Recording (macOS) only after restarting the host.
        if let Err(e) = hext_capture::preflight() {
            warn!("{e}");
        }
        hext_input::preflight();
    }

    let port = endpoint.local_addr()?.port();
    // Where other machines reach this one: the address listened on, or the
    // LAN address when listening on all.
    let reachable = match args.listen.ip() {
        ip if ip.is_unspecified() => local_ip(),
        ip => Some(ip),
    };
    let ip = reachable.map_or_else(|| "<IP-deste-PC>".to_owned(), |ip| ip.to_string());
    println!();
    println!("  Hext Remote — aguardando conexão");
    println!("  Endereço:          {ip}:{port}");
    println!("  Código de acesso:  {}", group(&code));
    println!("  Impressão digital: {}", identity.fingerprint());
    println!();
    println!("  No outro computador:");
    println!(
        "    {} view {ip} --code {code} --fingerprint {}",
        exe_name(),
        identity.fingerprint()
    );
    if let Some(server) = &args.server {
        println!(
            "  ou, pelo ID (mostrado abaixo): {} view <ID> --server {server}",
            exe_name()
        );
    }
    println!();

    let session_codes = SessionCodes::default();
    let config = Arc::new(HostConfig {
        peer: PeerInfo::local(APP_VERSION, device_name()),
        access_code: code,
        session_codes: session_codes.clone(),
        display: args.display,
        test_pattern: args.test_pattern,
        encoder: EncoderSettings {
            bitrate_bps: args.bitrate_mbps * 1_000_000,
            max_fps: args.fps as f32,
            preference: match args.encoder {
                CodecChoice::Auto => EncoderPreference::Auto,
                CodecChoice::Software => EncoderPreference::Software,
                CodecChoice::Hardware => EncoderPreference::Hardware,
            },
        },
    });
    // One session at a time, handshake included: attempts are serialized,
    // which together with the penalty rate-limits code guessing.
    let slot = Arc::new(Semaphore::new(1));

    if let Some(server) = args.server.clone() {
        // Viewers accepted through the server connect to this address.
        let Some(ip) = reachable else {
            anyhow::bail!(
                "não foi possível descobrir o IP deste computador na rede; informe-o com --listen <IP>:{port}"
            );
        };
        let addr = SocketAddr::new(ip, port);
        tokio::spawn(crate::signaling::host(
            server,
            key.clone(),
            device_name(),
            addr,
            session_codes,
            slot.clone(),
        ));
    }

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let Ok(permit) = slot.clone().try_acquire_owned() else {
                    info!("recusando {}: já existe uma sessão", incoming.remote_address());
                    incoming.refuse();
                    continue;
                };
                let config = config.clone();
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    let result = match incoming.await {
                        Ok(conn) => host::serve(conn, config).await,
                        Err(e) => Err(NetError::from(e).into()),
                    };
                    match result {
                        Ok(_) => {}
                        Err(SessionError::Net(NetError::Rejected(RejectReason::WrongCode))) => {
                            warn!("{remote}: código de acesso errado");
                            tokio::time::sleep(WRONG_CODE_PENALTY).await;
                        }
                        Err(e) => warn!("sessão {remote}: {e}"),
                    }
                    drop(permit);
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    endpoint.close(0u32.into(), b"host encerrado");
    endpoint.wait_idle().await;
    Ok(())
}

/// A random 9-digit code, like "482913557".
fn random_code() -> Result<String> {
    let mut bytes = [0u8; 8];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("sem fonte de números aleatórios"))?;
    Ok(format!("{:09}", u64::from_le_bytes(bytes) % 1_000_000_000))
}

/// "482913557" → "482 913 557".
fn group(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    chars
        .chunks(3)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The LAN address other machines would use to reach this one.
/// Connecting a UDP socket sends nothing; it only picks the outgoing interface.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0))).ok()?;
    socket.connect(SocketAddr::from(([192, 0, 2, 1], 9))).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_nine_digits_and_grouped() {
        let code = random_code().unwrap();
        assert_eq!(code.len(), 9);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(group("482913557"), "482 913 557");
    }
}
