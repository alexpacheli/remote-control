//! Host side: records what the computer plays and hands out Opus packets.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use hext_proto::AudioPacket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::{Encoder, LOG_TARGET};

#[cfg(target_os = "macos")]
use crate::sck as platform;
#[cfg(windows)]
use crate::wasapi as platform;
#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::Chunks;
    use crate::AudioError;

    pub fn available() -> bool {
        false
    }

    pub struct Loopback;

    impl Loopback {
        pub fn open() -> Result<(Self, Chunks), AudioError> {
            Err(AudioError::Unsupported)
        }

        pub fn needs_reopen(&mut self) -> bool {
            false
        }
    }
}

/// Stereo samples at 48 kHz, as the system delivers them.
pub(crate) type Chunks = Receiver<Vec<f32>>;

/// Whether this computer can record what it plays: Windows, and macOS 13
/// or newer.
pub fn loopback_available() -> bool {
    platform::available()
}

/// A capture that failed to open is tried again after this long (a
/// computer with no sound device yet, or the sign-in screen).
const RETRY: Duration = Duration::from_secs(5);
/// How long the thread waits for sound before looking around.
const WAIT: Duration = Duration::from_millis(100);
/// How often the sound sent is logged (at debug).
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// The computer's sound, recorded on a thread of its own while it is on.
/// Dropping it stops the recording.
pub struct Capture {
    on: Sender<bool>,
}

impl Capture {
    /// A capture sending its packets to `packets`, off until
    /// [`Self::set_on`]; `None` where the system cannot record its sound.
    pub fn new(packets: mpsc::Sender<AudioPacket>) -> Option<Self> {
        if !loopback_available() {
            return None;
        }
        let (on, commands) = channel();
        std::thread::Builder::new()
            .name("hext-audio-in".into())
            .spawn(move || run(&commands, &packets))
            .inspect_err(|e| warn!("som: a thread não abriu: {e}"))
            .ok()?;
        Some(Self { on })
    }

    /// Records (the viewer hears) or stops.
    pub fn set_on(&self, on: bool) {
        let _ = self.on.send(on);
    }
}

fn run(commands: &Receiver<bool>, packets: &mpsc::Sender<AudioPacket>) {
    let mut encoder = match Encoder::new() {
        Ok(encoder) => encoder,
        Err(e) => {
            warn!("som: {e}");
            return;
        }
    };
    let mut on = false;
    let mut source: Option<(platform::Loopback, Chunks)> = None;
    let mut failed_at: Option<Instant> = None;
    let mut warned = false;
    let mut out = Vec::new();
    let mut report = Instant::now();
    loop {
        // The latest word of the viewer.
        loop {
            match commands.try_recv() {
                Ok(wanted) => on = wanted,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if !on {
            if source.take().is_some() {
                info!("som: captura parada");
                encoder.reset();
            }
            match commands.recv() {
                Ok(wanted) => on = wanted,
                Err(_) => return,
            }
            continue;
        }
        let now = Instant::now();
        if source.is_none() && failed_at.is_none_or(|t| now.duration_since(t) >= RETRY) {
            match platform::Loopback::open() {
                Ok(opened) => {
                    info!("som: capturando o que o computador toca");
                    source = Some(opened);
                    failed_at = None;
                    warned = false;
                }
                Err(e) => {
                    // Once per failure: the session goes on without sound.
                    if !std::mem::replace(&mut warned, true) {
                        warn!("som: não foi possível capturar ({e}); a sessão segue sem som");
                    } else {
                        debug!("som: {e}");
                    }
                    failed_at = Some(now);
                }
            }
        }
        let Some((loopback, chunks)) = &mut source else {
            // Waits for the next try, or for a word from the viewer.
            match commands.recv_timeout(RETRY) {
                Ok(wanted) => on = wanted,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            continue;
        };
        match chunks.recv_timeout(WAIT) {
            Ok(chunk) => {
                encoder.push(&chunk, &mut out);
                for packet in out.drain(..) {
                    match packets.try_send(packet) {
                        Ok(()) => {}
                        // The network is behind: this one is too late anyway.
                        Err(mpsc::error::TrySendError::Full(_)) => {}
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                info!("som: a captura parou; abrindo de novo");
                source = None;
                encoder.reset();
                continue;
            }
        }
        if loopback.needs_reopen() {
            info!("som: o dispositivo de som mudou; capturando o novo");
            source = None;
            encoder.reset();
        }
        if report.elapsed() >= REPORT_EVERY {
            report = Instant::now();
            debug!(
                target: LOG_TARGET,
                "som: {} quadros enviados, {} em silêncio",
                encoder.sent,
                encoder.silent
            );
        }
    }
}
