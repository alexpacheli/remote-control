//! Host side of a session.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hext_codec::{EncoderSettings, H264Encoder};
use hext_input::Area;
use hext_net::{Connection, NetError};
use hext_proto::{Control, Hello, InputEvent, PeerInfo, RejectReason, StreamKind, VideoFrame};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use crate::{Result, SessionError, codes_match};

pub struct HostConfig {
    pub peer: PeerInfo,
    pub access_code: String,
    /// Display index (0 = primary).
    pub display: usize,
    /// Stream a synthetic pattern instead of the real screen.
    pub test_pattern: bool,
    pub encoder: EncoderSettings,
}

/// State shared between the async session and the capture thread.
struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    area: Mutex<Area>,
}

/// Runs one session on an accepted connection until the client leaves.
///
/// Returns the client's identity once the session ends normally.
pub async fn serve(conn: Connection, config: Arc<HostConfig>) -> Result<PeerInfo> {
    let (mut control, hello) =
        hext_net::host_handshake(&conn, config.peer.clone(), |hello: &Hello| {
            if codes_match(&config.access_code, &hello.access_code) {
                Ok(())
            } else {
                Err(RejectReason::WrongCode)
            }
        })
        .await?;
    let client = hello.peer;
    info!(
        "conectado: {} ({:?}, v{}) de {}",
        client.device_name,
        client.os,
        client.app_version,
        conn.remote_address()
    );

    let shared = Arc::new(Shared {
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(false),
        area: Mutex::new(Area::default()),
    });

    // Capture and encoding run on their own thread: both are blocking, and
    // capture objects (COM on Windows) must stay on the thread that made them.
    // A one-slot channel gives backpressure: while the network is busy the
    // thread waits, and the next capture picks up the latest screen.
    let (frames_tx, mut frames_rx) = mpsc::channel::<VideoFrame>(1);
    let (started_tx, started_rx) = oneshot::channel();
    let capture = {
        let (shared, config) = (shared.clone(), config.clone());
        std::thread::Builder::new()
            .name("hext-capture".into())
            .spawn(move || capture_loop(&config, &shared, &frames_tx, started_tx))
            .map_err(|e| SessionError::Protocol(format!("capture thread: {e}")))?
    };
    let started = started_rx
        .await
        .map_err(|_| SessionError::Protocol("capture thread died".into()))?;
    if let Err(e) = started {
        conn.close(1u32.into(), b"capture unavailable");
        return Err(e.into());
    }

    let mut video = hext_net::open_uni(&conn, StreamKind::Video).await?;
    let writer = tokio::spawn(async move {
        while let Some(frame) = frames_rx.recv().await {
            video.send(&frame).await?;
        }
        Ok::<_, NetError>(())
    });
    let input = tokio::spawn(receive_input(conn.clone(), shared.clone()));

    let result = loop {
        tokio::select! {
            msg = control.recv() => match msg {
                Ok(Control::RequestKeyframe) => shared.keyframe.store(true, Ordering::Relaxed),
                Ok(Control::Bye) => {
                    // Closing here is what lets the client's `bye()` return.
                    conn.close(0u32.into(), b"bye");
                    break Ok(());
                }
                Ok(other) => debug!("mensagem inesperada: {other:?}"),
                // The client went away without a Bye.
                Err(_) => break Ok(()),
            },
            _ = conn.closed() => break Ok(()),
        }
    };

    shared.stop.store(true, Ordering::Relaxed);
    writer.abort();
    // Dropping the input task releases any keys still held.
    input.abort();
    let _ = input.await;
    conn.close(0u32.into(), b"session ended");
    let _ = tokio::task::spawn_blocking(move || capture.join()).await;
    info!("desconectado: {}", client.device_name);
    result.map(|()| client)
}

fn capture_loop(
    config: &HostConfig,
    shared: &Shared,
    frames: &mpsc::Sender<VideoFrame>,
    started: oneshot::Sender<std::result::Result<(), hext_capture::CaptureError>>,
) {
    let mut capturer = match hext_capture::open(config.display, config.test_pattern) {
        Ok(capturer) => {
            let _ = started.send(Ok(()));
            capturer
        }
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let mut encoder = H264Encoder::new(config.encoder);
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
        let changed = match capturer.next_frame(Duration::from_millis(100)) {
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
            continue;
        }
        let Some(frame) = capturer.frame() else {
            continue;
        };

        let started = Instant::now();
        let encoded = match encoder.encode(frame.width, frame.height, &frame.data) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => {
                skipped = true;
                last_encode = Some(tick);
                continue;
            }
            Err(e) => {
                warn!("encode: {e}");
                continue;
            }
        };
        let encode_time = started.elapsed();
        last_encode = Some(tick);
        skipped = false;

        let msg = VideoFrame {
            seq,
            width: frame.width,
            height: frame.height,
            keyframe: encoded.keyframe,
            capture_micros: micros(capturer.copy_time()),
            encode_micros: micros(encode_time),
            data: encoded.data,
        };
        seq += 1;
        // Fails only when the session is over.
        if frames.blocking_send(msg).is_err() {
            break;
        }
    }
}

/// Injects the client's input until the stream or connection ends.
async fn receive_input(conn: Connection, shared: Arc<Shared>) -> Result<()> {
    let (kind, mut stream) = hext_net::accept_uni(&conn).await?;
    if kind != StreamKind::Input {
        return Err(SessionError::Protocol(format!(
            "unexpected {kind:?} stream"
        )));
    }
    let mut injector = ReleaseOnDrop(hext_input::injector());
    loop {
        let event: InputEvent = stream.recv().await?;
        let area = *shared.area.lock().expect("lock");
        injector.0.inject(&event, area);
    }
}

/// Never leave keys stuck down on the host when a session ends.
struct ReleaseOnDrop(Box<dyn hext_input::Injector>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release_all();
    }
}

fn micros(d: Duration) -> u32 {
    d.as_micros().min(u32::MAX as u128) as u32
}
