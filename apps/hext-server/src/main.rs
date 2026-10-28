use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use hext_server::{Config, Db, State};
use tracing::info;

/// Servidor do Hext Remote: IDs dos computadores, presença e sinalização.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Endereço HTTP para escutar (o TLS fica no proxy na frente, ex.: Caddy).
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Arquivo do banco de dados SQLite.
    #[arg(long, default_value = "hext-server.db")]
    db: PathBuf,
    /// Atrás de um proxy (Caddy) na mesma máquina: o endereço de cada
    /// computador vem do cabeçalho X-Forwarded-For.
    #[arg(long)]
    behind_proxy: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let db = Db::open(&args.db).with_context(|| format!("banco {}", args.db.display()))?;
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("escutar em {}", args.listen))?;
    info!(
        "hext-server {} em http://{}",
        env!("CARGO_PKG_VERSION"),
        args.listen
    );
    let state = Arc::new(State::new(db));
    tokio::select! {
        served = hext_server::serve(listener, state, Config { behind_proxy: args.behind_proxy }) => served?,
        _ = tokio::signal::ctrl_c() => info!("encerrando"),
    }
    Ok(())
}
