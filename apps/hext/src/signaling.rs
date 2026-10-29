//! Connecting by ID through `hext-server`.
//!
//! The host stays logged in, shows its ID and asks its user about each
//! request; on a yes it hands the viewer (through the server) where to
//! connect and a one-time session code. The viewer pins the host's device
//! key, remembered per ID (trust on first use).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hext_crypto::DeviceKey;
use hext_net::{Connection, Endpoint};
use hext_session::SessionCodes;
use hext_session::host::BoxFuture;
use hext_signal::{Check, Client, ClientMsg, DeviceId, KnownHosts, Rejection, Role, ServerMsg};
use tokio::sync::{Semaphore, mpsc};
use tracing::{info, warn};

use crate::APP_VERSION;

/// How long the host's user has to answer (the server gives up at 60 s).
const ANSWER_TIME: Duration = Duration::from_secs(55);
/// Waits between attempts to reach the server, doubling up to the last.
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How long the host waits for its relay before answering a request.
const RELAY_WAIT: Duration = Duration::from_secs(5);
/// How long to try reaching the host (directly or through the relay).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Asks the host's user whether a device may connect.
pub trait Ask: Send {
    /// The question for `name` (ID `id`); a future dropped before it ends
    /// (the request was cancelled, or time ran out) is a no.
    fn ask<'a>(&'a mut self, name: &'a str, id: DeviceId) -> BoxFuture<'a, bool>;

    /// This computer's ID, once the server gave it (or a different one).
    fn show_id(&mut self, id: DeviceId);
}

/// Asks on the terminal.
pub struct Terminal(mpsc::UnboundedReceiver<String>);

impl Terminal {
    pub fn new() -> Self {
        Self(stdin_lines())
    }
}

impl Ask for Terminal {
    fn ask<'a>(&'a mut self, name: &'a str, id: DeviceId) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Lines typed before the question are not answers to it.
            while self.0.try_recv().is_ok() {}
            println!();
            println!("  \"{name}\" (ID {id}) quer acessar este computador.");
            println!("  Aceitar? [s/N]");
            self.0.recv().await.is_some_and(|l| {
                matches!(l.trim().to_lowercase().as_str(), "s" | "sim" | "y" | "yes")
            })
        })
    }

    fn show_id(&mut self, id: DeviceId) {
        println!("  Seu ID: {id}");
        println!();
    }
}

/// Host side: stays logged in to `server` (reconnecting when it drops)
/// and asks the user (`ask`) about each request. Viewers it accepts get
/// `endpoint`'s addresses (relay and direct) to reach it; `slot` is free
/// while no session runs.
#[allow(clippy::too_many_arguments)]
pub async fn host(
    server: String,
    key: Arc<DeviceKey>,
    name: String,
    (endpoint, has_relay): (Endpoint, bool),
    codes: SessionCodes,
    slot: Arc<Semaphore>,
    mut ask: Box<dyn Ask>,
) {
    let mut retry = RETRY_FIRST;
    let mut shown: Option<DeviceId> = None;
    loop {
        match Client::connect(&server, &key, &name, APP_VERSION, Role::Host).await {
            Ok(mut client) => {
                retry = RETRY_FIRST;
                if shown != Some(client.id()) {
                    ask.show_id(client.id());
                    shown = Some(client.id());
                } else {
                    info!("reconectado ao servidor");
                }
                if let Err(e) = serve(
                    &mut client,
                    ask.as_mut(),
                    (&endpoint, has_relay),
                    &codes,
                    &slot,
                )
                .await
                {
                    warn!("servidor de sinalização: {e}");
                }
            }
            Err(e) => warn!("servidor de sinalização: {e} (tentando de novo em {retry:?})"),
        }
        tokio::time::sleep(retry).await;
        retry = (retry * 2).min(RETRY_MAX);
    }
}

