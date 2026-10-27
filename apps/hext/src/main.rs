//! Hext Remote.
//!
//! F0: command-line skeleton that proves the transport between two machines
//! on the LAN (QUIC handshake + datagram RTT). The desktop UI and the remote
//! view replace these commands in the next phases.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hext_metrics::Rolling;
use hext_net::{Connection, Fingerprint, Identity, Incoming, NetError};
use hext_proto::{Control, DEFAULT_PORT, Datagram, PeerInfo};
use tracing::{debug, info, warn};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "hext",
    version,
    about = "Hext Remote — acesso e suporte remoto"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Aguarda conexões nesta máquina (modo host).
    Host {
        /// Endereço e porta UDP para escutar.
        #[arg(long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT))]
        listen: SocketAddr,
    },
    /// Conecta num host pelo IP e mede a latência.
    Connect {
        /// IP do host, com porta opcional (padrão 7420).
        addr: String,
        /// Impressão digital mostrada pelo host. Sem ela a conexão é cifrada mas não autenticada.
        #[arg(long)]
        fingerprint: Option<Fingerprint>,
        /// Quantidade de pings.
        #[arg(long, default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..))]
        count: u32,
        /// Intervalo entre pings, em milissegundos.
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
        interval_ms: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Host { listen } => host(listen).await,
        Command::Connect {
            addr,
            fingerprint,
            count,
            interval_ms,
        } => {
            connect(
                &addr,
                fingerprint,
                count,
                Duration::from_millis(interval_ms),
            )
            .await
        }
    }
}

fn device_name() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// Name of this executable as the user runs it (`HextRemote.exe`, `hext`…).
fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "hext".to_owned())
}

fn parse_addr(addr: &str) -> Result<SocketAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Ok(sock);
    }
    let ip: IpAddr = addr
        .parse()
        .with_context(|| format!("endereço inválido: {addr}"))?;
    Ok(SocketAddr::new(ip, DEFAULT_PORT))
}

async fn host(listen: SocketAddr) -> Result<()> {
    let identity = Identity::generate()?;
    let endpoint = hext_net::host_endpoint(listen, &identity)?;
    info!("escutando em {} (UDP)", endpoint.local_addr()?);
    println!(
        "\nImpressão digital deste host:\n  {}\n",
        identity.fingerprint()
    );
    println!(
        "No outro computador:\n  {} connect <IP-deste-PC> --fingerprint {}\n",
        exe_name(),
        identity.fingerprint()
    );

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    if let Err(e) = serve(incoming).await {
                        warn!("sessão {remote}: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    endpoint.close(0u32.into(), b"host encerrado");
    endpoint.wait_idle().await;
    Ok(())
}

async fn serve(incoming: Incoming) -> Result<()> {
    let conn = incoming.await?;
    let (mut control, client) =
        hext_net::host_handshake(&conn, PeerInfo::local(APP_VERSION, device_name())).await?;
    info!(
        "conectado: {} ({:?}, v{}) de {}",
        client.device_name,
        client.os,
        client.app_version,
        conn.remote_address()
    );
    loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Ok(Control::Bye) => {
                    // Closing here is what lets the client's `bye()` return.
                    conn.close(0u32.into(), b"bye");
                    break;
                }
                Ok(other) => warn!("mensagem inesperada: {other:?}"),
                Err(_) => break,
            },
            dg = hext_net::recv_datagram(&conn) => match dg {
                Ok(Datagram::Ping { seq, sent_micros }) => {
                    let pong = Datagram::Pong { seq, sent_micros };
                    if hext_net::send_datagram(&conn, &pong).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(NetError::Codec(e)) => debug!("datagrama ignorado: {e}"),
                // The client went away without a Bye.
                Err(_) => break,
            },
        }
    }
    info!("desconectado: {}", client.device_name);
    Ok(())
}

async fn connect(
    addr: &str,
    pin: Option<Fingerprint>,
    count: u32,
    interval: Duration,
) -> Result<()> {
    let addr = parse_addr(addr)?;
    if pin.is_none() {
        warn!("sem --fingerprint: a conexão é cifrada, mas a identidade do host NÃO é verificada");
    }
    let endpoint = hext_net::client_endpoint(addr)?;
    let started = Instant::now();
    let conn = hext_net::connect(&endpoint, addr, pin)
        .await
        .with_context(|| format!("falha ao conectar em {addr}"))?;
    let (control, host) =
        hext_net::client_handshake(&conn, PeerInfo::local(APP_VERSION, device_name())).await?;
    println!(
        "Conectado a {} ({:?}, v{}) em {:.1} ms",
        host.device_name,
        host.os,
        host.app_version,
        started.elapsed().as_secs_f64() * 1000.0
    );

    let pongs = Arc::new(Mutex::new(Pongs::new(count)));
    let clock = Instant::now();
    let receiver = tokio::spawn(collect_pongs(conn.clone(), clock, pongs.clone()));

    let mut ticker = tokio::time::interval(interval);
    for seq in 0..count {
        ticker.tick().await;
        let sent_micros = clock.elapsed().as_micros() as u64;
        hext_net::send_datagram(&conn, &Datagram::Ping { seq, sent_micros })?;
    }
    // Let in-flight pongs arrive.
    tokio::time::sleep(Duration::from_millis(500)).await;
    receiver.abort();

    let (summary, answered) = {
        let pongs = pongs.lock().expect("lock");
        (pongs.rtt.summary(), pongs.answered.len())
    };
    match summary {
        Some(summary) => {
            let lost = (count as usize).saturating_sub(answered);
            println!("RTT: {summary}");
            println!("Perdidos: {lost} de {count}");
        }
        None => println!(
            "Nenhuma resposta recebida (firewall bloqueando UDP {}?)",
            addr.port()
        ),
    }

    control.bye(&conn).await?;
    conn.close(0u32.into(), b"fim");
    endpoint.wait_idle().await;
    Ok(())
}

/// Answers received so far, counting each ping we sent at most once.
struct Pongs {
    count: u32,
    answered: HashSet<u32>,
    rtt: Rolling,
}

impl Pongs {
    fn new(count: u32) -> Self {
        Self {
            count,
            answered: HashSet::new(),
            rtt: Rolling::new(count as usize),
        }
    }

    fn record(&mut self, seq: u32, rtt: Duration) {
        // Ignore duplicates and pongs for pings we never sent.
        if seq < self.count && self.answered.insert(seq) {
            self.rtt.push(rtt);
        }
    }
}

async fn collect_pongs(conn: Connection, clock: Instant, pongs: Arc<Mutex<Pongs>>) {
    loop {
        match hext_net::recv_datagram(&conn).await {
            Ok(Datagram::Pong { seq, sent_micros }) => {
                let now = clock.elapsed().as_micros() as u64;
                let rtt = Duration::from_micros(now.saturating_sub(sent_micros));
                pongs.lock().expect("lock").record(seq, rtt);
            }
            Ok(_) => {}
            Err(NetError::Codec(e)) => debug!("datagrama ignorado: {e}"),
            Err(_) => break,
        }
    }
}
