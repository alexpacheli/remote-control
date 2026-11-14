//! Client (viewer) side of a session.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_codec::{DecoderPreference, YuvFrame};
use hext_net::video::{Counters, Event, LOG_TARGET, MAX_WAITING, Reassembler, ResendRequest};
use hext_net::{Connection, ConnectionError, ControlStream, FramedSend, NetError, PasswordLogin};
use hext_proto::{
    AudioPacket, ChatText, Control, Datagram, Displays, ENDED_ON_HOST, FileOutcome, Hello,
    InputEvent, PeerInfo, Permissions, StreamKind, VideoFrame, VirtualDisplay,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::chat_pace::ChatPace;
use crate::files::{self, Confirm, FileAccess, FileEnd, Tell};
use crate::{NETWORK_GRACE, RESTART_LIFETIME, Result, ResumeToken, SessionError, Stats};

/// Frames waiting for the decoder at most. A keyframe takes a while to
/// decode, and frames held back by a missing one arrive all at once, so a
/// short queue would give up and ask for a keyframe at the worst moment.
const DECODE_QUEUE: usize = MAX_WAITING + 2;

/// Sound packets waiting for the player (10 ms each): the player keeps its
/// own short queue, this only rides out a busy moment.
const AUDIO_QUEUE: usize = 32;

/// A decoder this many frames behind for this long is not catching up.
const BEHIND_FRAMES: usize = 3;
const BEHIND_FOR: Duration = Duration::from_millis(250);

/// Whether the decoder keeps falling behind: a queue that drains again is a
/// burst, one that stays is delay that would never go away.
#[derive(Default)]
struct Backlog {
    /// Where the queue settled lately, and since when.
    settled: Option<(usize, Instant)>,
}

impl Backlog {
    fn behind(&mut self, queued: usize, now: Instant) -> bool {
        if queued <= BEHIND_FRAMES {
            self.settled = None;
            return false;
        }
        match self.settled {
            // One frame of slack: arrivals and decodes take turns. Going up
            // starts over too, or the first frames of a burst would count as
            // where the queue stands while it is still draining it; one that
            // only ever grows ends at the full queue.
            Some((settled, since)) if queued == settled || queued == settled + 1 => {
                let behind = now.duration_since(since) > BEHIND_FOR;
                if behind {
                    // Skipping to a keyframe is the answer: what stays in the
                    // queue while it arrives is not a second verdict.
                    self.settled = None;
                }
                behind
            }
            _ => {
                self.settled = Some((queued, now));
                false
            }
        }
    }
}

/// A frame taking longer than this to decode is worth a log line.
const SLOW_DECODE: Duration = Duration::from_millis(33);

/// What the host said about coming back and about restarting.
#[derive(Debug, Default)]
struct Resume {
    token: Option<ResumeToken>,
    /// The host's answer to the last request to restart.
    restart: Option<bool>,
    /// Why the connection ended, if it did before this side closed it:
    /// closing replaces the reason it reports.
    ended: Option<ConnectionError>,
}

/// The session's side of the file traffic, until [`ClientHandle::files`].
struct FileLink {
    paths: mpsc::UnboundedReceiver<PathBuf>,
    answers: mpsc::UnboundedReceiver<(u64, FileOutcome)>,
    outcomes: mpsc::UnboundedSender<(u64, FileOutcome)>,
}

/// What the chat brings from the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatNews {
    Text(ChatText),
    /// Nobody there can read what was sent.
    NoOne,
}

/// A decoded picture ready to be shown.
pub struct DecodedFrame {
    pub seq: u64,
    /// The display of the host it shows.
    pub display: u8,
    pub yuv: YuvFrame,
}

