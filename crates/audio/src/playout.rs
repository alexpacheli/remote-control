//! Viewer side: packets in, a short and bounded queue of samples out.

use std::collections::VecDeque;
use std::time::Duration;

use hext_proto::AudioPacket;

use crate::{AudioError, CHANNELS, FRAME, SAMPLE_RATE};

/// Lost packets in a row concealed by Opus; a longer gap is the network
/// gone for a moment, and what comes after it starts over.
const MAX_CONCEAL: u32 = 4;

/// Decodes packets in the order they arrive, making up for lost ones.
pub struct Depacketizer {
    opus: opus::Decoder,
    /// The number expected next.
    next: Option<u32>,
    pcm: Vec<f32>,
    /// Frames rebuilt from the copy in the next packet, concealed, and
    /// packets dropped as late.
    pub recovered: u64,
    pub concealed: u64,
    pub late: u64,
}

impl Depacketizer {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            opus: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?,
            next: None,
            pcm: vec![0.0; FRAME * CHANNELS],
            recovered: 0,
            concealed: 0,
            late: 0,
        })
    }

    /// Appends the packet's sound (and what makes up for any lost before
    /// it) to `out`, as interleaved stereo at [`SAMPLE_RATE`].
    pub fn push(&mut self, packet: &AudioPacket, out: &mut Vec<f32>) {
        if let Some(next) = self.next {
            let ahead = packet.seq.wrapping_sub(next) as i32;
            if ahead < 0 {
                // Overtaken, or a copy: its time is past.
                self.late += 1;
                return;
            }
            let missing = ahead as u32;
            if missing > MAX_CONCEAL {
                let _ = self.opus.reset_state();
            } else {
                for i in 0..missing {
                    if i + 1 == missing && !packet.prev.is_empty() {
                        self.decode(&packet.prev.clone(), out);
                        self.recovered += 1;
                    } else {
                        // Opus makes up a frame from the ones before.
                        self.decode(&[], out);
                        self.concealed += 1;
                    }
                }
            }
        }
        self.decode(&packet.data, out);
        self.next = Some(packet.seq.wrapping_add(1));
    }

    fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) {
        match self.opus.decode_float(data, &mut self.pcm, false) {
            Ok(frames) => out.extend_from_slice(&self.pcm[..frames * CHANNELS]),
            Err(e) => tracing::debug!("opus: {e}"),
        }
    }

    /// Nothing came for a while: the next packet starts over.
    pub fn reset(&mut self) {
        self.next = None;
        let _ = self.opus.reset_state();
    }
}

/// How much sound waits for the device: enough to ride out the network's
/// jitter, little enough not to be heard late.
pub const TARGET: Duration = Duration::from_millis(40);
/// Past this, the oldest sound is dropped back to [`TARGET`]: a burst
/// after a stall, or a host's clock a little faster than this device's.
pub const HIGH: Duration = Duration::from_millis(70);

/// The queue between the decoder and the audio device, in the device's
/// rate and channels.
pub struct Playout {
    samples: VecDeque<f32>,
    channels: usize,
    target: usize,
    high: usize,
    /// Playing; otherwise filling up to `target` first.
    playing: bool,
    /// Times the queue ran dry, and samples dropped to keep it short.
    pub underruns: u64,
    pub dropped: u64,
}

impl Playout {
    pub fn new(rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let samples_in = |d: Duration| (rate as f64 * d.as_secs_f64()) as usize * channels;
        Self {
            samples: VecDeque::with_capacity(samples_in(HIGH) * 2),
            channels,
            target: samples_in(TARGET),
            high: samples_in(HIGH),
            playing: false,
            underruns: 0,
            dropped: 0,
        }
    }

    /// Sound waiting, in samples per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    /// How long the sound waiting takes to play.
    pub fn delay(&self, rate: u32) -> Duration {
        Duration::from_secs_f64(self.frames() as f64 / f64::from(rate.max(1)))
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
        if self.samples.len() > self.high {
            // (Whole frames of every channel, so left stays left.)
            let excess = self.samples.len() - self.target;
            let excess = excess - excess % self.channels;
            self.samples.drain(..excess);
            self.dropped += excess as u64;
        }
    }

    /// Fills `out` (the device's buffer): silence until the queue holds
    /// [`TARGET`], and again from where it runs dry.
    pub fn pull(&mut self, out: &mut [f32]) {
        if !self.playing && self.samples.len() >= self.target {
            self.playing = true;
        }
        let mut filled = 0;
        if self.playing {
            filled = out.len().min(self.samples.len());
            for (o, s) in out.iter_mut().zip(self.samples.drain(..filled)) {
                *o = s;
            }
            if filled < out.len() {
                self.playing = false;
                self.underruns += 1;
            }
        }
        out[filled..].fill(0.0);
    }

