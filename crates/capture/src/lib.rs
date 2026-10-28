//! Screen capture.
//!
//! [`open`] returns a [`Capturer`] for one display: DXGI Desktop Duplication
//! on Windows, ScreenCaptureKit on macOS, or a synthetic [`TestPattern`] (any
//! OS) for development and tests.

use std::time::Duration;

#[cfg(windows)]
mod gpu_nv12;
#[cfg(target_os = "macos")]
mod macos;
mod test_pattern;
#[cfg(windows)]
mod windows;

pub use test_pattern::TestPattern;

/// Layout of a captured image's pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Tightly packed BGRA rows (`width * 4` bytes each).
    Bgra,
    /// 4:2:0 YUV, BT.601 limited range, as video encoders take it: a
    /// `width * height` Y plane, then a half-height plane of interleaved
    /// U/V pairs (`width` bytes per row). No conversion left for the CPU.
    Nv12,
}

/// A captured image. Width and height are always even, as 4:2:0 video requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// The pixels in system memory; empty when the image is only on the GPU
    /// (see `texture`).
    pub data: Vec<u8>,
    /// Windows: the image as an NV12 texture on the capture's D3D11 device
    /// (created with video support and multithread protection), handed to
    /// the encoder without leaving the GPU. Valid until a few more frames
    /// are captured: the capturer reuses a small ring of textures.
    #[cfg(windows)]
    pub texture: Option<::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>,
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
