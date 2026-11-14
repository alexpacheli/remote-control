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
use hext_proto::{AudioPacket, Displays, InputEvent, VideoFrame};
use hext_session::SessionError;
use hext_session::host::{BoxFuture, Desktop, LocalDesktop, Screen, next_displays};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
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
        /// Tell text copied here (the session has the permission).
        clipboard: bool,
    },
    Keyframe,
    Input(InputEvent),
    /// Text from the viewer, for this session's clipboard.
    Clipboard(String),
    /// Stream this display from now on.
    SelectDisplay(usize),
    /// Stop streaming (the session ended).
    Stop,
    /// The indicator for where the interface cannot be seen (the sign-in
    /// screen, a locked computer); `None`: no session.
    Indicator(Option<Indicator>),
    /// Turn the virtual monitor on (and stream it) or off.
    VirtualDisplay(bool),
    /// The virtual monitor's next size.
    NextVirtualSize,
    /// The privacy screen on or off (it goes off with the agent: a new one
    /// starts without it, and the viewer is told).
    Privacy(bool),
    /// Record the computer's sound (for the viewer), or stop.
    Audio(bool),
}

/// The session, as the agent shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Indicator {
    pub text: String,
    /// Also on the user's own desktop: no interface there has the bar.
    pub everywhere: bool,
}

/// Agent → service.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromAgent {
    /// The answer to `Start` `id`: streaming, or why not.
    Started { id: u64, result: Result<(), String> },
    /// A frame of stream `id` (frames of an older one may still be on the
    /// way when the next starts).
    Frame { id: u64, frame: VideoFrame },
    /// Text copied during stream `id`.
    Clipboard { id: u64, text: String },
    /// The displays there are and the one stream `id` shows.
    Displays { id: u64, displays: Displays },
    /// Someone pressed Encerrar on the indicator.
    EndSession,
    /// 10 ms of the computer's sound, during stream `id`.
    Audio { id: u64, packet: AudioPacket },
}

/// What to stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Stream {
    pub display: usize,
    pub test_pattern: bool,
    pub encoder: EncoderSettings,
    /// The session asked for the virtual monitor (the next agent turns it
    /// on too).
    pub virtual_display: bool,
}

