//! F3 spike: does Hext Remote's video work over `iroh`?
//!
//! `host` streams synthetic frames shaped like real video (P-frames around
//! the bitrate, a keyframe ten times larger every few seconds) through the
//! same packetizer and FEC as the app; `view` dials the host by its iroh
//! ID only, rebuilds the frames and acknowledges each one. Both sides print
//! once a second: the path in use (direct or relay, with its RTT), rates,
//! FEC and losses, and on the host the time from starting to send a frame to
//! receiving its acknowledgement.
//!
//! Datagrams: `[0] + Datagram::Video`, `[1] + frame number` (ack, viewer →
//! host), `[2]` (keyframe request, viewer → host).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hext_net::video::{Event, Packetizer, Reassembler};
use hext_proto::{Datagram, VideoFrame};
use iroh::endpoint::presets::{self, Preset};
use iroh::endpoint::{Connection, QuicTransportConfig};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use tokio::sync::Mutex;

const ALPN: &[u8] = b"hext/spike/1";

const TAG_VIDEO: u8 = 0;
const TAG_ACK: u8 = 1;
const TAG_KEYFRAME: u8 = 2;

/// Bytes a video packet adds to its shard (as in `hext_net::video`), plus the tag.
const OVERHEAD: usize = 33;

/// Largest datagram sent. With several paths (relay and direct), iroh may
/// report the size of one path while sending on another with a smaller MTU
/// (e.g. Tailscale's 1280), and it silently drops datagrams that don't fit.
/// QUIC guarantees 1200-byte packets on every path; this leaves room for
/// its headers.
const MAX_DATAGRAM: usize = 1100;

#[derive(Parser)]
#[command(
    name = "hext-spike",
    version,
    about = "Spike F3: vídeo do Hext Remote sobre iroh"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Espera uma conexão e envia vídeo sintético.
    Host {
        /// Quadros por segundo.
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Taxa de bits dos P-frames, em Mbps.
        #[arg(long, default_value_t = 8.0)]
        mbps: f64,
        /// Um keyframe (10× maior) a cada tantos segundos.
        #[arg(long, default_value_t = 2)]
        keyframe_secs: u32,
        /// Só pelo relay (sem conexão direta), para medir o pior caso.
        #[arg(long)]
        relay_only: bool,
        /// Relay próprio (ex.: https://relay.hext.com.br) no lugar dos da n0.
        #[arg(long)]
        relay: Option<RelayUrl>,
    },
    /// Conecta num host pelo ID dele e recebe o vídeo.
    View {
        /// ID mostrado pelo host.
        id: EndpointId,
        /// Só pelo relay (sem conexão direta), para medir o pior caso.
        #[arg(long)]
        relay_only: bool,
        /// Relay próprio (ex.: https://relay.hext.com.br) no lugar dos da n0.
        #[arg(long)]
        relay: Option<RelayUrl>,
        /// Por quantos segundos receber.
        #[arg(long, default_value_t = 60)]
        seconds: u64,
    },
}

/// Synthetic video shape.
#[derive(Clone, Copy)]
struct Video {
    fps: u32,
    mbps: f64,
    keyframe_secs: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "error".into()),
        )
        .init();
    match Cli::parse().command {
        Command::Host {
            fps,
            mbps,
            keyframe_secs,
            relay_only,
            relay,
        } => {
            let endpoint = endpoint(presets::N0, relay_only, relay, true).await?;
            wait_online(&endpoint).await;
            println!(
                "\n  No outro computador:\n    hext-spike view {}\n",
                endpoint.id()
            );
            let video = Video {
                fps,
                mbps,
                keyframe_secs,
            };
            host(&endpoint, video, None).await?;
            endpoint.close().await;
        }
        Command::View {
            id,
            relay_only,
            relay,
            seconds,
        } => {
            let endpoint = endpoint(presets::N0, relay_only, relay, false).await?;
            view(&endpoint, id.into(), Duration::from_secs(seconds)).await?;
            endpoint.close().await;
        }
    }
    Ok(())
}

async fn endpoint(
    preset: impl Preset,
    relay_only: bool,
    relay: Option<RelayUrl>,
    accept: bool,
) -> Result<Endpoint> {
    // The same datagram buffers as the app (see hext_net's endpoint).
    let transport = QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(4 * 1024 * 1024))
        .datagram_send_buffer_size(256 * 1024)
        .build();
    let mut builder = Endpoint::builder(preset).transport_config(transport);
    if accept {
        builder = builder.alpns(vec![ALPN.to_vec()]);
    }
    if let Some(relay) = relay {
        builder = builder.relay_mode(RelayMode::custom([relay]));
    }
    if relay_only {
        builder = builder.clear_ip_transports();
    }
    Ok(builder.bind().await?)
}

