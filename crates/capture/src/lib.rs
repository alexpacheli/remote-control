//! Screen capture.
//!
//! [`open`] returns a [`Capturer`] for one display: DXGI Desktop Duplication
//! on Windows, ScreenCaptureKit on macOS, or a synthetic [`TestPattern`] (any
//! OS) for development and tests.

use std::time::Duration;

#[cfg(target_os = "macos")]
mod macos;
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
    #[error("sem permissão para capturar a tela: {0}")]
    PermissionDenied(String),
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

/// Checks (and on macOS requests) the permission to capture the screen, so
/// a host can report a missing one at startup rather than on first connection.
pub fn preflight() -> Result<(), CaptureError> {
    #[cfg(target_os = "macos")]
    {
        macos::preflight()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}

/// Opens display `index` (0 = primary), or the test pattern, delivering at
/// most `max_fps` frames per second where the platform can throttle.
pub fn open(
    index: usize,
    test_pattern: bool,
    max_fps: u32,
) -> Result<Box<dyn Capturer>, CaptureError> {
    if test_pattern {
        return Ok(Box::new(TestPattern::new(1280, 720, max_fps)));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::DxgiCapturer::new(index)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::ScreenCaptureKitCapturer::new(
            index, max_fps,
        )?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (index, max_fps);
        Err(CaptureError::Unsupported)
    }
}

/// Rounds a dimension down to an even number.
fn even(v: u32) -> u32 {
    v & !1
}
