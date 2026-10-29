//! Host side of a session.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_codec::EncoderSettings;
use hext_input::Area;
use hext_net::video::VideoSender;
use hext_net::{Connection, NetError};
use hext_proto::{Control, Hello, InputEvent, PeerInfo, RejectReason, StreamKind, VideoFrame};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{Result, SessionError, codes_match};

/// How often a still screen repeats the number of its last frame.
const IDLE_NOTICE: Duration = Duration::from_millis(250);

pub struct HostConfig {
    pub peer: PeerInfo,
    /// The code shown to the host's user, good for any number of sessions;
    /// `None`: sessions only through the server (accepted one by one).
    pub access_code: Option<String>,
    /// One-time codes given to viewers the host's user accepted (through
    /// the server); each also opens one session.
    pub session_codes: crate::SessionCodes,
    /// Where the screen comes from and the input goes.
    pub desktop: Arc<dyn Desktop>,
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a session's screen comes from and its input goes: this process
/// ([`LocalDesktop`]), or another one (the agent of a Windows service).
pub trait Desktop: Send + Sync {
    /// Starts streaming the screen, encoded, into `frames`.
    fn open(&self, frames: mpsc::Sender<VideoFrame>) -> BoxFuture<'_, Result<Box<dyn Screen>>>;

    /// The video's bit rate, for pacing it out.
    fn bitrate_bps(&self) -> u32;
}

/// A screen being streamed.
pub trait Screen: Send {
    /// The viewer lost video: the next frame must be a keyframe.
    fn request_keyframe(&self);
    /// Replays the viewer's input.
    fn inject(&mut self, event: &InputEvent);
    /// Stops streaming, releasing any key or button still held.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// The screen of this process's desktop: capture and encoding on a thread
/// of their own, input injected here.
pub struct LocalDesktop {
    /// Display index (0 = primary).
    pub display: usize,
    /// Stream a synthetic pattern instead of the real screen.
    pub test_pattern: bool,
    pub encoder: EncoderSettings,
}

impl Desktop for LocalDesktop {
    fn open(&self, frames: mpsc::Sender<VideoFrame>) -> BoxFuture<'_, Result<Box<dyn Screen>>> {
        let settings = CaptureSettings {
            display: self.display,
            test_pattern: self.test_pattern,
            encoder: self.encoder,
        };
        Box::pin(async move {
            let shared = Arc::new(Shared {
                stop: AtomicBool::new(false),
                keyframe: AtomicBool::new(false),
                area: Mutex::new(Area::default()),
            });
            // Capture and encoding run on their own thread: both are
            // blocking, and capture objects (COM on Windows) must stay on
            // the thread that made them. A one-slot channel gives
            // backpressure: while the network is busy the thread waits, and
            // the next capture picks up the latest screen.
            let (started_tx, started_rx) = oneshot::channel();
            let thread = {
                let shared = shared.clone();
                std::thread::Builder::new()
                    .name("hext-capture".into())
                    .spawn(move || capture_loop(&settings, &shared, &frames, started_tx))
                    .map_err(|e| SessionError::Protocol(format!("capture thread: {e}")))?
            };
            let started = started_rx
                .await
                .map_err(|_| SessionError::Protocol("capture thread died".into()))?;
            started?;
            let (input, input_thread) = spawn_input(shared.clone())?;
            Ok(Box::new(LocalScreen {
                shared,
                thread: Some(thread),
                input: Some(input),
                input_thread: Some(input_thread),
                input_failed: false,
            }) as Box<dyn Screen>)
        })
    }