/// Waits (bounded) until the relay connection is up: without it, the viewer
/// can only reach this host on a direct address.
async fn wait_online(endpoint: &Endpoint) {
    let started = Instant::now();
    match tokio::time::timeout(Duration::from_secs(15), endpoint.online()).await {
        Ok(()) => println!(
            "online (relay ok) em {:.1} s",
            started.elapsed().as_secs_f64()
        ),
        Err(_) => println!(
            "aviso: sem relay após 15 s (firewall? proxy?); só conexões diretas devem funcionar"
        ),
    }
}

/// Accepts one viewer and streams to it until it leaves (or `stop_after`).
/// Returns how many frames it acknowledged.
async fn host(endpoint: &Endpoint, video: Video, stop_after: Option<Duration>) -> Result<usize> {
    let incoming = endpoint.accept().await.context("endpoint fechado")?;
    let conn = incoming.await?;
    println!("conectado: {} — {}", conn.remote_id(), path(&conn));

    let sent_at: Arc<Mutex<HashMap<u64, Instant>>> = Arc::default();
    let latencies: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let acked = Arc::new(AtomicUsize::new(0));
    let keyframe = Arc::new(AtomicBool::new(false));
    {
        let (conn, sent_at, latencies, acked, keyframe) = (
            conn.clone(),
            sent_at.clone(),
            latencies.clone(),
            acked.clone(),
            keyframe.clone(),
        );
        tokio::spawn(async move {
            while let Ok(datagram) = conn.read_datagram().await {
                match datagram.first() {
                    Some(&TAG_ACK) if datagram.len() == 9 => {
                        let seq = u64::from_le_bytes(datagram[1..9].try_into().expect("8 bytes"));
                        if let Some(at) = sent_at.lock().await.remove(&seq) {
                            latencies.lock().await.push(at.elapsed());
                            acked.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Some(&TAG_KEYFRAME) => keyframe.store(true, Ordering::Relaxed),
                    _ => {}
                }
            }
        });
    }

    let fps = video.fps.max(1);
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let p_frame = (video.mbps * 1_000_000.0 / 8.0 / f64::from(fps)) as usize;
    let keyframe_every = u64::from(fps * video.keyframe_secs.max(1));
    let mut packetizer = Packetizer::default();
    let mut rng = 0x2545_f491_4f6c_dd1d_u64;
    let (mut seq, mut frames, mut bytes, mut skipped) = (0u64, 0u32, 0usize, 0u32);
    let started = Instant::now();
    let mut window = Instant::now();
    let mut next = Instant::now();
    loop {
        if stop_after.is_some_and(|limit| started.elapsed() >= limit) {
            // Give the last acks a moment.
            tokio::time::sleep(Duration::from_millis(300)).await;
            conn.close(0u32.into(), b"fim");
            return Ok(acked.load(Ordering::Relaxed));
        }
        if conn.close_reason().is_some() {
            println!("conexão encerrada");
            return Ok(acked.load(Ordering::Relaxed));
        }

        let key = seq.is_multiple_of(keyframe_every) || keyframe.swap(false, Ordering::Relaxed);
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // P-frames vary ±50% around the average; keyframes are ~10×.
        let size = if key {
            p_frame * 10
        } else {
            p_frame / 2 + (rng as usize % p_frame.max(1))
        };
        let frame = VideoFrame {
            seq,
            width: 1920,
            height: 1080,
            keyframe: key,
            capture_micros: 0,
            encode_micros: 0,
            hardware: false,
            data: (0..size).map(|i| (i as u64 ^ rng) as u8).collect(),
        };
        let encoded = hext_proto::encode(&frame)?;
        let max = conn
            .max_datagram_size()
            .context("o outro lado não aceita datagramas")?
            .min(MAX_DATAGRAM);
        sent_at.lock().await.insert(seq, Instant::now());
        for packet in packetizer.packetize(seq, &encoded, max.saturating_sub(OVERHEAD)) {
            let mut datagram = vec![TAG_VIDEO];
            datagram.extend(hext_proto::encode(&Datagram::Video(packet))?);
            conn.send_datagram_wait(datagram.into()).await?;
        }
        frames += 1;
        bytes += encoded.len();
        seq += 1;

        // Like the real host: when sending runs late, frames are skipped.
        next += interval;
        let now = Instant::now();
        if next > now {
            tokio::time::sleep(next - now).await;
        } else {
            while next + interval < now {
                next += interval;
                skipped += 1;
            }
        }

        if window.elapsed() >= Duration::from_secs(1) {
            let secs = window.elapsed().as_secs_f64();
            let mut samples = std::mem::take(&mut *latencies.lock().await);
            samples.sort();
            let pct = |p: usize| {
                samples
                    .get(samples.len() * p / 100)
                    .map_or("—".to_owned(), |d| {
                        format!("{:.1} ms", d.as_secs_f64() * 1000.0)
                    })
            };
            if samples.is_empty() && seq > u64::from(fps) {
                println!("aviso: nenhuma confirmação do visualizador neste segundo");
            }
            println!(
                "{:>5.1} fps | {:>5.1} Mbps | pulados {skipped:>3} | quadro→ack p50 {} p95 {} | {}",
                f64::from(frames) / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                pct(50),
                pct(95),
                path(&conn),
            );
            // Acks that never came (lost frames) must not pile up.
            sent_at
                .lock()
                .await
                .retain(|_, at| at.elapsed() < Duration::from_secs(5));
            (window, frames, bytes, skipped) = (Instant::now(), 0, 0, 0);
        }
    }
}

/// Dials the host and receives for `duration`. Returns the frames received.
async fn view(endpoint: &Endpoint, host: EndpointAddr, duration: Duration) -> Result<usize> {
    let started = Instant::now();
    let conn = endpoint
        .connect(host, ALPN)
        .await
        .context("falha ao conectar")?;
    println!(
        "conectado em {:.2} s — {}",
        started.elapsed().as_secs_f64(),
        path(&conn)
    );

    let mut reassembler = Reassembler::new();
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    let (mut window, mut frames, mut bytes) = (Instant::now(), 0u32, 0usize);
    let mut total = 0;
    let mut direct_at: Option<Duration> = None;
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        let events = tokio::select! {
            datagram = conn.read_datagram() => {
                let Ok(datagram) = datagram else {
                    println!("o host encerrou");
                    break;
                };
                if datagram.first() != Some(&TAG_VIDEO) {
                    continue;
                }
                match hext_proto::decode::<Datagram>(&datagram[1..]) {
                    Ok(Datagram::Video(packet)) => {
                        bytes += datagram.len();
                        reassembler.push(packet, Instant::now())
                    }
                    _ => continue,
                }
            }
            _ = tick.tick() => reassembler.tick(Instant::now()),
        };
        for event in events {
            match event {
                Event::Frame(frame) => {
                    frames += 1;
                    total += 1;
                    let mut ack = vec![TAG_ACK];
                    ack.extend_from_slice(&frame.seq.to_le_bytes());
                    let _ = conn.send_datagram(ack.into());
                }
                Event::Lost => {
                    let _ = conn.send_datagram(vec![TAG_KEYFRAME].into());
                }
            }
        }

        if direct_at.is_none() && conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
            let at = started.elapsed();
            direct_at = Some(at);
            println!("conexão direta em {:.2} s", at.as_secs_f64());
        }
        if window.elapsed() >= Duration::from_secs(1) {
            let secs = window.elapsed().as_secs_f64();
            let c = reassembler.counters;
            println!(
                "{:>5.1} fps | {:>5.1} Mbps | FEC {} blocos, {} quadros perdidos (total) | {}",
                f64::from(frames) / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                c.recovered,
                c.lost,
                path(&conn),
            );
            (window, frames, bytes) = (Instant::now(), 0, 0);
        }
    }
    conn.close(0u32.into(), b"fim");
    if direct_at.is_none() {
        println!("nunca ficou direto: tudo passou pelo relay");
    }
    Ok(total)
}

