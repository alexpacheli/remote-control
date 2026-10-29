//! The installed host is two processes (ARCHITECTURE 3.4): the service,
//! which owns the network and outlives logons, and an agent in the user's
//! session, which captures, encodes and injects input there. They talk over
//! a local pipe; this module is both ends of it.
//!
//! The session with the viewer belongs to the service ([`AgentHub`], a
//! [`Desktop`]), so it survives the agent being replaced (a logoff, another
//! user): the new agent picks up the stream with a keyframe.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use hext_codec::EncoderSettings;
use hext_proto::{InputEvent, VideoFrame};
use hext_session::SessionError;
use hext_session::host::{BoxFuture, Desktop, LocalDesktop, Screen};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

/// More than any message (a keyframe of a 4K screen is a few MB).
const MAX_MESSAGE: usize = 64 << 20;
/// How long a session waits for an agent to start streaming.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Service → agent.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToAgent {
    /// Start streaming the screen (again, if it was), as stream `id`.
    Start {
        id: u64,
        stream: Stream,
    },
    Keyframe,
    Input(InputEvent),
    /// Stop streaming (the session ended).
    Stop,
}

/// Agent → service.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromAgent {
    /// The answer to `Start` `id`: streaming, or why not.
    Started { id: u64, result: Result<(), String> },
    /// A frame of stream `id` (frames of an older one may still be on the
    /// way when the next starts).
    Frame { id: u64, frame: VideoFrame },
}

/// What to stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Stream {
    pub display: usize,
    pub test_pattern: bool,
    pub encoder: EncoderSettings,
}

impl Stream {
    fn desktop(&self) -> LocalDesktop {
        LocalDesktop {
            display: self.display,
            test_pattern: self.test_pattern,
            encoder: self.encoder,
        }
    }
}

/// Writes one message: its length (u32, little endian), then postcard.
pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let bytes = hext_proto::encode(msg).map_err(std::io::Error::other)?;
    writer
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

/// Reads one message; `None` at a clean end of the stream.
pub async fn read_msg<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> std::io::Result<Option<T>> {
    let mut len = [0; 4];
    match reader.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(std::io::Error::other(format!(
            "mensagem grande demais: {len}"
        )));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    hext_proto::decode(&bytes)
        .map(Some)
        .map_err(std::io::Error::other)
}

/// A task that ends with its owner (when the owner is dropped, too).
struct Task<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------------------------------------------------------------- service

/// The service's end: whichever agent is connected now, and the session
/// streaming through it.
#[derive(Clone, Default)]
pub struct AgentHub {
    state: Arc<Mutex<HubState>>,
}

#[derive(Default)]
struct HubState {
    /// The agent connected now, by connection number.
    agent: Option<(u64, mpsc::UnboundedSender<ToAgent>)>,
    next_agent: u64,
    session: Option<Session>,
    /// Numbers each session's stream, so frames of one never reach another.
    next_stream: u64,
}

struct Session {
    id: u64,
    stream: Stream,
    frames: mpsc::Sender<VideoFrame>,
    /// Told when the first agent answers `Start`.
    started: Option<oneshot::Sender<Result<(), String>>>,
    /// Frame numbers as the viewer sees them: a new agent starts over at 0.
    seq: u64,
}

impl AgentHub {
    /// Serves an agent connected through `pipe` until it disconnects;
    /// it replaces the one before, if any.
    pub async fn attach<P: AsyncRead + AsyncWrite + Send + 'static>(&self, pipe: P) {
        let (mut reader, mut writer) = tokio::io::split(pipe);
        let (tx, mut rx) = mpsc::unbounded_channel::<ToAgent>();
        let id = {
            let mut state = self.state.lock().expect("lock");
            let id = state.next_agent;
            state.next_agent += 1;
            // A session in progress continues on the new agent.
            if let Some(session) = &state.session {
                let _ = tx.send(ToAgent::Start {
                    id: session.id,
                    stream: session.stream,
                });
            }
            state.agent = Some((id, tx));
            id
        };
        // Forgets this agent however this ends (also when cancelled).
        let _attached = Attached {
            hub: self.clone(),
            id,
        };
        let _writing = Task(tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write_msg(&mut writer, &msg).await.is_err() {
                    break;
                }
            }
        }));
        loop {
            match read_msg::<_, FromAgent>(&mut reader).await {
                Ok(Some(FromAgent::Started { id, result })) => {
                    let mut state = self.state.lock().expect("lock");
                    if let Some(session) = state.session.as_mut().filter(|s| s.id == id) {
                        if let Some(started) = session.started.take() {
                            let _ = started.send(result);
                        } else if let Err(e) = result {
                            warn!("agent: {e}");
                        }
                    }
                }
                Ok(Some(FromAgent::Frame { id, mut frame })) => {
                    let frames = {
                        let mut state = self.state.lock().expect("lock");
                        let Some(session) = state.session.as_mut().filter(|s| s.id == id) else {
                            continue;
                        };
                        frame.seq = session.seq;
                        session.seq += 1;
                        session.frames.clone()
                    };
                    // Waits while the network is busy: backpressure on the
                    // agent through the pipe.
                    let _ = frames.send(frame).await;
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("agent: {e}");
                    break;
                }
            }
        }
    }

    fn send(&self, msg: ToAgent) {
        if let Some((_, agent)) = &self.state.lock().expect("lock").agent {
            let _ = agent.send(msg);
        }
    }

    /// Ends the session's streaming (the agent releases any held key).
    pub fn stop(&self) {
        self.state.lock().expect("lock").session = None;
        self.send(ToAgent::Stop);
    }

    /// A [`Desktop`] streaming `stream` through the agents.
    pub fn desktop(&self, stream: Stream) -> AgentDesktop {
        AgentDesktop {
            hub: self.clone(),
            stream,
        }
    }
}