    pub fn clear(&mut self) {
        self.samples.clear();
        self.playing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Encoder;

    const RATE: u32 = 48_000;
    /// 10 ms of stereo.
    const TEN_MS: usize = 480 * 2;

    fn ms(samples: usize) -> usize {
        samples / 2 / 48
    }

    #[test]
    fn it_waits_for_the_target_then_plays_steadily() {
        let mut playout = Playout::new(RATE, 2);
        let mut out = vec![1.0; TEN_MS];
        playout.push(&[0.5; TEN_MS * 3]);
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "30 ms is not enough yet");
        playout.push(&[0.5; TEN_MS]);
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
        // 10 ms in, 10 ms out: it neither drops nor runs dry.
        for _ in 0..1000 {
            playout.push(&[0.5; TEN_MS]);
            playout.pull(&mut out);
            assert!(out.iter().all(|&s| s == 0.5));
        }
        assert_eq!((playout.underruns, playout.dropped), (0, 0));
        assert_eq!(ms(playout.samples.len()), 30);
    }

    #[test]
    fn a_burst_never_makes_it_late() {
        let mut playout = Playout::new(RATE, 2);
        let mut out = vec![0.0; TEN_MS];
        // The network held 500 ms back and lets it all go at once.
        for _ in 0..50 {
            playout.push(&[0.5; TEN_MS]);
            assert!(playout.delay(RATE) <= HIGH);
        }
        assert!(playout.delay(RATE) >= TARGET - Duration::from_millis(10));
        assert!(playout.dropped > 0);
        // And the delay stays bounded whatever the pattern.
        for round in 0..200 {
            let burst = if round % 10 == 0 { 8 } else { 1 };
            for _ in 0..burst {
                playout.push(&[0.5; TEN_MS]);
            }
            playout.pull(&mut out);
            assert!(playout.delay(RATE) <= HIGH, "round {round}");
        }
    }

    #[test]
    fn a_gap_plays_silence_then_fills_up_again() {
        let mut playout = Playout::new(RATE, 2);
        let mut out = vec![0.0; TEN_MS];
        for _ in 0..4 {
            playout.push(&[0.5; TEN_MS]);
        }
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.5));
        // Nothing comes: the rest plays, then silence, never old sound.
        for _ in 0..3 {
            playout.pull(&mut out);
        }
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(playout.underruns, 1);
        // It comes back: 40 ms are gathered before playing again.
        playout.push(&[0.7; TEN_MS * 2]);
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        playout.push(&[0.7; TEN_MS * 2]);
        playout.pull(&mut out);
        assert!(out.iter().all(|&s| s == 0.7));
    }

    #[test]
    fn a_short_pull_is_padded_with_silence() {
        let mut playout = Playout::new(RATE, 2);
        playout.push(&[0.5; TEN_MS * 4 + 2]);
        let mut out = vec![1.0; TEN_MS * 5];
        playout.pull(&mut out);
        assert!(out[..TEN_MS * 4 + 2].iter().all(|&s| s == 0.5));
        assert!(out[TEN_MS * 4 + 2..].iter().all(|&s| s == 0.0));
    }

    fn packets(n: usize) -> Vec<AudioPacket> {
        let mut encoder = Encoder::new().unwrap();
        let tone: Vec<f32> = (0..n * FRAME)
            .flat_map(|i| {
                let s = (i as f32 * 0.05).sin() * 0.3;
                [s, s]
            })
            .collect();
        let mut out = Vec::new();
        encoder.push(&tone, &mut out);
        out
    }

    #[test]
    fn every_packet_is_10_ms_and_losses_are_made_up() {
        let sent = packets(20);
        let mut depacketizer = Depacketizer::new().unwrap();
        let mut pcm = Vec::new();
        for (i, packet) in sent.iter().enumerate() {
            // One lost (rebuilt from the copy), then two in a row (one
            // concealed, one rebuilt).
            if [3, 8, 9].contains(&i) {
                continue;
            }
            depacketizer.push(packet, &mut pcm);
        }
        assert_eq!(pcm.len(), 20 * FRAME * CHANNELS, "no time lost");
        assert_eq!(depacketizer.recovered, 2);
        assert_eq!(depacketizer.concealed, 1);
        // An old packet arriving late is dropped.
        depacketizer.push(&sent[3], &mut pcm);
        assert_eq!(depacketizer.late, 1);
        assert_eq!(pcm.len(), 20 * FRAME * CHANNELS);
    }

    #[test]
    fn a_long_gap_starts_over_without_making_up_sound() {
        let sent = packets(20);
        let mut depacketizer = Depacketizer::new().unwrap();
        let mut pcm = Vec::new();
        depacketizer.push(&sent[0], &mut pcm);
        depacketizer.push(&sent[15], &mut pcm);
        assert_eq!(pcm.len(), 2 * FRAME * CHANNELS);
        assert_eq!(depacketizer.concealed, 0);
    }
}
