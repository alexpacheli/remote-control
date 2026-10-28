//! Hext Remote.
//!
//! F1: `host` shares this machine's screen; `view` opens a window showing a
//! host's screen and forwards mouse and keyboard. The desktop UI (Tauri)
//! replaces the command line later.

mod bench;
mod host;
mod viewer;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use hext_net::Fingerprint;
use hext_proto::DEFAULT_PORT;

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
    /// Compartilha a tela deste computador e aceita controle remoto.
    Host(HostArgs),
    /// Abre a tela de um host e controla mouse e teclado.
    View(ViewArgs),
    /// Mede o tempo de codificação de vídeo com uma imagem da tela (macOS).
    Bench(BenchArgs),
}

#[derive(clap::Args)]
struct BenchArgs {
    /// Monitor a capturar (0 = principal).
    #[arg(long, default_value_t = 0)]
    display: usize,
    /// Quadros por configuração.
    #[arg(long, default_value_t = 180, value_parser = clap::value_parser!(u32).range(20..=10_000))]
    frames: u32,
    /// Taxa de bits do vídeo, em Mbps.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=200))]
    bitrate_mbps: u32,
    /// Quadros por segundo (o ritmo do host).
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
}

#[derive(clap::Args)]
struct HostArgs {
    /// Endereço e porta UDP para escutar.
    #[arg(long, default_value_t = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT))]
    listen: SocketAddr,
    /// Código de acesso. Sem ele, um código aleatório é gerado.
    #[arg(long)]
    code: Option<String>,
    /// Monitor a compartilhar (0 = principal).
    #[arg(long, default_value_t = 0)]
    display: usize,
    /// Transmite uma imagem de teste em vez da tela (padrão no Linux por enquanto).
    #[arg(long, default_value_t = !cfg!(any(windows, target_os = "macos")))]
    test_pattern: bool,
    /// Taxa de bits do vídeo, em Mbps.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(1..=200))]
    bitrate_mbps: u32,
    /// Limite de quadros por segundo.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
    /// Codificador de vídeo.
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    encoder: CodecChoice,
}

/// Which encoder (host) or decoder (viewer) to use.
#[derive(Clone, Copy, ValueEnum)]
enum CodecChoice {
    /// Placa de vídeo se houver; senão, ou se ela falhar, o processador.
    Auto,
    /// Sempre o processador (OpenH264).
    Software,
    /// Só a placa de vídeo; falha se não houver.
    Hardware,
}

#[derive(clap::Args)]
struct ViewArgs {
    /// IP do host, com porta opcional (padrão 7420).
    addr: String,
    /// Código de acesso mostrado pelo host.
    #[arg(long)]
    code: String,
    /// Impressão digital mostrada pelo host. Sem ela a conexão é cifrada mas não autenticada.
    #[arg(long)]
    fingerprint: Option<Fingerprint>,
    /// Visualizador no Mac, host Windows/Linux: o que a tecla Command envia.
    #[arg(long, value_enum, default_value_t = CmdKey::Ctrl)]
    cmd_key: CmdKey,
    /// Visualizador Windows/Linux, host Mac: o que a tecla Ctrl envia.
    #[arg(long, value_enum, default_value_t = CtrlKey::Cmd)]
    ctrl_key: CtrlKey,
    /// Sem janela: só recebe, decodifica e mostra estatísticas (para testes).
    #[arg(long)]
    headless: bool,
    /// No modo --headless, por quantos segundos receber.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..))]
    seconds: u64,
    /// Decodificador de vídeo.
    #[arg(long, value_enum, default_value_t = CodecChoice::Auto)]
    decoder: CodecChoice,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CtrlKey {
    /// Ctrl funciona como Command (Ctrl+C copia no Mac); a tecla Windows vira Control.
    Cmd,
    /// Ctrl continua Control.
    Ctrl,
}

#[derive(Clone, Copy, ValueEnum)]
enum CmdKey {
    /// Command funciona como Ctrl (Cmd+C copia no Windows).
    Ctrl,
    /// Command funciona como a tecla Windows.
    Win,
}

fn main() -> Result<()> {
    // quinn_udp warns on every MTU probe that a link (e.g. Tailscale, MTU
    // 1280) cannot carry; quinn just keeps the smaller size, so hide them.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,quinn_udp=error".into()
            }),
        )
        .init();

    match Cli::parse().command {
        Command::Host(args) => tokio::runtime::Runtime::new()?.block_on(host::run(args)),
        Command::View(args) => viewer::run(args),
        Command::Bench(args) => bench::run(&args),
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