/// An agent's connection to the hub, until dropped.
struct Attached {
    hub: AgentHub,
    id: u64,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let mut state = self.hub.state.lock().expect("lock");
        if state.agent.as_ref().is_some_and(|(a, _)| *a == self.id) {
            state.agent = None;
        }
    }
}

/// The screen of the user's session, through the agents of an [`AgentHub`].
pub struct AgentDesktop {
    hub: AgentHub,
    stream: Stream,
}

impl Desktop for AgentDesktop {
    fn open(
        &self,
        frames: mpsc::Sender<VideoFrame>,
    ) -> BoxFuture<'_, hext_session::Result<Box<dyn Screen>>> {
        Box::pin(async move {
            let (started_tx, started_rx) = oneshot::channel();
            {
                let mut state = self.hub.state.lock().expect("lock");
                let id = state.next_stream;
                state.next_stream += 1;
                state.session = Some(Session {
                    id,
                    stream: self.stream,
                    frames,
                    started: Some(started_tx),
                    seq: 0,
                });
                // Without an agent now, the next one to connect starts it.
                if let Some((_, agent)) = &state.agent {
                    let _ = agent.send(ToAgent::Start {
                        id,
                        stream: self.stream,
                    });
                }
            }
            let screen = AgentScreen {
                hub: self.hub.clone(),
            };
            let failed = |message: String| {
                screen.end();
                Err(SessionError::Protocol(message))
            };
            match tokio::time::timeout(START_TIMEOUT, started_rx).await {
                Ok(Ok(Ok(()))) => Ok(Box::new(screen) as Box<dyn Screen>),
                Ok(Ok(Err(e))) => failed(format!("agent: {e}")),
                Ok(Err(_)) => failed("agent: sessão substituída".into()),
                Err(_) => failed("nenhum agent na sessão do usuário respondeu".into()),
            }
        })
    }

    fn bitrate_bps(&self) -> u32 {
        self.stream.encoder.bitrate_bps
    }
}

struct AgentScreen {
    hub: AgentHub,
}

impl AgentScreen {
    fn end(&self) {
        self.hub.stop();
    }
}

impl Screen for AgentScreen {
    fn request_keyframe(&self) {
        self.hub.send(ToAgent::Keyframe);
    }

    fn inject(&mut self, event: &InputEvent) {
        self.hub.send(ToAgent::Input(*event));
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        // The agent releases held keys when it stops.
        self.end();
        Box::pin(async {})
    }
}

// ---------------------------------------------------------------- agent

/// The agent's end: streams this session's screen on the service's
/// request, until the pipe closes.
pub async fn serve<P: AsyncRead + AsyncWrite + Send + 'static>(pipe: P) -> std::io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(pipe);
    // Frames wait here while the pipe is busy (backpressure on capture).
    let (out_tx, mut out_rx) = mpsc::channel::<FromAgent>(2);
    let mut writing = Task(tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            write_msg(&mut writer, &msg).await?;
        }
        Ok::<_, std::io::Error>(())
    }));
    let mut screen: Option<(Box<dyn Screen>, Task<()>)> = None;
    let result = loop {
        let msg = match read_msg::<_, ToAgent>(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        match msg {
            ToAgent::Start { id, stream } => {
                stop(&mut screen).await;
                let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(1);
                let answer = match stream.desktop().open(frames_tx).await {
                    Ok(opened) => {
                        info!("transmitindo a tela");
                        let out = out_tx.clone();
                        let forward = Task(tokio::spawn(async move {
                            while let Some(frame) = frames_rx.recv().await {
                                if out.send(FromAgent::Frame { id, frame }).await.is_err() {
                                    break;
                                }
                            }
                        }));
                        screen = Some((opened, forward));
                        Ok(())
                    }
                    Err(e) => {
                        warn!("abrir a tela: {e}");
                        Err(e.to_string())
                    }
                };
                if out_tx
                    .send(FromAgent::Started { id, result: answer })
                    .await
                    .is_err()
                {
                    break Ok(());
                }
            }
            ToAgent::Keyframe => {
                if let Some((screen, _)) = &screen {
                    screen.request_keyframe();
                }
            }
            ToAgent::Input(event) => {
                if let Some((screen, _)) = &mut screen {
                    screen.inject(&event);
                }
            }
            ToAgent::Stop => stop(&mut screen).await,
        }
    };
    stop(&mut screen).await;
    drop(out_tx);
    let _ = (&mut writing.0).await;
    result
}

