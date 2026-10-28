//! Client (viewer) side of a session.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_codec::{DecoderPreference, YuvFrame};
use hext_net::{Connection, ControlStream, FramedRecv, FramedSend};
use hext_proto::{Control, Hello, InputEvent, PeerInfo, StreamKind, VideoFrame};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::{Result, SessionError, Stats};

/// A decoded picture ready to be shown.
pub struct DecodedFrame {
    pub seq: u64,
    pub yuv: YuvFrame,
}

/// A running client session.
pub struct ClientHandle {
    pub host: PeerInfo,
    pub stats: Arc<Mutex<Stats>>,
    conn: Connection,
    input: mpsc::UnboundedSender<InputEvent>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl ClientHandle {
    /// Queues an input event for the host. Dropped silently once the session is over.
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    /// Current round-trip time estimate of the connection.
    pub fn rtt(&self) -> Duration {
        self.conn.rtt()
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
    decoder: DecoderPreference,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> Result<ClientHandle> {
    let (control, host) = hext_net::client_handshake(&conn, hello).await?;
    let input_stream = hext_net::open_uni(&conn, StreamKind::Input).await?;
    let (kind, video) = hext_net::accept_uni(&conn).await?;
    if kind != StreamKind::Video {
        return Err(SessionError::Protocol(format!(
            "unexpected {kind:?} stream"
        )));
    }

    let stats = Arc::new(Mutex::new(Stats::default()));
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        conn.clone(),
        control,
        video,
        input_stream,
        input_rx,
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
        stats,
        conn,
        input: input_tx,
        shutdown: Some(shutdown_tx),
        task,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run(
    conn: Connection,
    mut control: ControlStream,
    mut video: FramedRecv,
    mut input_stream: FramedSend,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut shutdown: oneshot::Receiver<()>,
    stats: Arc<Mutex<Stats>>,
    decoder: DecoderPreference,
    ready: oneshot::Sender<Result<()>>,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> Result<()> {
    // Decoding is CPU-bound, so it gets its own thread. The short queue makes
    // a slow decoder stop the reads, and QUIC flow control then slows the
    // host down, instead of a backlog (and delay) building up here.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(2);
    let (keyframe_tx, mut keyframe_rx) = mpsc::unbounded_channel::<()>();
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
    let mut receiver = tokio::spawn(async move {
        loop {
            let frame: VideoFrame = video.recv().await?;
            if frames_tx.send(frame).await.is_err() {
                return Ok(()); // decoder gone: the session is ending
            }
        }
    });

    let mut say_bye = false;
    let result = loop {
        tokio::select! {
            done = &mut receiver => break match done {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => peer_closed_is_ok(e),
                Err(e) => Err(SessionError::Protocol(format!("video: {e}"))),
            },
            Some(event) = input_rx.recv() => {
                if let Err(e) = send_input(&mut input_stream, &mut input_rx, event).await {
                    break peer_closed_is_ok(e);
                }
            }
            Some(()) = keyframe_rx.recv() => {
                if let Err(e) = control.send(&Control::RequestKeyframe).await {
                    break peer_closed_is_ok(e);
                }
            }
            msg = control.recv() => match msg {
                Ok(Control::Bye) => break Ok(()),
                Ok(other) => debug!("mensagem inesperada: {other:?}"),
                Err(e) => break peer_closed_is_ok(e),
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
    conn.close(0u32.into(), b"fim");
    // Stopping the receiver drops the queue's sender, which ends the decoder.
    receiver.abort();
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    result
}

/// The host leaving is a normal end of session, not an error.
fn peer_closed_is_ok(e: hext_net::NetError) -> Result<()> {
    if e.is_peer_closed() {
        Ok(())
    } else {
        Err(e.into())
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
                on_frame(DecodedFrame {
                    seq: frame.seq,
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