    fn bitrate_bps(&self) -> u32 {
        self.encoder.bitrate_bps
    }
}

#[derive(Clone, Copy)]
struct CaptureSettings {
    display: usize,
    test_pattern: bool,
    encoder: EncoderSettings,
}

struct LocalScreen {
    shared: Arc<Shared>,
    /// Taken by `close`, which waits for it.
    thread: Option<std::thread::JoinHandle<()>>,
    /// The input thread; it releases held keys when this closes.
    input: Option<std::sync::mpsc::Sender<InputEvent>>,
    /// Taken by `close`, which waits for it (keys released).
    input_thread: Option<std::thread::JoinHandle<()>>,
    /// The input thread is gone (logged once).
    input_failed: bool,
}

/// Also when the session is dropped without `close` (a task cancelled):
/// never leave keys stuck down on the host (the input thread releases them
/// when its channel closes), nor the capture running.
impl Drop for LocalScreen {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// Injects input on a thread of its own: on Windows it follows the input
/// desktop (the sign-in screen and UAC prompts, for a SYSTEM agent), which
/// only works per thread.
fn spawn_input(
    shared: Arc<Shared>,
) -> Result<(
    std::sync::mpsc::Sender<InputEvent>,
    std::thread::JoinHandle<()>,
)> {
    /// How often to check whether the input desktop changed (not on every
    /// mouse move).
    const FOLLOW_EVERY: Duration = Duration::from_millis(200);
    let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
    let thread = std::thread::Builder::new()
        .name("hext-input".into())
        .spawn(move || {
            let mut injector = hext_input::injector();
            let mut followed: Option<Instant> = None;
            while let Ok(event) = rx.recv() {
                if followed.is_none_or(|t| t.elapsed() >= FOLLOW_EVERY) {
                    hext_capture::follow_input_desktop();
                    followed = Some(Instant::now());
                }
                let area = *shared.area.lock().expect("lock");
                injector.inject(&event, area);
            }
            injector.release_all();
        })
        .map_err(|e| SessionError::Protocol(format!("input thread: {e}")))?;
    Ok((tx, thread))
}

impl Screen for LocalScreen {
    fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Relaxed);
    }

    fn inject(&mut self, event: &InputEvent) {
        if *event == InputEvent::SecureAttention {
            // Takes the Windows service (this process is not it).
            debug!("Ctrl+Alt+Del: só o serviço pode enviar");
            return;
        }
        let sent = self
            .input
            .as_ref()
            .is_some_and(|input| input.send(*event).is_ok());
        if !sent && !std::mem::replace(&mut self.input_failed, true) {
            warn!("input: a thread de injeção parou; mouse e teclado não chegam mais");
        }
    }

    fn close(mut self: Box<Self>) -> BoxFuture<'static, ()> {
        // Releases the keys and stops the capture (`Drop`), then waits for
        // the capture to let go of the screen and the encoder.
        let thread = self.thread.take();
        let input_thread = self.input_thread.take();
        // Closing its channel makes the input thread release held keys.
        self.input = None;
        drop(self);
        Box::pin(async move {
            let _ = tokio::task::spawn_blocking(move || {
                if let Some(input) = input_thread {
                    let _ = input.join();
                }
                if let Some(thread) = thread {
                    let _ = thread.join();
                }
            })
            .await;
        })
    }
}

/// State shared between a [`LocalScreen`] and its capture thread.
struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    area: Mutex<Area>,
}

