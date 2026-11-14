//! Host side: stereo samples in, Opus packets out, nothing while silent.

use hext_proto::AudioPacket;

use crate::dsp::Framer;
use crate::{AudioError, BITRATE, CHANNELS, FRAME, SAMPLE_RATE};

/// Louder than this somewhere in a frame is sound (about -80 dBFS: digital
/// silence and the dither of some drivers stay below).
const SILENCE_LEVEL: f32 = 1e-4;

/// Silent frames still sent after the sound stops, so it ends as it
/// sounded (the decoder's tail, reverb fading out) and a short pause in the
/// middle of a song is not cut.
pub const HANGOVER: u32 = 20;

/// Whether a frame is worth sending.
pub struct SilenceGate {
    /// Silent frames in a row.
    quiet: u32,
}

impl Default for SilenceGate {
    /// Starts closed: silence at the start sends nothing.
    fn default() -> Self {
        Self { quiet: HANGOVER }
    }
}

impl SilenceGate {
    /// Whether to send this frame.
    pub fn pass(&mut self, frame: &[f32]) -> bool {
        if frame.iter().any(|s| s.abs() > SILENCE_LEVEL) {
            self.quiet = 0;
            return true;
        }
        self.quiet = self.quiet.saturating_add(1);
        self.quiet <= HANGOVER
    }
}

/// Sizes a 10 ms packet never comes near (at 64 kbit/s it is about 80 bytes).
const MAX_PACKET: usize = 1000;

/// Frames, gates and encodes the computer's sound.
pub struct Encoder {
    opus: opus::Encoder,
    framer: Framer,
    gate: SilenceGate,
    seq: u32,
    /// The last packet sent, carried again by the next; empty after silence.
    prev: Vec<u8>,
    /// Frames sent and kept back, for the log.
    pub sent: u64,
    pub silent: u64,
}

impl Encoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut opus = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )?;
        opus.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        Ok(Self {
            opus,
            framer: Framer::default(),
            gate: SilenceGate::default(),
            seq: 0,
            prev: Vec::new(),
            sent: 0,
            silent: 0,
        })
    }

    /// Takes interleaved stereo samples at [`SAMPLE_RATE`]; appends the
    /// packets due to `out`.
    pub fn push(&mut self, samples: &[f32], out: &mut Vec<AudioPacket>) {
        let Self {
            opus,
            framer,
            gate,
            seq,
            prev,
            sent,
            silent,
        } = self;
        framer.push(samples, |frame| {
            debug_assert_eq!(frame.len(), FRAME * CHANNELS);
            if !gate.pass(frame) {
                prev.clear();
                *silent += 1;
                return;
            }
            let data = match opus.encode_vec_float(frame, MAX_PACKET) {
                Ok(data) => data,
                Err(e) => {
                    tracing::debug!("opus: {e}");
                    prev.clear();
                    return;
                }
            };
            out.push(AudioPacket {
                seq: *seq,
                data: data.clone(),
                prev: std::mem::replace(prev, data),
            });
            *seq = seq.wrapping_add(1);
            *sent += 1;
        });
    }

    /// The sound stopped (capture closed): what was half a frame goes, and
    /// the next sound starts like the first.
    pub fn reset(&mut self) {
        self.framer.clear();
        self.gate = SilenceGate::default();
        self.prev.clear();
        let _ = self.opus.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, start: usize) -> Vec<f32> {
        (start * FRAME..(start + frames) * FRAME)
            .flat_map(|i| {
                let s = (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.3;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn silence_sends_nothing() {
        let mut encoder = Encoder::new().unwrap();
        let mut out = Vec::new();
        encoder.push(&vec![0.0; FRAME * CHANNELS * 100], &mut out);
        assert!(out.is_empty());
        // Dither below the level is silence too.
        let dither: Vec<f32> = (0..FRAME * CHANNELS * 10)
            .map(|i| if i % 2 == 0 { 3e-5 } else { -3e-5 })
            .collect();
        encoder.push(&dither, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn sound_goes_then_its_tail_then_nothing() {
        let mut encoder = Encoder::new().unwrap();
        let mut out = Vec::new();
        encoder.push(&tone(50, 0), &mut out);
        assert_eq!(out.len(), 50);
        encoder.push(&vec![0.0; FRAME * CHANNELS * 100], &mut out);
        assert_eq!(out.len(), 50 + HANGOVER as usize);
        // Numbered one by one, each carrying the one before.
        for pair in out.windows(2) {
            assert_eq!(pair[1].seq, pair[0].seq + 1);
            assert_eq!(pair[1].prev, pair[0].data);
        }
        assert!(out[0].prev.is_empty());
        // After the silence: the numbers go on, nothing old is carried.
        let before = out.len();
        encoder.push(&tone(3, 0), &mut out);
        assert_eq!(out.len(), before + 3);
        assert_eq!(out[before].seq, out[before - 1].seq + 1);
        assert!(out[before].prev.is_empty());
        assert!(out.iter().all(|p| p.data.len() < 400));
    }

    #[test]
    fn the_gate_opens_at_once_and_closes_late() {
        let mut gate = SilenceGate::default();
        let quiet = vec![0.0; FRAME * CHANNELS];
        let loud = vec![0.5; FRAME * CHANNELS];
        assert!(!gate.pass(&quiet));
        assert!(gate.pass(&loud));
        for _ in 0..HANGOVER {
            assert!(gate.pass(&quiet));
        }
        assert!(!gate.pass(&quiet));
        assert!(gate.pass(&loud));
    }
}
