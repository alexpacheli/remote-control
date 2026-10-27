use std::time::{Duration, Instant};

use crate::{CaptureError, Capturer, Frame, Rect, even};

/// Synthetic animated screen: a gradient background with a moving square.
///
/// Produces frames at a fixed rate so the whole pipeline can run on machines
/// without a supported capture backend (CI, Linux, macOS before F2).
pub struct TestPattern {
    frame: Frame,
    interval: Duration,
    next_at: Instant,
    tick: u32,
    copy_time: Duration,
}

impl TestPattern {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let (width, height) = (even(width), even(height));
        Self {
            frame: Frame {
                width,
                height,
                data: vec![0; (width * height * 4) as usize],
            },
            interval: Duration::from_secs(1) / fps.max(1),
            next_at: Instant::now(),
            tick: 0,
            copy_time: Duration::ZERO,
        }
    }

    fn draw(&mut self) {
        let (w, h) = (self.frame.width as usize, self.frame.height as usize);
        let t = self.tick as usize;
        let size = h / 6;
        let sq_x = (t * 8) % (w - size);
        let sq_y = (h - size) / 2;
        for (y, row) in self.frame.data.chunks_exact_mut(w * 4).enumerate() {
            for (x, px) in row.chunks_exact_mut(4).enumerate() {
                let in_square =
                    (sq_x..sq_x + size).contains(&x) && (sq_y..sq_y + size).contains(&y);
                let (b, g, r) = if in_square {
                    (255, 255, 255)
                } else {
                    (
                        (x * 255 / w) as u8,
                        (y * 255 / h) as u8,
                        ((t * 2) % 256) as u8,
                    )
                };
                px.copy_from_slice(&[b, g, r, 255]);
            }
        }
    }
}

impl Capturer for TestPattern {
    fn next_frame(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        let now = Instant::now();
        if self.next_at > now {
            let wait = self.next_at - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(false);
            }
            std::thread::sleep(wait);
        }
        // Skip missed ticks instead of bursting to catch up.
        self.next_at = self.next_at.max(Instant::now() - self.interval) + self.interval;
        let started = Instant::now();
        self.draw();
        self.copy_time = started.elapsed();
        self.tick = self.tick.wrapping_add(1);
        Ok(true)
    }

    fn frame(&self) -> Option<&Frame> {
        (self.tick > 0).then_some(&self.frame)
    }

    fn rect(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.frame.width,
            height: self.frame.height,
        }
    }

    fn copy_time(&self) -> Duration {
        self.copy_time
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_changing_even_frames() {
        let mut tp = TestPattern::new(321, 201, 1000);
        assert!(tp.frame().is_none());
        assert!(tp.next_frame(Duration::from_secs(1)).unwrap());
        let first = tp.frame().unwrap().clone();
        assert_eq!((first.width, first.height), (320, 200));
        assert_eq!(first.data.len(), 320 * 200 * 4);
        assert!(tp.next_frame(Duration::from_secs(1)).unwrap());
        assert_ne!(tp.frame().unwrap().data, first.data);
    }

    #[test]
    fn respects_timeout() {
        let mut tp = TestPattern::new(64, 64, 1);
        assert!(tp.next_frame(Duration::from_secs(1)).unwrap());
        // The next frame is a second away.
        assert!(!tp.next_frame(Duration::from_millis(5)).unwrap());
    }
}
