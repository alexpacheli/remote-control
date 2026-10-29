//! Hext Remote.
//!
//! F1: `host` shares this machine's screen; `view` opens a window showing a
//! host's screen and forwards mouse and keyboard. The desktop UI (Tauri)
//! replaces the command line later.

#[cfg_attr(not(windows), allow(dead_code))] // the service is Windows-only for now
mod agent;
mod bench;
mod host;
mod service;
mod signaling;
mod viewer;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use hext_proto::DEFAULT_PORT;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Our relay (São Paulo), for when the two computers cannot connect directly.
const DEFAULT_RELAY: &str = "https://relay.hext.com.br";

/// The relay to use: the `--relay` option, except that our relay serves only
/// devices registered with our server, so with another server (or none)
/// the default relay is left out.
fn relay_for(value: &str, server: Option<&str>) -> Result<Option<hext_net::RelayUrl>> {
    if value == DEFAULT_RELAY && server != Some(DEFAULT_SERVER) {
        tracing::info!(
            "sem relay: o relay padrão só atende computadores registrados no servidor padrão (--relay <url> para usar outro)"
        );
        return Ok(None);
    }
    relay(value)
}

/// Our signaling server: IDs, presence and connection requests.
const DEFAULT_SERVER: &str = "wss://signal.hext.com.br";

/// The `--server` option: a URL, or `off` (no IDs; connecting by IP only).
fn server(value: &str) -> Option<&str> {
    (!value.eq_ignore_ascii_case("off")).then_some(value)
}

/// The `--relay` option: a URL, or `off` for direct connections only.
fn relay(value: &str) -> Result<Option<hext_net::RelayUrl>> {
    if value.eq_ignore_ascii_case("off") {
        return Ok(None);
    }
    let url = value
        .parse()
        .with_context(|| format!("relay inválido: {value}"))?;
    Ok(Some(url))
}

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
    /// Instala o Hext Remote como serviço do Windows: online com o ID deste
    /// computador desde o boot, antes de alguém entrar.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// (Interno) Captura e input na sessão do usuário, a pedido do serviço.
    #[command(hide = true)]
    Agent {
        #[arg(long)]
        pipe: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Instala (ou atualiza) e inicia o serviço. Precisa de administrador.
    Install,
    /// Para e remove o serviço. Precisa de administrador.
    Uninstall {
        /// Apaga também a identidade deste computador (o ID deixa de valer).
        #[arg(long)]
        purge: bool,
    },
    /// Mostra se o serviço está rodando e o ID deste computador.
    Status,
    /// (Interno) O próprio serviço, iniciado pelo Windows.
    #[command(hide = true)]
    Run,
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
    /// Arquivo da chave deste computador (criado na primeira execução). O padrão
    /// fica na pasta de configurações do usuário.
    #[arg(long)]
    device_key: Option<std::path::PathBuf>,
    /// Servidor de sinalização: mostra o ID deste computador e pergunta aqui se
    /// aceita cada pedido de acesso (`off`: só conexões pelo IP).
    #[arg(long, default_value = DEFAULT_SERVER)]
    server: String,
    /// Relay para quando não dá para conectar direto (`off` desliga).
    #[arg(long, default_value = DEFAULT_RELAY)]
    relay: String,
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
    /// ID do host (com --server), ou o IP dele com porta opcional (padrão 7420).
    target: String,
    /// Código de acesso mostrado pelo host (para conectar pelo IP).
    #[arg(long)]
    code: Option<String>,
    /// Servidor de sinalização, para conectar pelo ID.
    #[arg(long, default_value = DEFAULT_SERVER)]
    server: String,
    /// Arquivo da chave deste computador (criado na primeira execução).
    #[arg(long)]
    device_key: Option<std::path::PathBuf>,
    /// Chave mostrada pelo host (para conectar pelo IP).
    #[arg(long)]
    key: Option<hext_crypto::PublicKey>,
    /// Relay para quando não dá para conectar direto (`off` desliga).
    #[arg(long, default_value = DEFAULT_RELAY)]
    relay: String,
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
    let command = Cli::parse().command;
    // The service and its agents have no console: they log to a file.
    let log_file = match &command {
        Command::Service {
            action: ServiceAction::Run,
        } => service::log_file("service"),
        Command::Agent { .. } => service::log_file("agent"),
        _ => None,
    };
    // noq_udp (iroh's QUIC) warns on every MTU probe that a link (e.g.
    // Tailscale, MTU 1280) cannot carry; it just keeps the smaller size.
    let logs = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,noq_udp=error".into()),
    );
    match log_file {
        Some(file) => logs
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => logs.init(),
    }

    match command {
        Command::Host(args) => tokio::runtime::Runtime::new()?.block_on(host::run(args)),
        Command::View(args) => viewer::run(args),
        Command::Bench(args) => bench::run(&args),
        Command::Service { action } => service::command(action),
        Command::Agent { pipe } => service::agent(&pipe),
    }
}

/// This computer's key, created on first run.
fn device_key(path: Option<&std::path::Path>) -> Result<hext_crypto::DeviceKey> {
    let store = match path {
        Some(path) => hext_crypto::FileKeyStore::new(path),
        None => hext_crypto::FileKeyStore::default_location()?,
    };
    let (key, created) = hext_crypto::load_or_create(&store)?;
    if created {
        tracing::info!(
            "chave deste computador criada em {}",
            store.path().display()
        );
    }
    Ok(key)
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