/// The selected path: "direto <addr>" or "relay <url>", with its RTT.
fn path(conn: &Connection) -> String {
    let paths = conn.paths();
    let Some(selected) = paths.iter().find(|p| p.is_selected()) else {
        return "sem caminho".to_owned();
    };
    let kind = if selected.is_relay() {
        "relay"
    } else {
        "direto"
    };
    format!(
        "{kind} {:?} rtt {:.1} ms",
        selected.remote_addr(),
        selected.rtt().as_secs_f64() * 1000.0
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app's video (packetizer, FEC, reassembler) over an iroh
    /// connection, directly on this machine: no relay, no address lookup.
    #[tokio::test]
    async fn video_flows_over_iroh() {
        let host_ep = endpoint(presets::Minimal, false, None, true).await.unwrap();
        let view_ep = endpoint(presets::Minimal, false, None, false)
            .await
            .unwrap();
        let addr = host_ep.addr();
        let video = Video {
            fps: 30,
            mbps: 2.0,
            keyframe_secs: 1,
        };
        let host_task = tokio::spawn(async move {
            let acked = host(&host_ep, video, Some(Duration::from_secs(3))).await;
            host_ep.close().await;
            acked
        });
        let received = view(&view_ep, addr, Duration::from_secs(4)).await.unwrap();
        let acked = host_task.await.unwrap().unwrap();
        view_ep.close().await;
        // ~90 frames in 3 s at 30 fps; allow for start-up.
        assert!(received >= 60, "received {received}");
        assert!(acked >= 60, "acked {acked}");
    }
}