/// A running client session.
pub struct ClientHandle {
    pub host: PeerInfo,
    /// What the host lets this viewer do (it refuses the rest anyway).
    pub permissions: Permissions,
    pub stats: Arc<Mutex<Stats>>,
    conn: Connection,
    input: mpsc::UnboundedSender<InputEvent>,
    restart: mpsc::UnboundedSender<()>,
    clipboard_out: mpsc::UnboundedSender<String>,
    /// Taken by [`Self::clipboard`].
    clipboard_in: Option<mpsc::Receiver<String>>,
    send_file: mpsc::UnboundedSender<PathBuf>,
    /// Taken by [`Self::files`].
    file_link: Option<FileLink>,
    displays: watch::Receiver<Displays>,
    /// [`Control::SelectDisplay`] and [`Control::VirtualDisplay`].
    select_display: mpsc::UnboundedSender<Control>,
    chat_out: mpsc::UnboundedSender<ChatText>,
    /// Taken by [`Self::chat`].
    chat_in: Option<mpsc::Receiver<ChatNews>>,
    /// Taken by [`Self::audio`].
    audio_in: Option<mpsc::Receiver<AudioPacket>>,
    /// Whether this viewer wants the host's sound (not muted).
    audio_on: watch::Sender<bool>,
    resume: Arc<Mutex<Resume>>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
    /// The last round-trip time seen, for while paths change.
    last_rtt: Mutex<Duration>,
}

impl ClientHandle {
    /// Queues an input event for the host. Dropped silently once the session is over.
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Current round-trip time estimate of the connection (the last one
    /// known while no path is selected, e.g. switching relay → direct).
    pub fn rtt(&self) -> Duration {
        let mut last = self.last_rtt.lock().expect("lock");
        if let Some(path) = self.path() {
            *last = path.rtt;
        }
        *last
    }

    /// The path in use: direct (peer to peer) or through the relay.
    pub fn path(&self) -> Option<hext_net::PathInfo> {
        hext_net::path(&self.conn)
    }

    /// Where to send text copied here, and where the host's arrives; the
    /// second ends with the session. Once. The host ignores the first and
    /// sends nothing without [`Permissions::clipboard`].
    pub fn clipboard(&mut self) -> Option<(mpsc::UnboundedSender<String>, mpsc::Receiver<String>)> {
        let incoming = self.clipboard_in.take()?;
        Some((self.clipboard_out.clone(), incoming))
    }

    /// Queues a file for the host. It takes [`Self::files`] running, and
    /// [`Permissions::files`].
    pub fn send_file(&self, path: PathBuf) {
        let _ = self.send_file.send(path);
    }

    /// The file traffic of this session, to be run for as long as it lasts:
    /// what comes goes to `access`, once `confirm` says yes (the host gave
    /// the permission, so the viewer's person has the say), and `tell` hears
    /// how each file goes. Once; afterwards, it does nothing.
    pub fn files(
        &mut self,
        access: Arc<dyn FileAccess>,
        tell: Tell,
        confirm: Confirm,
    ) -> impl Future<Output = ()> + Send + 'static {
        let end = self.file_link.take().map(|link| FileEnd {
            conn: self.conn.clone(),
            allowed: self.permissions.files,
            access: Some(access),
            paths: link.paths,
            answers: link.answers,
            outcomes: link.outcomes,
            tell,
            confirm: Some(confirm),
        });
        async move {
            if let Some(end) = end {
                files::run(end).await;
            }
        }
    }

    /// Sends a message to the host's user.
    pub fn send_chat(&self, text: ChatText) {
        let _ = self.chat_out.send(text);
    }

    /// What the chat brings. Once; afterwards, `None`.
    pub fn chat(&mut self) -> Option<mpsc::Receiver<ChatNews>> {
        self.chat_in.take()
    }

    /// The host's sound as it arrives (it sends it to a viewer that asked,
    /// [`Hello::audio`]). Once; afterwards, `None`.
    pub fn audio(&mut self) -> Option<mpsc::Receiver<AudioPacket>> {
        self.audio_in.take()
    }

    /// Asks the host to send its sound, or to stop (muted). Told to a host
    /// that offers it ([`Displays::audio`]), as soon as it says so.
    pub fn set_audio(&self, on: bool) {
        self.audio_on.send_replace(on);
    }

    /// The host's displays, as it told last (none, until it does).
    pub fn displays(&self) -> Displays {
        *self.displays.borrow()
    }

    /// Completes at each news of [`Self::displays`], until the session ends.
    pub fn displays_changed(&self) -> watch::Receiver<Displays> {
        self.displays.clone()
    }

