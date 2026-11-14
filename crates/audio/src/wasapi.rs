//! Windows: what the default output device plays, through WASAPI loopback
//! (`cpal` opens an input stream on an output device in loopback mode).
//!
//! With nothing playing, Windows delivers nothing at all: the silence gate
//! only has the end of each sound to deal with.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::capture::Chunks;
use crate::dsp::{Resampler, to_stereo};
use crate::{AudioError, SAMPLE_RATE};

/// Chunks waiting for the encoder (each a few ms): plenty.
const QUEUE: usize = 64;
/// How often to check whether the default device changed (headphones
/// plugged in, another one chosen).
const CHECK_EVERY: Duration = Duration::from_secs(2);

pub fn available() -> bool {
    true
}

pub struct Loopback {
    _stream: cpal::Stream,
    device: Option<cpal::DeviceId>,
    broken: Arc<AtomicBool>,
    checked: Instant,
}

fn device_error(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

impl Loopback {
    pub fn open() -> Result<(Self, Chunks), AudioError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| device_error("nenhum dispositivo de saída de som"))?;
        let config = device.default_output_config().map_err(device_error)?;
        let (tx, rx) = sync_channel::<Vec<f32>>(QUEUE);
        let broken = Arc::new(AtomicBool::new(false));
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, tx, &broken),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, tx, &broken),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, tx, &broken),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, tx, &broken),
            other => Err(device_error(format!("formato sem suporte: {other:?}"))),
        }?;
        stream.play().map_err(device_error)?;
        tracing::debug!(
            "som: loopback de \"{}\" a {} Hz, {} canais",
            device
                .description()
                .map(|d| d.name().to_owned())
                .unwrap_or_default(),
            config.sample_rate(),
            config.channels()
        );
        Ok((
            Self {
                _stream: stream,
                device: device.id().ok(),
                broken,
                checked: Instant::now(),
            },
            rx,
        ))
    }

    /// The stream failed, or the default device is another one now.
    pub fn needs_reopen(&mut self) -> bool {
        if self.broken.load(Ordering::Relaxed) {
            return true;
        }
        if self.checked.elapsed() < CHECK_EVERY {
            return false;
        }
        self.checked = Instant::now();
        let now = cpal::default_host()
            .default_output_device()
            .and_then(|d| d.id().ok());
        now.is_some() && now != self.device
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    chunks: std::sync::mpsc::SyncSender<Vec<f32>>,
    broken: &Arc<AtomicBool>,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels());
    let mut resampler = Resampler::new(config.sample_rate(), SAMPLE_RATE, 2);
    let (mut floats, mut stereo) = (Vec::new(), Vec::new());
    let failed = broken.clone();
    device
        .build_input_stream::<T, _, _>(
            config.config(),
            move |data: &[T], _| {
                floats.clear();
                floats.extend(data.iter().map(|s| cpal::Sample::to_sample::<f32>(*s)));
                stereo.clear();
                to_stereo(&floats, channels, &mut stereo);
                let mut out = Vec::with_capacity(stereo.len() + 16);
                resampler.process(&stereo, &mut out);
                // A full queue: the encoder is stuck, and this is too late.
                let _ = chunks.try_send(out);
            },
            move |e| {
                tracing::debug!("som: {e}");
                failed.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map_err(device_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records 2 s of what this computer plays and reports the levels: play
    /// something while it runs. `cargo test -p hext-audio loopback -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a sound device, and something playing to hear anything"]
    fn loopback_records_what_plays() {
        let (_loopback, chunks) = Loopback::open().expect("loopback");
        let until = Instant::now() + Duration::from_secs(2);
        let mut samples = Vec::new();
        while Instant::now() < until {
            if let Ok(chunk) = chunks.recv_timeout(Duration::from_millis(100)) {
                samples.extend(chunk);
            }
        }
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt();
        println!(
            "{} ms gravados (48 kHz estéreo), pico {peak:.4}, RMS {rms:.4}",
            samples.len() / 2 / 48
        );
        let mut encoder = crate::Encoder::new().unwrap();
        let mut packets = Vec::new();
        encoder.push(&samples, &mut packets);
        let bytes: usize = packets.iter().map(|p| p.data.len() + p.prev.len()).sum();
        println!(
            "{} pacotes ({} em silêncio), {bytes} bytes de Opus",
            packets.len(),
            encoder.silent
        );
    }

    /// Plays a tone through [`crate::Player`] and hears it back through the
    /// loopback: the time from the packet handed to the player to the sound
    /// recorded is the viewer's queue, the output device and the capture.
    /// It plays a short beep. `cargo test -p hext-audio round_trip -- --ignored --nocapture`
    #[test]
    #[ignore = "plays a beep on this computer's speakers"]
    fn round_trip_through_player_and_loopback() {
        use crate::{FRAME, Player, SAMPLE_RATE};
        let (_loopback, chunks) = Loopback::open().expect("loopback");
        let player = Player::new();
        let mut encoder = crate::Encoder::new().unwrap();
        let mut packets = Vec::new();
        // 500 ms barely above silence (opens the device, fills the queue),
        // then 300 ms of tone.
        let signal: Vec<f32> = (0..80 * FRAME)
            .flat_map(|i| {
                let level = if i < 50 * FRAME { 3e-4 } else { 0.3 };
                let s = (i as f32 * 880.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin();
                [s * level, s * level]
            })
            .collect();
        encoder.push(&signal, &mut packets);
        assert_eq!(packets.len(), 80);
        let start = Instant::now();
        let mut tone_at = None;
        let mut heard_at = None;
        for (i, packet) in packets.into_iter().enumerate() {
            let due = start + Duration::from_millis(10 * i as u64);
            while Instant::now() < due {
                while let Ok(chunk) = chunks.try_recv() {
                    if tone_at.is_some()
                        && heard_at.is_none()
                        && chunk.iter().any(|s| s.abs() > 0.05)
                    {
                        heard_at = Some(Instant::now());
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if i == 50 {
                tone_at = Some(Instant::now());
            }
            player.push(packet);
        }
        let until = Instant::now() + Duration::from_secs(1);
        while heard_at.is_none() && Instant::now() < until {
            if let Ok(chunk) = chunks.recv_timeout(Duration::from_millis(5))
                && chunk.iter().any(|s| s.abs() > 0.05)
            {
                heard_at = Some(Instant::now());
            }
        }
        let (tone_at, heard_at) = (tone_at.unwrap(), heard_at.expect("the tone came back"));
        println!(
            "do pacote ao som gravado: {} ms (fila do visualizador + saída + captura)",
            heard_at.duration_since(tone_at).as_millis()
        );
    }
}
