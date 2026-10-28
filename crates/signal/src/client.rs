//! The app's side of signaling.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hext_crypto::{DeviceKey, PublicKey};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::{ClientMsg, DeviceId, PROTOCOL, Rejection, SIGNAL_PATH, ServerMsg, auth_message};

/// How long logging in may take.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("servidor de sinalização: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("mensagem inválida do servidor: {0}")]
    Json(#[from] serde_json::Error),
    #[error("o servidor respondeu fora do protocolo: {0}")]
    Protocol(String),
    #[error("o servidor recusou: {0}")]
    Server(String),
    #[error(
        "versão do protocolo de sinalização incompatível: a nossa é {local}, a do servidor é {remote}"
    )]
    Version { local: u16, remote: u16 },
    #[error("o servidor de sinalização fechou a conexão")]
    Closed,
    #[error("o servidor de sinalização não respondeu a tempo")]
    Timeout,
}

type Result<T, E = ClientError> = std::result::Result<T, E>;

/// Longest wait for a host's answer (the server gives up sooner).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(90);

/// A logged-in connection to the server.
pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    id: DeviceId,
    next_request: u64,
}

/// A host's yes: what to expect and how to get in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Access {
    /// The host's device key: pin it.
    pub host_key: PublicKey,
    /// Where it can be reached (`ip:port`).
    pub addrs: Vec<String>,
    /// Present it instead of an access code; good for one session.
    pub session_code: String,
}

impl Client {
    /// Connects to `server` (`wss://host` or, for tests, `ws://host:port`)
    /// and logs in with `key`.
    pub async fn connect(
        server: &str,
        key: &DeviceKey,
        device_name: &str,
        app_version: &str,
    ) -> Result<Self> {
        let url = format!("{}{SIGNAL_PATH}", server.trim_end_matches('/'));
        let login = async {
            let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
                url,
                None,
                false,
                Some(Connector::Rustls(tls_config())),
            )
            .await?;
            let mut client = Self {
                ws,
                // Replaced once the server says.
                id: DeviceId::new(DeviceId::MIN).expect("a valid ID"),
                next_request: 1,
            };
            let nonce = match client.recv().await? {
                ServerMsg::Challenge { protocol, nonce } if protocol == PROTOCOL => nonce,
                ServerMsg::Challenge { protocol, .. } => {
                    return Err(ClientError::Version {
                        local: PROTOCOL,
                        remote: protocol,
                    });
                }
                other => return Err(unexpected(&other)),
            };
            client
                .send(&ClientMsg::Hello {
                    protocol: PROTOCOL,
                    public_key: key.public_key(),
                    signature: key.sign(&auth_message(&nonce)),
                    device_name: device_name.to_owned(),
                    app_version: app_version.to_owned(),
                })
                .await?;
            match client.recv().await? {
                ServerMsg::Welcome { id } => client.id = id,
                other => return Err(unexpected(&other)),
            }
            Ok(client)
        };
        tokio::time::timeout(LOGIN_TIMEOUT, login)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    /// This device's ID.
    pub fn id(&self) -> DeviceId {
        self.id
    }

    /// Asks device `to` for access and waits for its user's answer (or the
    /// server's: offline, timeout…).
    pub async fn request_access(&mut self, to: DeviceId) -> Result<Result<Access, Rejection>> {
        let request = self.next_request;
        self.next_request += 1;
        self.send(&ClientMsg::Connect { request, to }).await?;
        let answer = async {
            loop {
                match self.recv().await? {
                    ServerMsg::Accepted {
                        request: r,
                        host_key,
                        addrs,
                        session_code,
                    } if r == request => {
                        return Ok(Ok(Access {
                            host_key,
                            addrs,
                            session_code,
                        }));
                    }
                    ServerMsg::Rejected { request: r, reason } if r == request => {
                        return Ok(Err(reason));
                    }
                    _ => {} // about something else
                }
            }
        };
        tokio::time::timeout(ANSWER_TIMEOUT, answer)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        self.ws.send(Message::text(msg.to_json())).await?;
        Ok(())
    }

    /// The next message from the server. [`ServerMsg::Error`] ends the
    /// connection and comes back as [`ClientError::Server`].
    pub async fn recv(&mut self) -> Result<ServerMsg> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    return match serde_json::from_str(&text)? {
                        ServerMsg::Error { message } => Err(ClientError::Server(message)),
                        msg => Ok(msg),
                    };
                }
                Some(Ok(Message::Close(_))) | None => return Err(ClientError::Closed),
                // Pings are answered by the library.
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            }
        }
    }

    /// Says goodbye and closes the connection.
    pub async fn close(mut self) -> Result<()> {
        self.ws.close(None).await?;
        Ok(())
    }
}

fn unexpected(msg: &ServerMsg) -> ClientError {
    match msg {
        ServerMsg::Error { message } => ClientError::Server(message.clone()),
        other => ClientError::Protocol(format!("{other:?}")),
    }
}

/// TLS with the usual public CAs, on the same crypto (ring) as the rest
/// (built once: the app reconnects often).
fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG.get_or_init(build_tls_config).clone()
}

fn build_tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}