    /// Asks the host to show display `index`; [`DecodedFrame::display`] says
    /// when the picture got there.
    pub fn select_display(&self, index: u8) {
        let _ = self.select_display.send(Control::SelectDisplay { index });
    }

    /// Asks the host to turn its virtual monitor on (and show it) or off.
    /// Only to a host that offers one ([`Displays::virtual_display`]): an
    /// older host would not know the message.
    pub fn virtual_display(&self, on: bool) {
        if self.displays().virtual_display != VirtualDisplay::None {
            let _ = self.select_display.send(Control::VirtualDisplay { on });
        }
    }

    /// Asks the host to turn its privacy screen on or off, if it has one
    /// ([`Displays::privacy`]).
    pub fn privacy(&self, on: bool) {
        if self.displays().privacy != hext_proto::Privacy::None {
            let _ = self.select_display.send(Control::Privacy { on });
        }
    }

    /// Asks the host for its virtual monitor's next size, if it can change
    /// it ([`VirtualDisplay::On`]).
    pub fn next_virtual_size(&self) {
        if matches!(
            self.displays().virtual_display,
            VirtualDisplay::On {
                resizable: true,
                ..
            }
        ) {
            let _ = self.select_display.send(Control::NextVirtualSize);
        }
    }

    /// Asks the host to restart the computer. It refuses without
    /// [`Permissions::restart`].
    pub fn request_restart(&self) {
        let mut resume = self.resume.lock().expect("lock");
        if resume.restart == Some(true) {
            return; // already on its way
        }
        resume.restart = None;
        let _ = self.restart.send(());
    }

    /// The host's answer to [`Self::request_restart`], once it came.
    pub fn restart_answer(&self) -> Option<bool> {
        self.resume.lock().expect("lock").restart
    }

    /// Completes when the connection ends, however it does.
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        let conn = self.conn.clone();
        async move {
            conn.closed().await;
        }
    }

    /// The token to come back with, once the host gave it.
    pub fn token(&self) -> Option<ResumeToken> {
        self.resume.lock().expect("lock").token
    }

    /// Once the connection ended: the token to come back with and how long
    /// there is to use it, if this end was not meant as the last.
    pub fn comes_back(&self) -> Option<(ResumeToken, Duration)> {
        let resume = self.resume.lock().expect("lock");
        let token = resume.token?;
        let reason = resume.ended.clone().or_else(|| self.conn.close_reason());
        info!(?reason, "conexão terminou");
        if let Some(ConnectionError::ApplicationClosed(close)) = &reason
            && close.reason.as_ref() == ENDED_ON_HOST
        {
            return None;
        }
        if resume.restart == Some(true) {
            return Some((token, RESTART_LIFETIME));
        }
        matches!(
            reason,
            Some(ConnectionError::TimedOut | ConnectionError::Reset)
        )
        .then_some((token, NETWORK_GRACE))
    }

    /// `true` once the session ended (host left, network lost or closed).
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Says goodbye to the host and waits for the session to wind down.
    pub async fn close(mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task
            .await
            .map_err(|e| SessionError::Protocol(format!("session task: {e}")))?
    }
}