/// Runs one session on an accepted connection until the client leaves.
///
/// Returns the client's identity once the session ends normally.
pub async fn serve(conn: Connection, config: Arc<HostConfig>) -> Result<PeerInfo> {
    // The viewer proved it holds this key; a session code is only good from
    // the viewer it was issued to.
    let viewer = hext_net::remote_key(&conn);
    let (mut control, hello) =
        hext_net::host_handshake(&conn, config.peer.clone(), |hello: &Hello| {
            let code = config.access_code.as_deref();
            if code.is_some_and(|code| codes_match(code, &hello.access_code))
                || config.session_codes.redeem(&hello.access_code, &viewer)
            {
                Ok(())
            } else {
                Err(RejectReason::WrongCode)
            }
        })
        .await?;
    let client = hello.peer;
    // (Not the path: right after the handshake it is usually the relay,
    // and a direct one follows; the viewer's title shows it live.)
    info!(
        "conectado: {} ({:?}, v{})",
        client.device_name, client.os, client.app_version,
    );

    // One slot: backpressure from the network back to the capture.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(1);
    let screen = match config.desktop.open(frames_tx).await {
        Ok(screen) => Arc::new(Mutex::new(Some(screen))),
        Err(e) => {
            conn.close(1u32.into(), b"capture unavailable");
            return Err(e);
        }
    };

    // Video goes out as datagrams with FEC (see `hext_net::video`).
    let video = conn.clone();
    let bitrate = config.desktop.bitrate_bps();
    let mut writer = tokio::spawn(async move {
        let mut sender = VideoSender::new(bitrate);
        loop {
            match tokio::time::timeout(IDLE_NOTICE, frames_rx.recv()).await {
                Ok(Some(frame)) => sender.send(&video, &frame).await?,
                Ok(None) => return Ok::<_, NetError>(()),
                // A still screen: let the viewer check it has the last frame.
                Err(_) => sender.send_idle(&video).await?,
            }
        }
    });
    let mut input = tokio::spawn(receive_input(conn.clone(), screen.clone()));

    let result = loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Ok(Control::RequestKeyframe) => {
                    if let Some(screen) = screen.lock().expect("lock").as_ref() {
                        screen.request_keyframe();
                    }
                }
                Ok(Control::Bye) => {
                    // Closing here is what lets the client's `bye()` return.
                    conn.close(0u32.into(), b"bye");
                    break Ok(());
                }
                Ok(other) => debug!("mensagem inesperada: {other:?}"),
                // A peer that left is a clean end; anything else is an error.
                Err(e) if e.is_peer_closed() => break Ok(()),
                Err(e) => break Err(e.into()),
            },
            _ = conn.closed() => break Ok(()),
            // Without video or input the session is useless: end it.
            done = &mut writer => break task_end("vídeo", done.map(|r| r.map_err(SessionError::from))),
            done = &mut input => break task_end("input", done),
        }
    };

    writer.abort();
    // (A finished task was already awaited by the loop and must not be
    // polled again.)
    if !input.is_finished() {
        input.abort();
        let _ = (&mut input).await;
    }
    conn.close(0u32.into(), b"session ended");
    // Releases any keys still held and stops the capture.
    let screen = screen.lock().expect("lock").take();
    if let Some(screen) = screen {
        screen.close().await;
    }
    info!("desconectado: {}", client.device_name);
    result.map(|()| client)
}

