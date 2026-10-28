use std::time::{Duration, Instant};

use hext_metrics::Rolling;
use hext_net::video::Counters;

/// Viewer-side statistics for the stats line (window title for now).
pub struct Stats {
    window_start: Instant,
    frames: u32,
    bytes: u64,
    capture: Rolling,
    encode: Rolling,
    decode: Rolling,
    /// Last completed one-second window.
    last: Option<Snapshot>,
    hardware: bool,
    decode_hardware: bool,
    pub width: u32,
    pub height: u32,
    /// FEC and loss totals since the session started.
    pub video: Counters,
}

/// Rates and averages over the last full second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub fps: f64,
    pub mbps: f64,
    pub capture_ms: f64,
    pub encode_ms: f64,
    pub decode_ms: f64,
    /// The host encodes on the GPU.
    pub hardware: bool,
    /// This viewer decodes on the GPU.
    pub decode_hardware: bool,
    pub video: Counters,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            bytes: 0,
            capture: Rolling::new(120),
            encode: Rolling::new(120),
            decode: Rolling::new(120),
            last: None,
            hardware: false,
            decode_hardware: false,
            width: 0,
            height: 0,
            video: Counters::default(),
        }
    }
}

impl Stats {
    pub fn record(
        &mut self,
        bytes: usize,
        capture: Duration,
        encode: Duration,
        decode: Duration,
        hardware: bool,
        decode_hardware: bool,
    ) {
        self.roll_window();
        self.hardware = hardware;
        self.decode_hardware = decode_hardware;
        self.frames += 1;
        self.bytes += bytes as u64;
        self.capture.push(capture);
        self.encode.push(encode);
        self.decode.push(decode);
    }

    /// Statistics of the last full second, if one has passed.
    pub fn snapshot(&mut self) -> Option<Snapshot> {
        self.roll_window();
        self.last
    }

    fn roll_window(&mut self) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let secs = elapsed.as_secs_f64();
        let avg_ms = |r: &Rolling| r.summary().map_or(0.0, |s| s.avg.as_secs_f64() * 1000.0);
        self.last = Some(Snapshot {
            fps: self.frames as f64 / secs,
            mbps: self.bytes as f64 * 8.0 / secs / 1_000_000.0,
            capture_ms: avg_ms(&self.capture),
            encode_ms: avg_ms(&self.encode),
            decode_ms: avg_ms(&self.decode),
            hardware: self.hardware,
            decode_hardware: self.decode_hardware,
            video: self.video,
        });
        self.window_start = Instant::now();
        self.frames = 0;
        self.bytes = 0;
    }
}

impl std::fmt::Display for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.0} fps | {:.1} Mbps | captura {:.1} ms | encode {:.1} ms ({}) | decode {:.1} ms ({})",
            self.fps,
            self.mbps,
            self.capture_ms,
            self.encode_ms,
            unit(self.hardware),
            self.decode_ms,
            unit(self.decode_hardware),
        )?;
        // Only once the network actually loses something.
        if self.video != Counters::default() {
            write!(
                f,
                " | total: FEC {} blocos, {} quadros perdidos",
                self.video.recovered, self.video.lost
            )?;
        }
        Ok(())
    }
}

fn unit(gpu: bool) -> &'static str {
    if gpu { "GPU" } else { "CPU" }
}
