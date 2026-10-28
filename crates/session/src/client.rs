//! Client (viewer) side of a session.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_codec::{DecoderPreference, YuvFrame};
use hext_net::video::{Event, Reassembler};
use hext_net::{Connection, ControlStream, FramedSend, NetError};
use hext_proto::{Control, Datagram, Hello, InputEvent, PeerInfo, StreamKind, VideoFrame};
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

    let stats = Arc::new(Mutex::new(Stats::default()));
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = oneshot::channel();
    let task = tokio::spawn(run(
        conn.clone(),
        control,
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
    mut input_stream: FramedSend,
    mut input_rx: mpsc::UnboundedReceiver<InputEvent>,
    mut shutdown: oneshot::Receiver<()>,
    stats: Arc<Mutex<Stats>>,
    decoder: DecoderPreference,
    ready: oneshot::Sender<Result<()>>,
    on_frame: impl FnMut(DecodedFrame) + Send + 'static,
) -> Result<()> {
    // Decoding is CPU-bound, so it gets its own thread, behind a short queue.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(2);
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
    let mut receiver = tokio::spawn(receive_video(video_conn, frames_tx, loss_tx, video_stats));

    let mut say_bye = false;
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
            msg = control.recv() => match msg {
                Ok(Control::Bye) => break Ok(()),
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
    conn.close(0u32.into(), b"fim");
    // Stopping the receiver drops the queue's sender, which ends the decoder.
    receiver.abort();
    let _ = tokio::task::spawn_blocking(move || decoder.join()).await;
    result
}

/// Rebuilds frames from the host's video datagrams and queues them for the
/// decoder; a frame lost despite FEC asks the host for a keyframe.
async fn receive_video(
    conn: Connection,
    frames: mpsc::Sender<VideoFrame>,
    keyframe: mpsc::UnboundedSender<()>,
    stats: Arc<Mutex<Stats>>,
) -> hext_net::Result<()> {
    let mut reassembler = Reassembler::new();
    // Also notices a lost frame when nothing newer arrives (a still screen).
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let events = tokio::select! {
            datagram = hext_net::recv_datagram(&conn) => match datagram {
                Ok(Datagram::Video(packet)) => reassembler.push(packet, Instant::now()),
                Ok(Datagram::VideoIdle { last_frame }) => {
                    reassembler.idle(last_frame, Instant::now())
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
        if events.is_empty() {
            continue;
        }
        for event in events {
            match event {
                // Never wait for the decoder: the datagrams (and loss checks)
                // must keep flowing. A decoder that can't keep up drops to
                // the next keyframe instead of falling ever further behind.
                Event::Frame(frame) => match frames.try_send(frame) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        debug!("decodificador atrasado; pulando até um keyframe");
                        reassembler.resync(Instant::now());
                        let _ = keyframe.send(());
                        break;
                    }
                    // Decoder gone: the session is ending.
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                },
                Event::Lost => {
                    debug!("quadro perdido apesar do FEC; pedindo keyframe");
                    let _ = keyframe.send(());
                }
            }
        }
        stats.lock().expect("lock").video = reassembler.counters;
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