fn capture_loop(
    config: &CaptureSettings,
    shared: &Shared,
    frames: &mpsc::Sender<VideoFrame>,
    started: oneshot::Sender<Result<()>>,
) {
    let opened = hext_capture::open(
        config.display,
        config.test_pattern,
        config.encoder.max_fps.round() as u32,
    )
    .map_err(SessionError::from)
    .and_then(|capturer| Ok((capturer, hext_codec::open_encoder(config.encoder)?)));
    let (mut capturer, mut encoder) = match opened {
        Ok(opened) => {
            let _ = started.send(Ok(()));
            opened
        }
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    info!("codificador: {}", encoder.name());
    let mut hardware = encoder.is_hardware();
    let min_interval = Duration::from_secs_f32(1.0 / config.encoder.max_fps.max(1.0));
    let mut last_encode: Option<Instant> = None;
    let mut seq = 0u64;
    // Rate control skipped the last image; it must still be sent.
    let mut skipped = false;

    while !shared.stop.load(Ordering::Relaxed) {
        // Respect the frame-rate cap; changes accumulate in the meantime.
        if let Some(last) = last_encode {
            let since = last.elapsed();
            if since < min_interval {
                std::thread::sleep(min_interval - since);
            }
        }

        // The frame-rate cap counts from the start of each frame's work.
        let tick = Instant::now();
        // Owed work (a skipped image, a keyframe) must not wait for a change.
        let owed = skipped || shared.keyframe.load(Ordering::Relaxed);
        let timeout = if owed {
            Duration::ZERO
        } else {
            Duration::from_millis(100)
        };
        let changed = match capturer.next_frame(timeout) {
            Ok(changed) => changed,
            Err(e) => {
                warn!("captura: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let rect = capturer.rect();
        *shared.area.lock().expect("lock") = Area {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };

        let forced = shared.keyframe.swap(false, Ordering::Relaxed);
        if forced {
            encoder.force_keyframe();
        }
        // A still screen costs nothing, unless the viewer needs a keyframe
        // or the latest image was skipped.
        if !changed && !forced && !skipped {
            if encoder.pending() {
                // The last images are still inside the encoder: collect them
                // without feeding it new input.
                let started = Instant::now();
                match encoder.drain() {
                    Ok(Some(encoded)) => {
                        let Some(frame) = capturer.frame() else {
                            continue;
                        };
                        let sent = send_frame(
                            frames,
                            &mut seq,
                            frame,
                            encoded,
                            capturer.copy_time(),
                            started.elapsed(),
                            hardware,
                        );
                        if !sent {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        warn!("encode: {e}");
                        // Encode the latest image again (with the fallback encoder).
                        skipped = true;
                    }
                }
            }
            continue;
        }
        let Some(frame) = capturer.frame() else {
            // Nothing captured yet: keep a pending keyframe request alive.
            if forced {
                shared.keyframe.store(true, Ordering::Relaxed);
            }
            continue;
        };

        let started = Instant::now();
        let encoded = match encoder.encode(&image(frame)) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => {
                skipped = true;
                last_encode = Some(tick);
                continue;
            }
            Err(e) => {
                warn!("encode: {e}");
                // Don't drop a keyframe request: retry on the next loop
                // (owed = true keeps the timeout at zero).
                if forced {
                    shared.keyframe.store(true, Ordering::Relaxed);
                }
                continue;
            }
        };
        let encode_time = started.elapsed();
        last_encode = Some(tick);
        skipped = false;
        if hardware != encoder.is_hardware() {
            hardware = encoder.is_hardware();
            warn!("codificador agora: {}", encoder.name());
        }

        let sent = send_frame(
            frames,
            &mut seq,
            frame,
            encoded,
            capturer.copy_time(),
            encode_time,
            hardware,
        );
        if !sent {
            break;
        }
    }
}

/// The captured frame as encoder input.
fn image(frame: &hext_capture::Frame) -> hext_codec::Image<'_> {
    hext_codec::Image {
        width: frame.width,
        height: frame.height,
        format: match frame.format {
            hext_capture::PixelFormat::Bgra => hext_codec::PixelFormat::Bgra,
            hext_capture::PixelFormat::Nv12 => hext_codec::PixelFormat::Nv12,
        },
        pixels: gpu_texture(frame).unwrap_or(hext_codec::Pixels::Memory(&frame.data)),
    }
}

/// The frame's GPU texture, where the capture left the image on the GPU.
#[cfg(windows)]
fn gpu_texture(frame: &hext_capture::Frame) -> Option<hext_codec::Pixels<'_>> {
    frame.texture.as_ref().map(hext_codec::Pixels::D3d11)
}

#[cfg(not(windows))]
fn gpu_texture(_frame: &hext_capture::Frame) -> Option<hext_codec::Pixels<'_>> {
    None
}

/// Hands an encoded frame to the network task; `false` once the session is over.
fn send_frame(
    frames: &mpsc::Sender<VideoFrame>,
    seq: &mut u64,
    frame: &hext_capture::Frame,
    encoded: hext_codec::Encoded,
    capture_time: Duration,
    encode_time: Duration,
    hardware: bool,
) -> bool {
    let msg = VideoFrame {
        seq: *seq,
        width: frame.width,
        height: frame.height,
        keyframe: encoded.keyframe,
        capture_micros: micros(capture_time),
        encode_micros: micros(encode_time),
        hardware,
        data: encoded.data,
    };
    *seq += 1;
    frames.blocking_send(msg).is_ok()
}

/// How a video or input task ending affects the session: a peer that left
/// is a normal end, anything else is an error.
fn task_end(
    what: &str,
    done: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match done {
        Ok(Ok(())) => Ok(()),
        Ok(Err(SessionError::Net(e))) if e.is_peer_closed() => Ok(()),
        Ok(Err(e)) => Err(SessionError::Protocol(format!("{what}: {e}"))),
        Err(e) => Err(SessionError::Protocol(format!("{what}: {e}"))),
    }
}

type SharedScreen = Arc<Mutex<Option<Box<dyn Screen>>>>;

/// Injects the client's input until the stream or connection ends.
async fn receive_input(conn: Connection, screen: SharedScreen) -> Result<()> {
    let (kind, mut stream) = hext_net::accept_uni(&conn).await?;
    if kind != StreamKind::Input {
        return Err(SessionError::Protocol(format!(
            "unexpected {kind:?} stream"
        )));
    }
    loop {
        let event: InputEvent = stream.recv().await?;
        if let Some(screen) = screen.lock().expect("lock").as_mut() {
            screen.inject(&event);
        }
    }
}

fn micros(d: Duration) -> u32 {
    d.as_micros().min(u32::MAX as u128) as u32
}