/// Performs the handshake and starts streaming. `on_frame` runs on the
/// decoder thread for every decoded picture.
pub async fn start(
    conn: Connection,
    hello: Hello,
    password: Option<PasswordLogin>,
    decoder: DecoderPreference,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> Result<ClientHandle> {
    let audio_asked = hello.audio;
    let (control, host, permissions) = hext_net::client_handshake(&conn, hello, password).await?;
    let input_stream = hext_net::open_uni(&conn, StreamKind::Input).await?;

    let stats = Arc::new(Mutex::new(Stats::default()));
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (restart_tx, restart_rx) = mpsc::unbounded_channel();
    let (clipboard_out, clipboard_out_rx) = mpsc::unbounded_channel();
    let (clipboard_in_tx, clipboard_in) = mpsc::channel(4);
    let (send_file, paths) = mpsc::unbounded_channel();
    let (answers_tx, answers) = mpsc::unbounded_channel();
    let (outcomes, outcomes_rx) = mpsc::unbounded_channel();
    let (displays_tx, displays) = watch::channel(Displays::default());
    let (select_display, select_rx) = mpsc::unbounded_channel();
    let (chat_out, chat_out_rx) = mpsc::unbounded_channel();
    // A few slots: a burst nobody is taking loses its last messages.
    let (chat_in_tx, chat_in) = mpsc::channel(32);
    let (audio_in_tx, audio_in) = mpsc::channel(AUDIO_QUEUE);
    let (audio_on, audio_on_rx) = watch::channel(audio_asked);
    let resume = Arc::new(Mutex::new(Resume::default()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        conn.clone(),
        control,
        input_stream,
        input_rx,
        restart_rx,
        clipboard_out_rx,
        clipboard_in_tx,
        answers_tx,
        outcomes_rx,
        displays_tx,
        select_rx,
        chat_out_rx,
        chat_in_tx,
        Sound {
            packets: audio_in_tx,
            wanted: audio_on_rx,
            host_sends: audio_asked,
        },
        resume.clone(),
        shutdown_rx,
        stats.clone(),
        decoder,
        ready_tx,
        on_frame,
    ));
    // A decoder that cannot open (e.g. `Hardware` without hardware) fails
    // the session here instead of leaving it without a picture.
    let ready = ready_rx
        .await
        .unwrap_or_else(|_| Err(SessionError::Protocol("decoder thread died".into())));
    if let Err(e) = ready {
        task.abort();
        conn.close(1u32.into(), b"decoder unavailable");
        return Err(e);
    }
    Ok(ClientHandle {
        host,
        permissions,
        stats,
        conn,
        input: input_tx,
        restart: restart_tx,
        clipboard_out,
        clipboard_in: Some(clipboard_in),
        send_file,
        file_link: Some(FileLink {
            paths,
            answers,
            outcomes,
        }),
        displays,
        select_display,
        chat_out,
        chat_in: Some(chat_in),
        audio_in: Some(audio_in),
        audio_on,
        resume,
        shutdown: Some(shutdown_tx),
        task,
        last_rtt: Mutex::new(Duration::ZERO),
    })
}

#[allow(clippy::too_many_arguments)]
async fn run(
    conn: Connection,
    mut control: ControlStream,
    mut input_stream: FramedSend,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut restart_rx: mpsc::UnboundedReceiver<()>,
    mut clipboard_out: mpsc::UnboundedReceiver<String>,
    clipboard_in: mpsc::Sender<String>,
    file_answers: mpsc::UnboundedSender<(u64, FileOutcome)>,
    mut file_outcomes: mpsc::UnboundedReceiver<(u64, FileOutcome)>,
    displays: watch::Sender<Displays>,
    mut select_display: mpsc::UnboundedReceiver<Control>,
    mut chat_out: mpsc::UnboundedReceiver<ChatText>,
    chat_in: mpsc::Sender<ChatNews>,
    mut sound: Sound,
    resume: Arc<Mutex<Resume>>,
    mut shutdown: oneshot::Receiver<()>,
    stats: Arc<Mutex<Stats>>,
    decoder: DecoderPreference,
    ready: oneshot::Sender<Result<()>>,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> Result<()> {
    // Decoding is CPU-bound, so it gets its own thread, behind a queue.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(DECODE_QUEUE);
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel::<()>();
    let (loss_tx, video_conn, video_stats) = (keyframe_tx.clone(), conn.clone(), stats.clone());
    let decoder = std::thread::Builder::new()
        .name("hext-decode".into())
        .spawn(move || {
            decode_loop(
                &mut frames_rx,
                &keyframe_tx,
                &stats,
                decoder,
                ready,
                on_frame,
            )
        })
        .map_err(|e| SessionError::Protocol(format!("decoder thread: {e}")))?;
    let (resend_tx, mut resend_rx) = mpsc::unbounded_channel();
    let mut receiver = tokio::spawn(receive_video(
        video_conn,
        frames_tx,
        loss_tx,
        resend_tx,
        sound.packets.clone(),
        video_stats,
    ));

    let mut say_bye = false;
    let mut chat_pace = ChatPace::new();
    let result = loop {
        tokio::select! {
            done = &mut receiver => break match done {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => peer_closed_is_ok(&conn, e),
                Err(e) => Err(SessionError::Protocol(format!("video: {e}"))),
            },
            Some(event) = input_rx.recv() => {
                if let Err(e) = send_input(&mut input_stream, &mut input_rx, event).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(()) = keyframe_rx.recv() => {
                if let Err(e) = control.send(&Control::RequestKeyframe).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(ResendRequest { frame, packets }) = resend_rx.recv() => {
                if let Err(e) = control.send(&Control::ResendVideo { frame, packets }).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(()) = restart_rx.recv() => {
                if let Err(e) = control.send(&Control::Restart).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(text) = clipboard_out.recv() => {
                if let Err(e) = control.send(&Control::Clipboard { text }).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some((id, outcome)) = file_outcomes.recv() => {
                if let Err(e) = control.send(&Control::FileAnswer { id, outcome }).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(msg) = select_display.recv() => {
                if let Err(e) = control.send(&msg).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Some(text) = chat_out.recv() => {
                if let Err(e) = control.send(&Control::Chat { text }).await {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            Ok(()) = sound.wanted.changed() => {
                let shown = *displays.borrow();
                if let Some(msg) = sound.tell(&shown)
                    && let Err(e) = control.send(&msg).await
                {
                    break peer_closed_is_ok(&conn, e);
                }
            }
            msg = control.recv() => match msg {
                Ok(Control::Clipboard { text }) => {
                    let _ = clipboard_in.try_send(text);
                }
                Ok(Control::Chat { text }) => {
                    if !text.is_empty() && chat_pace.allows(Instant::now()) {
                        let _ = chat_in.try_send(ChatNews::Text(text));
                    }
                }
                Ok(Control::ChatNoOne) => {
                    if chat_pace.allows(Instant::now()) {
                        let _ = chat_in.try_send(ChatNews::NoOne);
                    }
                }
                Ok(Control::FileAnswer { id, outcome }) => {
                    let _ = file_answers.send((id, outcome));
                }
                // (The numbers are the host's word: nothing past the list.)
                Ok(Control::Displays(shown)) if shown.current < shown.count => {
                    displays.send_replace(shown);
                    // Muted before the host said it has sound (coming back).
                    if let Some(msg) = sound.tell(&shown)
                        && let Err(e) = control.send(&msg).await
                    {
                        break peer_closed_is_ok(&conn, e);
                    }
                }
                // The host ended it on purpose: nothing to come back to.
                Ok(Control::Bye) => {
                    resume.lock().expect("lock").token = None;
                    break Ok(());
                }
                Ok(Control::ResumeToken(token)) => {
                    resume.lock().expect("lock").token = Some(token);
                }
                Ok(Control::RestartAnswer { ok }) => {
                    resume.lock().expect("lock").restart = Some(ok);
                }
                Ok(other) => debug!("mensagem inesperada: {other:?}"),
                Err(e) => break peer_closed_is_ok(&conn, e),
            },
            _ = &mut shutdown => {
                say_bye = true;
                break Ok(());
            }
        }
    };

    if say_bye {
        let _ = control.bye(&conn).await;
    }
    resume.lock().expect("lock").ended = conn.close_reason();
    conn.close(0u32.into(), b"fim");
    // Stopping the receiver drops the queue's sender, which ends the decoder.
    receiver.abort();
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    result
}

/// The host's sound, on this side: where it goes, and whether the host is
/// sending it as this viewer wants.
struct Sound {
    packets: mpsc::Sender<AudioPacket>,
    /// Not muted.
    wanted: watch::Receiver<bool>,
    /// What the host was last asked for (first in the [`Hello`]).
    host_sends: bool,
}

impl Sound {
    /// What to tell the host, if it has sound and does not do as wanted.
    fn tell(&mut self, displays: &Displays) -> Option<Control> {
        let on = *self.wanted.borrow_and_update();
        if !displays.audio || on == self.host_sends {
            return None;
        }
        self.host_sends = on;
        Some(Control::Audio { on })
    }
}

/// Rebuilds frames from the host's video datagrams and queues them for the
/// decoder; a frame lacking packets despite FEC asks the host for them, and
/// for a keyframe if they don't come. Sound datagrams go to `audio`, as
/// they come.
async fn receive_video(
    conn: Connection,
    frames: mpsc::Sender<VideoFrame>,
    keyframe: mpsc::UnboundedSender<()>,
    resend: mpsc::UnboundedSender<ResendRequest>,
    audio: mpsc::Sender<AudioPacket>,
    stats: Arc<Mutex<Stats>>,
) -> hext_net::Result<()> {
    let mut reassembler = Reassembler::new();
    let mut report = ArrivalReport::new(&conn);
    let mut backlog = Backlog::default();
    // Also notices a lost frame when nothing newer arrives (a still screen).
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let events = tokio::select! {
            datagram = hext_net::recv_datagram(&conn) => match datagram.inspect(|_| report.read += 1) {
                Ok(Datagram::Video(packet)) => reassembler.push(packet, Instant::now()),
                Ok(Datagram::VideoIdle { last_frame }) => {
                    reassembler.idle(last_frame, Instant::now())
                }
                // (Full: nobody plays it, or the player is stuck.)
                Ok(Datagram::Audio(packet)) => {
                    let _ = audio.try_send(packet);
                    continue;
                }
                Ok(_) => continue,
                // One malformed datagram; the connection is fine.
                Err(NetError::Codec(e)) => {
                    debug!("datagrama inválido: {e}");
                    continue;
                }
                Err(e) => return Err(e),
            },
            _ = tick.tick() => reassembler.tick(Instant::now()),
        };
        for line in reassembler.take_reports() {
            info!(target: LOG_TARGET, "{line}");
        }
        report.roll(&conn, &mut reassembler);
        if events.is_empty() {
            continue;
        }
        for event in events {
            match event {
                // Never wait for the decoder: the datagrams (and loss checks)
                // must keep flowing. A decoder that can't keep up drops to
                // the next keyframe instead of falling ever further behind.
                Event::Frame(frame) => match frames.try_send(frame) {
                    Ok(()) => {
                        report.frames += 1;
                        let queued = frames.max_capacity() - frames.capacity();
                        if backlog.behind(queued, Instant::now()) {
                            info!(target: LOG_TARGET, "decodificador atrás há muito tempo; pulando até um keyframe");
                            reassembler.resync(Instant::now());
                            let _ = keyframe.send(());
                            break;
                        }
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        info!(target: LOG_TARGET, "fila do decodificador cheia; pulando até um keyframe");
                        reassembler.resync(Instant::now());
                        let _ = keyframe.send(());
                        break;
                    }
                    // Decoder gone: the session is ending.
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                },
                Event::Resend(request) => {
                    let _ = resend.send(request);
                }
                Event::Lost => {
                    debug!("quadro perdido apesar do FEC; pedindo keyframe");
                    let _ = keyframe.send(());
                }
            }
        }
        stats.lock().expect("lock").video = reassembler.counters;
    }
}

/// Logs how video arrived, once a second: at `info` when frames were lost,
/// otherwise at `debug`.
struct ArrivalReport {
    since: Instant,
    /// Frames handed to the decoder in this second.
    frames: u32,
    /// Datagrams read since this began.
    read: u64,
    /// QUIC's count of those it received, and `read`, a second ago.
    quic_before: u64,
    read_before: u64,
    counters_before: Counters,
}

impl ArrivalReport {
    fn new(conn: &Connection) -> Self {
        Self {
            since: Instant::now(),
            frames: 0,
            read: 0,
            quic_before: conn.stats().frame_rx.datagram,
            read_before: 0,
            counters_before: Counters::default(),
        }
    }

    fn roll(&mut self, conn: &Connection, reassembler: &mut Reassembler) {
        if self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let quic = conn.stats().frame_rx.datagram;
        let received = quic.saturating_sub(self.quic_before);
        let read = self.read - self.read_before;
        let arrival = reassembler.take_arrival();
        let counters = reassembler.counters;
        let lost = counters.lost - self.counters_before.lost;
        if let Some(path) = hext_net::path(conn) {
            reassembler.set_rtt(path.rtt);
        }
        let saved = counters.resent - self.counters_before.resent;
        let line = format!(
            "chegada: {} quadros, {received} datagramas ({} não lidos), maior intervalo num quadro {} ms, \
             maior transferência {} ms, até {} quadros passaram à frente | {lost} perdidos, \
             {saved} salvos por reenvio, {} recuperados, {} pacotes tardios",
            self.frames,
            received.saturating_sub(read),
            arrival.max_gap.as_millis(),
            arrival.max_transfer.as_millis(),
            arrival.max_overtaken,
            counters.recovered - self.counters_before.recovered,
            counters.late_packets - self.counters_before.late_packets,
        );
        if lost > 0 || saved > 0 {
            info!(target: LOG_TARGET, "{line}");
        } else {
            debug!(target: LOG_TARGET, "{line}");
        }
        self.since = Instant::now();
        self.frames = 0;
        self.quic_before = quic;
        self.read_before = self.read;
        self.counters_before = counters;
    }
}

/// The host leaving is a normal end of session, not an error, unless it
/// closed with an error code (e.g. its screen capture failed to start).
fn peer_closed_is_ok(conn: &Connection, e: hext_net::NetError) -> Result<()> {
    if !e.is_peer_closed() {
        return Err(e.into());
    }
    match conn.close_reason() {
        Some(hext_net::ConnectionError::ApplicationClosed(close))
            if close.error_code != hext_net::VarInt::from_u32(0) =>
        {
            Err(SessionError::Protocol(format!(
                "o host encerrou a sessão: {}",
                String::from_utf8_lossy(&close.reason)
            )))
        }
        _ => Ok(()),
    }
}

/// Sends `first` plus whatever else is queued, keeping only the latest of
/// consecutive mouse moves so a backlog never replays stale positions.
async fn send_input(
    stream: &mut FramedSend,
    queue: &mut mpsc::UnboundedReceiver<InputEvent>,
    first: InputEvent,
) -> hext_net::Result<()> {
    let mut pending = vec![first];
    while let Ok(event) = queue.try_recv() {
        if let (Some(InputEvent::MouseMove { .. }), InputEvent::MouseMove { .. }) =
            (pending.last(), event)
        {
            pending.pop();
        }
        pending.push(event);
    }
    for event in &pending {
        stream.send(event).await?;
    }
    Ok(())
}

fn decode_loop(
    frames: &mut mpsc::Receiver<VideoFrame>,
    keyframe: &mpsc::UnboundedSender<()>,
    stats: &Mutex<Stats>,
    preference: DecoderPreference,
    ready: oneshot::Sender<Result<()>>,
    mut on_frame: impl FnMut(DecodedFrame),
) {
    // Opened on this thread: platform decoders (COM) stay on their thread.
    let mut decoder = match hext_codec::open_decoder(preference) {
        Ok(decoder) => {
            let _ = ready.send(Ok(()));
            decoder
        }
        Err(e) => {
            let _ = ready.send(Err(e.into()));
            return;
        }
    };
    info!("decodificador: {}", decoder.name());
    // After an error, ignore frames until a keyframe resynchronizes the stream.
    let mut waiting_for_keyframe = false;
    while let Some(frame) = frames.blocking_recv() {
        if waiting_for_keyframe && !frame.keyframe {
            continue;
        }
        let started = Instant::now();
        match decoder.decode(&frame.data) {
            Ok(Some(yuv)) => {
                waiting_for_keyframe = false;
                {
                    let mut stats = stats.lock().expect("lock");
                    stats.width = yuv.width;
                    stats.height = yuv.height;
                    stats.record(
                        frame.data.len(),
                        Duration::from_micros(frame.capture_micros.into()),
                        Duration::from_micros(frame.encode_micros.into()),
                        started.elapsed(),
                        frame.hardware,
                        decoder.is_hardware(),
                    );
                }
                if started.elapsed() > SLOW_DECODE {
                    debug!(
                        target: LOG_TARGET,
                        "decode lento: quadro {} ({} kB, keyframe: {}) em {} ms",
                        frame.seq,
                        frame.data.len() / 1000,
                        frame.keyframe,
                        started.elapsed().as_millis()
                    );
                }
                on_frame(DecodedFrame {
                    seq: frame.seq,
                    display: frame.display,
                    yuv,
                });
            }
            Ok(None) => {}
            Err(e) => {
                warn!("decode do quadro {}: {e}; pedindo keyframe", frame.seq);
                waiting_for_keyframe = true;
                let _ = keyframe.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_hears_of_muting_only_when_it_has_sound_and_once() {
        let (wanted, wanted_rx) = watch::channel(true);
        let mut sound = Sound {
            packets: mpsc::channel(1).0,
            wanted: wanted_rx,
            host_sends: true,
        };
        let without = Displays::default();
        let with = Displays {
            audio: true,
            ..Displays::default()
        };
        assert_eq!(sound.tell(&with), None, "as asked in the Hello");
        wanted.send_replace(false);
        assert_eq!(sound.tell(&without), None, "an older host: nothing");
        assert_eq!(sound.tell(&with), Some(Control::Audio { on: false }));
        assert_eq!(sound.tell(&with), None, "once");
        wanted.send_replace(true);
        assert_eq!(sound.tell(&with), Some(Control::Audio { on: true }));
    }

    #[test]
    fn a_burst_for_the_decoder_is_not_falling_behind() {
        let mut backlog = Backlog::default();
        let t0 = Instant::now();
        assert!(!backlog.behind(DECODE_QUEUE - 1, t0));
        assert!(!backlog.behind(10, t0 + BEHIND_FOR));
        // It drained: the clock starts over.
        assert!(!backlog.behind(1, t0 + BEHIND_FOR));
        assert!(!backlog.behind(10, t0 + BEHIND_FOR * 2));
        assert!(backlog.behind(10, t0 + BEHIND_FOR * 3 + Duration::from_millis(1)));
    }

    #[test]
    fn a_burst_coming_in_and_draining_is_not_falling_behind() {
        let mut backlog = Backlog::default();
        let t0 = Instant::now();
        for queued in BEHIND_FRAMES + 1..=30 {
            assert!(!backlog.behind(queued, t0), "{queued} frames arriving");
        }
        for (i, queued) in (BEHIND_FRAMES + 1..30).rev().enumerate() {
            let now = t0 + BEHIND_FOR / 4 * (i as u32 + 1);
            assert!(!backlog.behind(queued, now), "{queued} frames left");
        }
    }

    #[test]
    fn a_queue_that_stops_draining_is_falling_behind() {
        let mut backlog = Backlog::default();
        let t0 = Instant::now();
        assert!(!backlog.behind(30, t0));
        assert!(!backlog.behind(10, t0 + BEHIND_FOR));
        assert!(!backlog.behind(11, t0 + BEHIND_FOR + Duration::from_millis(16)));
        assert!(!backlog.behind(10, t0 + BEHIND_FOR + Duration::from_millis(32)));
        assert!(backlog.behind(11, t0 + BEHIND_FOR * 2 + Duration::from_millis(1)));
    }

    #[test]
    fn falling_behind_starts_over_once_it_is_noticed() {
        let mut backlog = Backlog::default();
        let t0 = Instant::now();
        assert!(!backlog.behind(5, t0));
        let late = t0 + BEHIND_FOR + Duration::from_millis(1);
        assert!(backlog.behind(5, late));
        assert!(!backlog.behind(6, late + Duration::from_millis(1)));
    }

    #[test]
    fn a_long_burst_going_down_is_not_falling_behind() {
        let mut backlog = Backlog::default();
        let t0 = Instant::now();
        for (i, queued) in (4..DECODE_QUEUE).rev().enumerate() {
            let now = t0 + BEHIND_FOR / 4 * i as u32;
            assert!(!backlog.behind(queued, now), "{queued} frames left");
        }
    }
}