impl Stream {
    fn desktop(&self) -> LocalDesktop {
        LocalDesktop {
            display: self.display,
            test_pattern: self.test_pattern,
            encoder: self.encoder,
            virtual_display: self.virtual_display,
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
pub struct Task<T>(pub tokio::task::JoinHandle<T>);

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
    /// Sends Ctrl+Alt+Del, which only the service itself can.
    secure_attention: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Ends the session, for the indicator.
    end_session: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct HubState {
    /// The agent connected now, by connection number.
    agent: Option<(u64, mpsc::UnboundedSender<ToAgent>)>,
    next_agent: u64,
    session: Option<Session>,
    /// Numbers each session's stream, so frames of one never reach another.
    next_stream: u64,
    /// What the indicator shows during a session.
    indicator: Option<Indicator>,
}

struct Session {
    id: u64,
    stream: Stream,
    frames: mpsc::Sender<VideoFrame>,
    clipboard: Option<mpsc::Sender<String>>,
    displays: watch::Sender<Displays>,
    /// Told when the first agent answers `Start`.
    started: Option<oneshot::Sender<Result<(), String>>>,
    /// Frame numbers as the viewer sees them: a new agent starts over at 0.
    seq: u64,
    /// Where the sound goes, and whether the viewer wants it (the next
    /// agent records it too).
    audio: mpsc::Sender<AudioPacket>,
    audio_on: bool,
}

impl AgentHub {
    /// Serves an agent connected through `pipe` until it disconnects;
    /// it replaces the one before, if any.
    pub async fn attach<P: AsyncRead + AsyncWrite + Send + 'static>(&self, pipe: P) {
        let (mut reader, mut writer) = tokio::io::split(pipe);
        let (tx, mut rx) = mpsc::unbounded_channel::<ToAgent>();
        let agent = {
            let mut state = self.state.lock().expect("lock");
            let agent = state.next_agent;
            state.next_agent += 1;
            // A session in progress continues on the new agent.
            if let Some(session) = &state.session {
                let _ = tx.send(ToAgent::Start {
                    id: session.id,
                    stream: session.stream,
                    clipboard: session.clipboard.is_some(),
                });
                if session.audio_on {
                    let _ = tx.send(ToAgent::Audio(true));
                }
            }
            if state.indicator.is_some() {
                let _ = tx.send(ToAgent::Indicator(state.indicator.clone()));
            }
            state.agent = Some((agent, tx));
            agent
        };
        // Forgets this agent however this ends (also when cancelled).
        let _attached = Attached {
            hub: self.clone(),
            id: agent,
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
                Ok(Some(FromAgent::Clipboard { id, text })) => {
                    let state = self.state.lock().expect("lock");
                    let session = state.session.as_ref().filter(|s| s.id == id);
                    if let Some(clipboard) = session.and_then(|s| s.clipboard.as_ref()) {
                        let _ = clipboard.try_send(text);
                    }
                }
                Ok(Some(FromAgent::Displays { id, displays })) => {
                    let mut state = self.state.lock().expect("lock");
                    // (The news of an agent on its way out is about a capture
                    // that is over.)
                    let replaced = state.agent.as_ref().is_none_or(|(a, _)| *a != agent);
                    if let Some(session) =
                        state.session.as_mut().filter(|s| s.id == id && !replaced)
                    {
                        // The next agent picks up on the same display.
                        session.stream.display = displays.current.into();
                        session.displays.send_replace(displays);
                    }
                }
                Ok(Some(FromAgent::Audio { id, packet })) => {
                    let state = self.state.lock().expect("lock");
                    // (Full: the network is behind, and this is late already.)
                    if let Some(session) = state.session.as_ref().filter(|s| s.id == id) {
                        let _ = session.audio.try_send(packet);
                    }
                }
                Ok(Some(FromAgent::EndSession)) => {
                    if let Some(end) = &self.end_session {
                        end();
                    }
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

    /// A hub that sends Ctrl+Alt+Del with `send` (the agents cannot) and
    /// ends the session with `end`.
    pub fn new(
        send: impl Fn() + Send + Sync + 'static,
        end: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self {
            secure_attention: Some(Arc::new(send)),
            end_session: Some(Arc::new(end)),
            ..Self::default()
        }
    }

    /// What the indicator shows; `None`: no session.
    pub fn set_indicator(&self, indicator: Option<Indicator>) {
        self.state.lock().expect("lock").indicator = indicator.clone();
        self.send(ToAgent::Indicator(indicator));
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
        clipboard: Option<mpsc::Sender<String>>,
    ) -> BoxFuture<'_, hext_session::Result<Box<dyn Screen>>> {
        Box::pin(async move {
            let (started_tx, started_rx) = oneshot::channel();
            let share = clipboard.is_some();
            let (displays_tx, displays) = watch::channel(Displays::default());
            let (audio_tx, audio) = mpsc::channel(hext_session::host::AUDIO_QUEUE);
            {
                let mut state = self.hub.state.lock().expect("lock");
                let id = state.next_stream;
                state.next_stream += 1;
                state.session = Some(Session {
                    id,
                    stream: self.stream,
                    frames,
                    clipboard,
                    displays: displays_tx,
                    started: Some(started_tx),
                    seq: 0,
                    audio: audio_tx,
                    audio_on: false,
                });
                // Without an agent now, the next one to connect starts it.
                if let Some((_, agent)) = &state.agent {
                    let _ = agent.send(ToAgent::Start {
                        id,
                        stream: self.stream,
                        clipboard: share,
                    });
                }
            }
            let screen = AgentScreen {
                hub: self.hub.clone(),
                displays,
                audio: Some(audio),
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
    displays: watch::Receiver<Displays>,
    /// Taken by `audio`.
    audio: Option<mpsc::Receiver<AudioPacket>>,
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
        if *event == InputEvent::SecureAttention {
            if let Some(send) = &self.hub.secure_attention {
                send();
            }
            return;
        }
        self.hub.send(ToAgent::Input(*event));
    }

    fn set_clipboard(&mut self, text: String) {
        self.hub.send(ToAgent::Clipboard(text));
    }

    fn select_display(&self, index: usize) {
        // Kept for an agent that is yet to come; the one there corrects it
        // if the display does not open.
        if let Some(session) = &mut self.hub.state.lock().expect("lock").session {
            session.stream.display = index;
        }
        self.hub.send(ToAgent::SelectDisplay(index));
    }

    fn set_virtual_display(&self, on: bool) {
        // Kept for an agent that is yet to come, as the display is.
        if let Some(session) = &mut self.hub.state.lock().expect("lock").session {
            session.stream.virtual_display = on;
        }
        self.hub.send(ToAgent::VirtualDisplay(on));
    }

    fn next_virtual_size(&self) {
        self.hub.send(ToAgent::NextVirtualSize);
    }

    fn set_privacy(&self, on: bool) {
        self.hub.send(ToAgent::Privacy(on));
    }

    fn displays(&self) -> Option<watch::Receiver<Displays>> {
        Some(self.displays.clone())
    }

    fn audio(&mut self) -> Option<mpsc::Receiver<AudioPacket>> {
        self.audio.take()
    }

    fn set_audio(&self, on: bool) {
        // Kept for an agent that is yet to come, as the display is.
        if let Some(session) = &mut self.hub.state.lock().expect("lock").session {
            session.audio_on = on;
        }
        self.hub.send(ToAgent::Audio(on));
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
    let mut indicator = None;
    let result = loop {
        let msg = match read_msg::<_, ToAgent>(&mut reader).await {
            Ok(Some(msg)) => msg,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        match msg {
            ToAgent::Start {
                id,
                stream,
                clipboard,
            } => {
                stop(&mut screen).await;
                let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(1);
                let (copied_tx, mut copied_rx) = mpsc::channel::<String>(4);
                let copied_tx = clipboard.then_some(copied_tx);
                let answer = match stream.desktop().open(frames_tx, copied_tx).await {
                    Ok(mut opened) => {
                        info!("transmitindo a tela");
                        let out = out_tx.clone();
                        let mut audio = opened.audio();
                        let mut displays = opened.displays();
                        // What there is now is news for the service, too.
                        if let Some(displays) = &mut displays {
                            displays.mark_changed();
                        }
                        let forward = Task(tokio::spawn(async move {
                            loop {
                                let msg = tokio::select! {
                                    shown = next_displays(&mut displays) => {
                                        FromAgent::Displays { id, displays: shown }
                                    }
                                    frame = frames_rx.recv() => match frame {
                                        Some(frame) => FromAgent::Frame { id, frame },
                                        None => break,
                                    },
                                    Some(text) = copied_rx.recv() => {
                                        FromAgent::Clipboard { id, text }
                                    }
                                    packet = next_audio(&mut audio) => {
                                        FromAgent::Audio { id, packet }
                                    }
                                };
                                if out.send(msg).await.is_err() {
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
            ToAgent::Clipboard(text) => {
                if let Some((screen, _)) = &mut screen {
                    screen.set_clipboard(text);
                }
            }
            ToAgent::SelectDisplay(index) => {
                if let Some((screen, _)) = &screen {
                    screen.select_display(index);
                }
            }
            ToAgent::VirtualDisplay(on) => {
                if let Some((screen, _)) = &screen {
                    screen.set_virtual_display(on);
                }
            }
            ToAgent::NextVirtualSize => {
                if let Some((screen, _)) = &screen {
                    screen.next_virtual_size();
                }
            }
            ToAgent::Privacy(on) => {
                if let Some((screen, _)) = &screen {
                    screen.set_privacy(on);
                }
            }
            ToAgent::Audio(on) => {
                if let Some((screen, _)) = &screen {
                    screen.set_audio(on);
                }
            }
            ToAgent::Stop => stop(&mut screen).await,
            ToAgent::Indicator(shown) => {
                // (Not here: it waits for its window, and input does not.)
                let old = indicator.take();
                tokio::task::spawn_blocking(move || drop(old));
                indicator = shown.and_then(|shown| show_indicator(&shown, out_tx.clone()));
            }
        }
    };
    stop(&mut screen).await;
    drop(indicator);
    drop(out_tx);
    let _ = (&mut writing.0).await;
    result
}

/// The indicator on the sign-in screen, until dropped.
#[cfg(windows)]
fn show_indicator(
    shown: &Indicator,
    out: mpsc::Sender<FromAgent>,
) -> Option<crate::service::Banner> {
    crate::service::Banner::show(&shown.text, shown.everywhere, move || {
        // (On a thread of its own: it can wait for room among the frames.)
        let _ = out.blocking_send(FromAgent::EndSession);
    })
}

/// (Only the Windows service has an agent.)
#[cfg(not(windows))]
fn show_indicator(_shown: &Indicator, _out: mpsc::Sender<FromAgent>) -> Option<Box<()>> {
    None
}

/// The next sound packet; with no sound, this waits on.
async fn next_audio(audio: &mut Option<mpsc::Receiver<AudioPacket>>) -> AudioPacket {
    if let Some(audio) = audio
        && let Some(packet) = audio.recv().await
    {
        return packet;
    }
    std::future::pending().await
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
            virtual_display: false,
        }
    }

    #[tokio::test]
    async fn messages_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let start = ToAgent::Start {
            id: 7,
            stream: test_stream(),
            clipboard: false,
        };
        write_msg(&mut a, &start).await.unwrap();
        write_msg(&mut a, &ToAgent::Keyframe).await.unwrap();
        drop(a);
        assert!(matches!(
            read_msg::<_, ToAgent>(&mut b).await.unwrap(),
            Some(ToAgent::Start { id: 7, stream, .. }) if stream.test_pattern
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
        let screen = desktop.open(tx, None).await.unwrap();
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

    #[tokio::test(flavor = "multi_thread")]
    async fn ctrl_alt_del_is_sent_by_the_service() {
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = sent.clone();
        let hub = AgentHub::new(
            move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
            || {},
        );
        let _agent = agent(&hub);
        let (tx, _frames) = mpsc::channel(1);
        let mut screen = hub.desktop(test_stream()).open(tx, None).await.unwrap();
        screen.inject(&InputEvent::SecureAttention);
        assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), 1);
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
        let opening = tokio::spawn(async move { desktop.open(tx, None).await.map(|_| ()) });

        // A fake agent: a frame still on the way from an older stream,
        // then the answer and a frame of this one.
        let (mut reader, mut writer) = tokio::io::split(agent_end);
        let Some(ToAgent::Start { id, .. }) = read_msg(&mut reader).await.unwrap() else {
            panic!("expected Start");
        };
        let frame = |data: u8| VideoFrame {
            seq: 41,
            display: 0,
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

    #[tokio::test]
    async fn the_sound_goes_through_the_agent_and_stays_on_for_the_next() {
        let hub = AgentHub::default();
        let (service_end, agent_end) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let desktop = hub.desktop(test_stream());
        let (tx, _frames) = mpsc::channel(4);
        let opening = tokio::spawn(async move { desktop.open(tx, None).await });

        let (mut reader, mut writer) = tokio::io::split(agent_end);
        let Some(ToAgent::Start { id, .. }) = read_msg(&mut reader).await.unwrap() else {
            panic!("expected Start");
        };
        let started = FromAgent::Started { id, result: Ok(()) };
        write_msg(&mut writer, &started).await.unwrap();
        let mut screen = opening.await.unwrap().unwrap();
        let mut heard = screen.audio().expect("the agent has sound");
        screen.set_audio(true);
        assert!(matches!(
            read_msg(&mut reader).await.unwrap(),
            Some(ToAgent::Audio(true))
        ));
        let packet = |seq| AudioPacket {
            seq,
            data: vec![1; 8],
            prev: Vec::new(),
        };
        // An older stream's sound is dropped.
        let old = FromAgent::Audio {
            id: id + 1,
            packet: packet(1),
        };
        write_msg(&mut writer, &old).await.unwrap();
        let current = FromAgent::Audio {
            id,
            packet: packet(2),
        };
        write_msg(&mut writer, &current).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), heard.recv())
            .await
            .unwrap();
        assert_eq!(got, Some(packet(2)));

        // The next agent records it from the start, too.
        let (service_end, agent_end) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let (mut reader, _writer) = tokio::io::split(agent_end);
        assert!(matches!(
            read_msg(&mut reader).await.unwrap(),
            Some(ToAgent::Start { .. })
        ));
        assert!(matches!(
            read_msg(&mut reader).await.unwrap(),
            Some(ToAgent::Audio(true))
        ));
    }

    #[tokio::test]
    async fn the_clipboard_goes_through_the_agent_both_ways() {
        let hub = AgentHub::default();
        let (service_end, agent_end) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let desktop = hub.desktop(test_stream());
        let (tx, _frames) = mpsc::channel(1);
        let (copied_tx, mut copied) = mpsc::channel(1);
        let opening = tokio::spawn(async move { desktop.open(tx, Some(copied_tx)).await });

        let (mut reader, mut writer) = tokio::io::split(agent_end);
        let Some(ToAgent::Start {
            id,
            clipboard: true,
            ..
        }) = read_msg(&mut reader).await.unwrap()
        else {
            panic!("expected Start with the clipboard");
        };
        let started = FromAgent::Started { id, result: Ok(()) };
        write_msg(&mut writer, &started).await.unwrap();
        let mut screen = opening.await.unwrap().unwrap();

        // An older stream's text goes nowhere.
        for (id, text) in [(id + 1, "antigo"), (id, "copiado")] {
            let text = text.into();
            let msg = FromAgent::Clipboard { id, text };
            write_msg(&mut writer, &msg).await.unwrap();
        }
        assert_eq!(copied.recv().await.as_deref(), Some("copiado"));

        screen.set_clipboard("colado".into());
        assert!(matches!(
            read_msg::<_, ToAgent>(&mut reader).await.unwrap(),
            Some(ToAgent::Clipboard(text)) if text == "colado"
        ));
    }

    /// The next frame `width` wide.
    async fn until_width(frames: &mut mpsc::Receiver<VideoFrame>, width: u32) -> VideoFrame {
        loop {
            let frame = next(frames).await;
            if frame.width == width {
                return frame;
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_display_chosen_goes_through_the_agent_and_stays_with_the_next() {
        let hub = AgentHub::default();
        let first = agent(&hub);
        let (tx, mut frames) = mpsc::channel(1);
        let screen = hub.desktop(test_stream()).open(tx, None).await.unwrap();
        let mut displays = screen.displays().expect("told through the service");
        assert_eq!(until_width(&mut frames, 1280).await.width, 1280);

        screen.select_display(1);
        assert!(until_width(&mut frames, 640).await.keyframe);
        tokio::time::timeout(Duration::from_secs(5), async {
            while displays.borrow_and_update().current != 1 {
                displays.changed().await.unwrap();
            }
        })
        .await
        .expect("the service hears of it");
        let shown = *displays.borrow();
        assert_eq!((shown.count, shown.current), (2, 1));

        // The user logs off: the new agent shows the same display.
        first.abort();
        let _second = agent(&hub);
        tokio::time::sleep(Duration::from_millis(300)).await;
        for _ in 0..5 {
            assert_eq!(next(&mut frames).await.width, 640);
        }
        screen.close().await;
    }

    #[tokio::test]
    async fn an_agent_replaced_has_no_say_in_the_displays() {
        let hub = AgentHub::default();
        let (service_end, first) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let desktop = hub.desktop(test_stream());
        let (tx, mut frames) = mpsc::channel(4);
        let opening = tokio::spawn(async move { desktop.open(tx, None).await });
        let (mut reader, mut writer) = tokio::io::split(first);
        let Some(ToAgent::Start { id, .. }) = read_msg(&mut reader).await.unwrap() else {
            panic!("expected Start");
        };
        let started = FromAgent::Started { id, result: Ok(()) };
        write_msg(&mut writer, &started).await.unwrap();
        let screen = opening.await.unwrap().unwrap();
        let displays = screen.displays().expect("told through the service");

        let (service_end, second) = tokio::io::duplex(1 << 20);
        let attached = hub.clone();
        tokio::spawn(async move { attached.attach(service_end).await });
        let (mut second, _writer) = tokio::io::split(second);
        let Some(ToAgent::Start { .. }) = read_msg(&mut second).await.unwrap() else {
            panic!("the session goes on on the new agent");
        };

        let displays_news = FromAgent::Displays {
            id,
            displays: Displays {
                count: 2,
                current: 1,
                ..Default::default()
            },
        };
        write_msg(&mut writer, &displays_news).await.unwrap();
        // Read in order: once this frame is out, the news was heard.
        let frame = VideoFrame {
            seq: 0,
            display: 0,
            width: 2,
            height: 2,
            keyframe: true,
            capture_micros: 0,
            encode_micros: 0,
            hardware: false,
            data: vec![1],
        };
        let frame = FromAgent::Frame { id, frame };
        write_msg(&mut writer, &frame).await.unwrap();
        next(&mut frames).await;
        assert!(!displays.has_changed().unwrap(), "the capture is over");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_waits_for_an_agent() {
        let hub = AgentHub::default();
        let desktop = hub.desktop(test_stream());
        let (tx, mut frames) = mpsc::channel(1);
        let opening = tokio::spawn(async move { desktop.open(tx, None).await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let _agent = agent(&hub);
        opening.await.unwrap().unwrap();
        assert!(next(&mut frames).await.keyframe);
    }
}
