//! The viewer's speaker: a thread that decodes what arrives and an audio
//! device stream that drains it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hext_proto::AudioPacket;
use tracing::{debug, info, warn};

use crate::dsp::{Resampler, from_stereo};
use crate::playout::{Depacketizer, Playout};
use crate::{LOG_TARGET, SAMPLE_RATE};

/// Packets waiting for the decoding thread: far more than it ever lets wait.
const QUEUE: usize = 64;
/// With nothing arriving for this long the device is let go (the host went
/// quiet, or the window is muted); the next sound opens it again.
const IDLE_CLOSE: Duration = Duration::from_secs(5);
/// A device that failed is tried again after this long.
const RETRY: Duration = Duration::from_secs(3);
/// How often the thread looks up when nothing arrives.
const TICK: Duration = Duration::from_millis(500);

/// Plays a session's sound. Dropping it stops the thread.
pub struct Player {
    packets: SyncSender<AudioPacket>,
    muted: Arc<AtomicBool>,
}

impl Player {
    pub fn new() -> Self {
        let (packets, rx) = sync_channel(QUEUE);
        let muted = Arc::new(AtomicBool::new(false));
        let thread_muted = muted.clone();
        let spawned = std::thread::Builder::new()
            .name("hext-audio-out".into())
            .spawn(move || run(&rx, &thread_muted));
        if let Err(e) = spawned {
            warn!("som: a thread não abriu: {e}");
        }
        Self { packets, muted }
    }

    /// Hands a packet to the thread; never waits (a full queue drops it).
    pub fn push(&self, packet: AudioPacket) {
        if let Err(TrySendError::Full(_)) = self.packets.try_send(packet) {
            debug!(target: LOG_TARGET, "fila do som cheia");
        }
    }

    /// Where the packets go, for a task forwarding them.
    pub fn sender(&self) -> SyncSender<AudioPacket> {
        self.packets.clone()
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

/// An open audio device and the queue it plays from.
struct Output {
    _stream: cpal::Stream,
    queue: Arc<Mutex<Playout>>,
    /// The stream reported an error (device unplugged...).
    broken: Arc<AtomicBool>,
    rate: u32,
    channels: usize,
    resampler: Resampler,
}

impl Output {
    fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("nenhum dispositivo de saída de som")?;
        let config = device.default_output_config().map_err(|e| e.to_string())?;
        let rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let queue = Arc::new(Mutex::new(Playout::new(rate, channels)));
        let broken = Arc::new(AtomicBool::new(false));
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, &queue, &broken),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, &queue, &broken),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, &queue, &broken),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, &queue, &broken),
            other => return Err(format!("formato de som sem suporte: {other:?}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        let name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "?".into());
        info!("som: tocando em \"{name}\" ({rate} Hz, {channels} canais)");
        Ok(Self {
            _stream: stream,
            queue,
            broken,
            rate,
            channels,
            resampler: Resampler::new(SAMPLE_RATE, rate, 2),
        })
    }

    /// Queues decoded stereo at 48 kHz, in the device's rate and channels.
    fn play(&mut self, stereo: &[f32], scratch: &mut Vec<f32>, mapped: &mut Vec<f32>) {
        scratch.clear();
        self.resampler.process(stereo, scratch);
        mapped.clear();
        from_stereo(scratch, self.channels, mapped);
        self.queue.lock().expect("lock").push(mapped);
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    queue: &Arc<Mutex<Playout>>,
    broken: &Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let queue = queue.clone();
    let failed = broken.clone();
    let mut buffer: Vec<f32> = Vec::new();
    device
        .build_output_stream::<T, _, _>(
            config.config(),
            move |out: &mut [T], _| {
                buffer.resize(out.len(), 0.0);
                queue.lock().expect("lock").pull(&mut buffer);
                for (o, s) in out.iter_mut().zip(&buffer) {
                    *o = T::from_sample(*s);
                }
            },
            move |e| {
                debug!("som: {e}");
                failed.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| e.to_string())
}

/// The thread: decodes what arrives and keeps the device open while it does.
fn run(packets: &Receiver<AudioPacket>, muted: &AtomicBool) {
    let mut depacketizer = match Depacketizer::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("som: {e}");
            return;
        }
    };
    let mut output: Option<Output> = None;
    let mut failed_at: Option<Instant> = None;
    let mut warned = false;
    let mut last = Instant::now();
    let (mut pcm, mut scratch, mut mapped) = (Vec::new(), Vec::new(), Vec::new());
    let mut report = Instant::now();
    loop {
        let packet = match packets.recv_timeout(TICK) {
            Ok(packet) => Some(packet),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let now = Instant::now();
        if output
            .as_ref()
            .is_some_and(|o| o.broken.load(Ordering::Relaxed))
        {
            info!("som: o dispositivo de saída parou; abrindo de novo");
            output = None;
            failed_at = Some(now);
        }
        let Some(packet) = packet.filter(|_| !muted.load(Ordering::Relaxed)) else {
            if output.is_some() && now.duration_since(last) > IDLE_CLOSE {
                debug!("som: sem som há um tempo; fechando o dispositivo");
                output = None;
                depacketizer.reset();
            } else if let Some(out) = &output
                && muted.load(Ordering::Relaxed)
            {
                out.queue.lock().expect("lock").clear();
            }
            continue;
        };
        if now.duration_since(last) > IDLE_CLOSE {
            depacketizer.reset();
        }
        last = now;
        if output.is_none() && failed_at.is_none_or(|t| now.duration_since(t) > RETRY) {
            match Output::open() {
                Ok(opened) => {
                    output = Some(opened);
                    failed_at = None;
                }
                Err(e) => {
                    // Once: a computer without speakers goes on without sound.
                    if !std::mem::replace(&mut warned, true) {
                        warn!("som: sem saída de som ({e}); a sessão continua sem som");
                    } else {
                        debug!("som: {e}");
                    }
                    failed_at = Some(now);
                }
            }
        }
        pcm.clear();
        depacketizer.push(&packet, &mut pcm);
        if let Some(out) = &mut output {
            out.play(&pcm, &mut scratch, &mut mapped);
            if report.elapsed() >= Duration::from_secs(5) {
                report = now;
                let queue = out.queue.lock().expect("lock");
                debug!(
                    target: LOG_TARGET,
                    "som: fila {} ms, {} vezes vazia, {} ms descartados | {} refeitos, {} disfarçados, {} atrasados",
                    queue.delay(out.rate).as_millis(),
                    queue.underruns,
                    queue.dropped / out.channels as u64 * 1000 / u64::from(out.rate.max(1)),
                    depacketizer.recovered,
                    depacketizer.concealed,
                    depacketizer.late,
                );
            }
        }
    }
}