/// Answers requests until the connection to the server ends.
async fn serve(
    client: &mut Client,
    ask: &mut dyn Ask,
    (endpoint, has_relay): (&Endpoint, bool),
    codes: &SessionCodes,
    slot: &Semaphore,
) -> Result<(), hext_signal::ClientError> {
    loop {
        let ServerMsg::Request {
            request,
            from,
            from_key,
            from_name,
        } = client.recv().await?
        else {
            continue;
        };
        if slot.available_permits() == 0 {
            info!("pedido de {from_name} ({from}) recusado: já existe uma sessão");
            client.send(&ClientMsg::Reject { request }).await?;
            continue;
        }
        // One deadline for the question, whatever else arrives meanwhile.
        let deadline = tokio::time::sleep(ANSWER_TIME);
        tokio::pin!(deadline);
        let question = ask.ask(&from_name, from);
        tokio::pin!(question);
        let accepted = loop {
            tokio::select! {
                answer = &mut question => break answer,
                msg = client.recv() => match msg? {
                    ServerMsg::Cancelled { request: r } if r == request => {
                        println!("  O pedido foi cancelado.");
                        break false;
                    }
                    // Others asking meanwhile wait their turn by being told
                    // no: the user answers one at a time.
                    ServerMsg::Request { request: other, .. } => {
                        client.send(&ClientMsg::Reject { request: other }).await?;
                    }
                    _ => {}
                },
                _ = &mut deadline => {
                    println!("  Sem resposta: recusado.");
                    break false;
                }
            }
        };
        // A session may have started (by IP) while the user decided.
        let busy = slot.available_permits() == 0;
        if accepted && busy {
            println!("  Já existe uma sessão: o pedido foi recusado.");
        }
        let answer = if accepted && !busy {
            println!("  Aceito: conectando...");
            // The relay must be among the addresses: without it, a viewer
            // on another network has nothing to fall back on. Right after
            // starting (or a network change) it may still be connecting.
            if has_relay && endpoint.addr().relay_urls().next().is_none() {
                let _ = tokio::time::timeout(RELAY_WAIT, endpoint.online()).await;
            }
            ClientMsg::Accept {
                request,
                addrs: hext_net::addr_to_strings(&endpoint.addr()),
                // Good only from the device that asked.
                session_code: codes.issue(from_key),
            }
        } else {
            ClientMsg::Reject { request }
        };
        client.send(&answer).await?;
    }
}

/// Lines typed on the terminal, read on their own thread.
fn stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Logs in to `server` as a viewer: this also registers the device, which
/// our relay requires before it serves it.
pub async fn login(server: &str, key: &DeviceKey, name: &str) -> Result<Client> {
    Client::connect(server, key, name, APP_VERSION, Role::Viewer)
        .await
        .context("servidor de sinalização")
}

/// Viewer side: asks host `id` for access through `client` (logged in),
/// checks its key against the one remembered for that ID, and connects from
/// `endpoint` (directly or through the relay). Returns the connection and
/// the one-time code to present.
pub async fn connect(
    mut client: Client,
    id: DeviceId,
    known: &KnownHosts,
    endpoint: &Endpoint,
) -> Result<(Connection, String)> {
    println!("Pedindo acesso a {id}... aguardando o outro computador aceitar.");
    let access = match client.request_access(id).await? {
        Ok(access) => access,
        Err(reason) => bail!("{}", rejection(reason)),
    };
    let _ = client.close().await;

    let check = known
        .check(id, &access.host_key)
        .with_context(|| known.path().display().to_string())?;
    if let Check::Changed { before } = check {
        bail!(
            "a chave do computador {id} mudou (antes {before}, agora {}). Pode ser outro computador \
             se passando por ele. Se ele foi reinstalado e você confia nisso, apague a linha do ID \
             {} em {}",
            access.host_key,
            id.get(),
            known.path().display()
        );
    }

    let addr = hext_net::addr_from_strings(&access.host_key, &access.addrs)?;
    let conn = match tokio::time::timeout(CONNECT_TIMEOUT, hext_net::connect(endpoint, addr)).await
    {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => bail!("o outro computador aceitou, mas não foi possível conectar nele: {e}"),
        Err(_) => {
            bail!("o outro computador aceitou, mas não respondeu (nem direto nem pelo relay)")
        }
    };
    // The handshake proved the host holds that key.
    if check == Check::New {
        known
            .remember(id, &access.host_key)
            .with_context(|| known.path().display().to_string())?;
        info!("primeira conexão com {id}: chave guardada");
    }
    Ok((conn, access.session_code))
}

fn rejection(reason: Rejection) -> &'static str {
    match reason {
        Rejection::Declined => "o outro computador recusou o acesso",
        Rejection::Offline => "esse ID não está online",
        Rejection::Timeout => "ninguém respondeu no outro computador a tempo",
        Rejection::TooManyRequests => {
            "muitas tentativas seguidas, deste computador ou para esse ID; tente de novo mais tarde"
        }
        Rejection::Busy => "o outro computador já tem um pedido esperando resposta",
        Rejection::SelfConnect => "esse é o ID deste computador",
    }
}
