//! Screen capture.
//!
//! [`open`] returns a [`Capturer`] for one display: DXGI Desktop Duplication
//! on Windows, or a synthetic [`TestPattern`] (any OS) for development and
//! tests. macOS capture (ScreenCaptureKit) arrives in F2.

use std::time::Duration;

mod test_pattern;
#[cfg(windows)]
mod windows;

pub use test_pattern::TestPattern;

/// A captured image: tightly packed BGRA rows (`width * 4` bytes each).
///
/// Width and height are always even, as 4:2:0 video requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Where a display sits on the host's virtual desktop, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("display {0} not found")]
    NoSuchDisplay(usize),
    #[error("screen capture is not available on this OS yet; use the test pattern")]
    Unsupported,
    #[error("capture failed: {0}")]
    Platform(String),
}

pub trait Capturer {
    /// Waits up to `timeout` for the screen to change.
    ///
    /// Returns `true` when [`Capturer::frame`] holds a new image, `false` if
    /// nothing changed (a still screen costs no encoding or bandwidth).
    fn next_frame(&mut self, timeout: Duration) -> Result<bool, CaptureError>;

    /// The most recent image, if any was captured yet.
    fn frame(&self) -> Option<&Frame>;

    /// Position of the captured display on the virtual desktop.
    fn rect(&self) -> Rect;

    /// How long producing the last frame took (excluding the wait for a change).
    fn copy_time(&self) -> Duration;
}

/// Opens display `index` (0 = primary), or the test pattern.
pub fn open(index: usize, test_pattern: bool) -> Result<Box<dyn Capturer>, CaptureError> {
    if test_pattern {
        return Ok(Box::new(TestPattern::new(1280, 720, 60)));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::DxgiCapturer::new(index)?))
    }
    #[cfg(not(windows))]
    {
        let _ = index;
        Err(CaptureError::Unsupported)
    }
}

/// Rounds a dimension down to an even number.
fn even(v: u32) -> u32 {
    v & !1
}
