//! Small sample plumbing: frames, rates and channels.

use crate::{CHANNELS, FRAME};

/// Cuts a stream of stereo samples into frames of [`FRAME`] samples per
/// channel.
#[derive(Default)]
pub struct Framer {
    pending: Vec<f32>,
}

impl Framer {
    /// Adds interleaved stereo samples; `frame` runs for each frame completed.
    pub fn push(&mut self, samples: &[f32], mut frame: impl FnMut(&[f32])) {
        let len = FRAME * CHANNELS;
        let mut rest = samples;
        if !self.pending.is_empty() {
            let take = (len - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() < len {
                return;
            }
            frame(&self.pending);
            self.pending.clear();
        }
        let mut chunks = rest.chunks_exact(len);
        for chunk in &mut chunks {
            frame(chunk);
        }
        self.pending.extend_from_slice(chunks.remainder());
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// Changes the rate of interleaved samples by linear interpolation: plenty
/// for a technician hearing the computer, and nothing to add latency.
pub struct Resampler {
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls, in input frames after `last`.
    pos: f64,
    /// The last input frame of the previous call.
    last: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Self {
        Self {
            channels: channels.max(1),
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 0.0,
            last: vec![0.0; channels.max(1)],
        }
    }

    pub fn is_identity(&self) -> bool {
        self.step == 1.0
    }

    /// Appends `input` at the new rate to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        let ch = self.channels;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        // Frame `i` of [last, input...]: 0 is `last`.
        let at = |i: usize, c: usize| {
            if i == 0 {
                self.last[c]
            } else {
                input[(i - 1) * ch + c]
            }
        };
        let mut pos = self.pos;
        while pos < frames as f64 {
            let i = pos as usize;
            let frac = (pos - i as f64) as f32;
            for c in 0..ch {
                let (a, b) = (at(i, c), at(i + 1, c));
                out.push(a + (b - a) * frac);
            }
            pos += self.step;
        }
        self.pos = pos - frames as f64;
        self.last
            .copy_from_slice(&input[(frames - 1) * ch..frames * ch]);
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.last.fill(0.0);
    }
}

/// Interleaved samples of `channels` channels as stereo: mono doubled,
/// beyond two only the front left and right.
pub fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(input.iter().flat_map(|&s| [s, s])),
        2 => out.extend_from_slice(input),
        n => {
            for frame in input.chunks_exact(n) {
                out.extend_from_slice(&frame[..2]);
            }
        }
    }
}

/// Stereo samples for a device of `channels` channels: mono mixed down,
/// beyond two the other channels silent.
pub fn from_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(input.chunks_exact(2).map(|f| (f[0] + f[1]) * 0.5)),
        2 => out.extend_from_slice(input),
        n => {
            for frame in input.chunks_exact(2) {
                out.extend_from_slice(frame);
                out.extend(std::iter::repeat_n(0.0, n - 2));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_whole_however_the_samples_arrive() {
        let mut framer = Framer::default();
        let samples: Vec<f32> = (0..FRAME * CHANNELS * 3).map(|i| i as f32).collect();
        let mut frames = Vec::new();
        for chunk in samples.chunks(333) {
            framer.push(chunk, |f| frames.push(f.to_vec()));
        }
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| f.len() == FRAME * CHANNELS));
        assert_eq!(frames.concat(), samples);
    }

    #[test]
    fn resampling_keeps_the_time() {
        // 1 s at 44.1 kHz becomes 1 s at 48 kHz, in any chunks.
        let mut resampler = Resampler::new(44_100, 48_000, 2);
        let input = vec![0.5f32; 44_100 * 2];
        let mut out = Vec::new();
        for chunk in input.chunks(441 * 2) {
            resampler.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        // A constant stays constant (after the first sample, from silence).
        assert!(out[4..].iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampling_down_and_through() {
        let mut down = Resampler::new(48_000, 44_100, 1);
        let mut out = Vec::new();
        for _ in 0..100 {
            down.process(&[0.25; 480], &mut out);
        }
        assert!((44_090..=44_110).contains(&out.len()), "{}", out.len());
        let mut same = Resampler::new(48_000, 48_000, 2);
        let mut out = Vec::new();
        same.process(&[1.0, 2.0, 3.0, 4.0], &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn channels_map_both_ways() {
        let mut out = Vec::new();
        to_stereo(&[1.0, 2.0], 1, &mut out);
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0]);
        out.clear();
        // 5.1: front left and right.
        to_stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, &mut out);
        assert_eq!(out, [1.0, 2.0]);
        out.clear();
        from_stereo(&[1.0, 3.0], 1, &mut out);
        assert_eq!(out, [2.0]);
        out.clear();
        from_stereo(&[1.0, 3.0], 4, &mut out);
        assert_eq!(out, [1.0, 3.0, 0.0, 0.0]);
    }
}
