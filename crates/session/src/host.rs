//! Host side of a session.

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
    pub access_code: String,
    /// One-time codes given to viewers the host's user accepted (through
    /// the server); each also opens one session.
    pub session_codes: crate::SessionCodes,
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
            if codes_match(&config.access_code, &hello.access_code)
                || config.session_codes.redeem(&hello.access_code)
            {
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
        return Err(e);
    }

    // Video goes out as datagrams with FEC (see `hext_net::video`).
    let video = conn.clone();
    let bitrate = config.encoder.bitrate_bps;
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
    let mut input = tokio::spawn(receive_input(conn.clone(), shared.clone()));

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

    shared.stop.store(true, Ordering::Relaxed);
    writer.abort();
    // Dropping the input task releases any keys still held. (A finished task
    // was already awaited by the loop and must not be polled again.)
    if !input.is_finished() {
        input.abort();
        let _ = (&mut input).await;
    }
    conn.close(0u32.into(), b"session ended");
    let _ = tokio::task::spawn_blocking(move || capture.join()).await;
    info!("desconectado: {}", client.device_name);
    result.map(|()| client)
}

fn capture_loop(
    config: &HostConfig,
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