async fn stop(screen: &mut Option<(Box<dyn Screen>, Task<()>)>) {
    if let Some((screen, _forward)) = screen.take() {
        screen.close().await;
        info!("transmissão encerrada");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stream() -> Stream {
        Stream {
            display: 0,
            test_pattern: true,
            encoder: EncoderSettings::default(),
        }
    }

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let start = ToAgent::Start {
            id: 7,
            stream: test_stream(),
        };
        write_msg(&mut a, &start).await.unwrap();
        write_msg(&mut a, &ToAgent::Keyframe).await.unwrap();
        drop(a);
        assert!(matches!(
            read_msg::<_, ToAgent>(&mut b).await.unwrap(),
            Some(ToAgent::Start { id: 7, stream }) if stream.test_pattern
        ));
        assert!(matches!(
            read_msg::<_, ToAgent>(&mut b).await.unwrap(),
            Some(ToAgent::Keyframe)
        ));
        assert!(read_msg::<_, ToAgent>(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_oversized_message_is_refused() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_le_bytes()).await.unwrap();
        assert!(read_msg::<_, ToAgent>(&mut b).await.is_err());
    }

    /// Connects a new agent (the test pattern) to `hub`.
    fn agent(hub: &AgentHub) -> tokio::task::JoinHandle<()> {
        let (service_end, agent_end) = tokio::io::duplex(1 << 20);
        let hub = hub.clone();
        tokio::spawn(async move { hub.attach(service_end).await });
        tokio::spawn(async move {
            let _ = serve(agent_end).await;
        })
    }

    async fn next(frames: &mut mpsc::Receiver<VideoFrame>) -> VideoFrame {
        tokio::time::timeout(Duration::from_secs(10), frames.recv())
            .await
            .expect("a frame in time")
            .expect("frames")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_survives_the_agent_being_replaced() {
        let hub = AgentHub::default();
        let first = agent(&hub);
        let desktop = hub.desktop(test_stream());
        let (tx, mut frames) = mpsc::channel(1);
        let screen = desktop.open(tx).await.unwrap();
        let a = next(&mut frames).await;
        assert!(a.keyframe);
        let b = next(&mut frames).await;
        assert_eq!(b.seq, a.seq + 1);

        // The user logs off: that agent goes, another comes.
        first.abort();
        let _second = agent(&hub);
        // Frames keep coming, numbered on, with a keyframe from the new one.
        let mut last = b.seq;
        let resumed = loop {
            let frame = next(&mut frames).await;
            assert_eq!(frame.seq, last + 1, "no gap or restart in the numbering");
            last = frame.seq;
            if frame.keyframe {
                break frame;
            }
        };
        assert!(resumed.seq > b.seq);
        screen.close().await;
    }

    #[tokio::test]
    async fn frames_of_an_older_stream_are_dropped() {
        let hub = AgentHub::default();
        let (service_end, agent_end) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let desktop = hub.desktop(test_stream());
        let (tx, mut frames) = mpsc::channel(4);
        let opening = tokio::spawn(async move { desktop.open(tx).await.map(|_| ()) });

        // A fake agent: a frame still on the way from an older stream,
        // then the answer and a frame of this one.
        let (mut reader, mut writer) = tokio::io::split(agent_end);
        let Some(ToAgent::Start { id, .. }) = read_msg(&mut reader).await.unwrap() else {
            panic!("expected Start");
        };
        let frame = |data: u8| VideoFrame {
            seq: 41,
            width: 2,
            height: 2,
            keyframe: true,
            capture_micros: 0,
            encode_micros: 0,
            hardware: false,
            data: vec![data],
        };
        let old = FromAgent::Frame {
            id: id + 1,
            frame: frame(1),
        };
        write_msg(&mut writer, &old).await.unwrap();
        let started = FromAgent::Started { id, result: Ok(()) };
        write_msg(&mut writer, &started).await.unwrap();
        let current = FromAgent::Frame {
            id,
            frame: frame(2),
        };
        write_msg(&mut writer, &current).await.unwrap();

        opening.await.unwrap().unwrap();
        let got = next(&mut frames).await;
        assert_eq!(
            (got.data, got.seq),
            (vec![2], 0),
            "only this stream's, from 0"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_waits_for_an_agent() {
        let hub = AgentHub::default();
        let desktop = hub.desktop(test_stream());
        let (tx, mut frames) = mpsc::channel(1);
        let opening = tokio::spawn(async move { desktop.open(tx).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _agent = agent(&hub);
        opening.await.unwrap().unwrap();
        assert!(next(&mut frames).await.keyframe);
    }
}
