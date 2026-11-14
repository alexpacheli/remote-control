//! Screen capture.
//!
//! [`displays`] lists the displays and [`open`] returns a [`Capturer`] for
//! one: DXGI Desktop Duplication on Windows, ScreenCaptureKit on macOS, or a
//! synthetic [`TestPattern`] (any OS) for development and tests.

use std::time::Duration;

mod awake;
#[cfg(windows)]
mod desktop;
#[cfg(windows)]
mod gpu_nv12;
#[cfg(target_os = "macos")]
mod macos;
mod test_pattern;
pub mod virtual_display;
#[cfg(windows)]
mod windows;

pub use awake::KeepAwake;
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

/// A display of this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// Tells it from the others for as long as it stays connected, wherever
    /// the list puts it.
    pub id: String,
    pub rect: Rect,
    /// The monitor that exists only for the session ([`virtual_display`]).
    pub virtual_display: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("display {0} not found")]
    NoSuchDisplay(usize),
    #[error("display {0} is gone")]
    DisplayGone(String),
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

/// Moves the calling thread to the desktop getting input now (Windows:
/// the user's, or the sign-in / UAC one, where this process may), so input
/// injected from it lands there; whether it moved. Capture does this by
/// itself.
pub fn follow_input_desktop() -> bool {
    #[cfg(windows)]
    {
        desktop::follow_input_desktop()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The name of the desktop getting input now, where there is such a thing.
pub fn input_desktop_name() -> Option<String> {
    #[cfg(windows)]
    {
        desktop::input_desktop_name()
    }
    #[cfg(not(windows))]
    {
        None
    }
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

/// macOS: leaves these windows of this process (`CGWindowID`s) out of every
/// capture, the running ones included, for the privacy screen. Ok once every
/// running capture confirmed; an empty list only forgets the last one.
#[cfg(target_os = "macos")]
pub fn exclude_windows(ids: &[u32]) -> Result<(), CaptureError> {
    macos::exclude_windows(ids)
}

/// The displays of this computer, in the order sessions number them: the
/// main one first (the one at the origin, on every OS), then from left to
/// right.
pub fn displays(test_pattern: bool) -> Result<Vec<Display>, CaptureError> {
    if test_pattern {
        return Ok(numbered(test_pattern::displays()));
    }
    #[cfg(windows)]
    {
        windows::displays().map(numbered)
    }
    #[cfg(target_os = "macos")]
    {
        macos::displays().map(numbered)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(CaptureError::Unsupported)
    }
}

fn numbered(mut displays: Vec<Display>) -> Vec<Display> {
    displays.sort_by_key(|d| ((d.rect.x, d.rect.y) != (0, 0), d.rect.x, d.rect.y));
    displays
}

/// Opens display `index` of [`displays`] (0 = main), or the test pattern,
/// delivering at most `max_fps` frames per second where the platform can
/// throttle.
pub fn open(
    index: usize,
    test_pattern: bool,
    max_fps: u32,
) -> Result<Box<dyn Capturer>, CaptureError> {
    let displays = displays(test_pattern)?;
    let display = displays
        .get(index)
        .ok_or(CaptureError::NoSuchDisplay(index))?;
    open_display(display, test_pattern, max_fps)
}

/// Opens one of the [`displays`], as [`open`] does.
pub fn open_display(
    display: &Display,
    test_pattern: bool,
    max_fps: u32,
) -> Result<Box<dyn Capturer>, CaptureError> {
    if test_pattern {
        return Ok(Box::new(TestPattern::on(display.rect, max_fps)));
    }
    #[cfg(windows)]
    {
        Ok(Box::new(windows::DxgiCapturer::new(&display.id)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::ScreenCaptureKitCapturer::new(
            &display.id,
            max_fps,
        )?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = max_fps;
        Err(CaptureError::Unsupported)
    }
}

/// Rounds a dimension down to an even number.
fn even(v: u32) -> u32 {
    v & !1
}

/// Every how many pixels, across and down, [`is_black`] looks.
const BLACK_STEP: usize = 4;

/// The brightest a black screen gets: in luma (16 is black in the limited
/// range the NV12 images use), or in any BGRA channel.
const BLACK_LUMA: u8 = 24;
const BLACK_BGRA: u8 = 8;

/// Whether `frame` is a black screen, looking at one pixel in every
/// [`BLACK_STEP`] across and down: none brighter than black. A screen with
/// anything on it (a taskbar, a window, text) has bright pixels; a monitor
/// that is listed but shows nothing does not. `None`: it could not be read.
///
/// Reads the image back from the GPU when it is only there (Windows): a few
/// milliseconds, so it is for now and then, not every frame.
pub fn is_black(frame: &Frame) -> Option<bool> {
    let (width, height) = (frame.width as usize, frame.height as usize);
    if !frame.data.is_empty() {
        return Some(match frame.format {
            PixelFormat::Nv12 => luma_is_black(&frame.data, width, height, width),
            PixelFormat::Bgra => frame
                .data
                .chunks_exact(width * 4)
                .take(height)
                .step_by(BLACK_STEP)
                .all(|row| {
                    row.chunks_exact(4)
                        .step_by(BLACK_STEP)
                        .all(|px| px[0].max(px[1]).max(px[2]) <= BLACK_BGRA)
                }),
        });
    }
    #[cfg(windows)]
    if let Some(texture) = &frame.texture {
        return gpu_nv12::luma(texture, |luma, pitch| {
            luma_is_black(luma, width, height, pitch)
        })
        .inspect_err(|e| tracing::debug!("tela preta: {e}"))
        .ok();
    }
    None
}

/// Whether the luma plane `luma` (`height` rows of `pitch` bytes, `width`
/// of them the image) is black.
fn luma_is_black(luma: &[u8], width: usize, height: usize, pitch: usize) -> bool {
    luma.chunks(pitch)
        .take(height)
        .step_by(BLACK_STEP)
        .all(|row| {
            row[..width.min(row.len())]
                .iter()
                .step_by(BLACK_STEP)
                .all(|&y| y <= BLACK_LUMA)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(width: u32, height: u32, pixel: [u8; 4]) -> Frame {
        Frame {
            width,
            height,
            format: PixelFormat::Bgra,
            data: pixel.repeat((width * height) as usize),
            #[cfg(windows)]
            texture: None,
        }
    }

    #[test]
    fn a_black_screen_is_black_and_one_with_anything_on_it_is_not() {
        assert_eq!(is_black(&bgra(64, 32, [0, 0, 0, 255])), Some(true));
        assert_eq!(is_black(&bgra(64, 32, [5, 3, 6, 255])), Some(true));
        assert_eq!(is_black(&bgra(64, 32, [60, 40, 30, 255])), Some(false));
        // A taskbar: a bright band across a black screen.
        let mut frame = bgra(64, 32, [0, 0, 0, 255]);
        let row = 64 * 4;
        frame.data[28 * row..].fill(200);
        assert_eq!(is_black(&frame), Some(false));
        // NV12 in memory: the luma plane decides (16 is black).
        let mut nv12 = vec![16u8; 64 * 32];
        nv12.extend(vec![128u8; 64 * 16]);
        let mut frame = Frame {
            format: PixelFormat::Nv12,
            data: nv12,
            ..bgra(64, 32, [0; 4])
        };
        assert_eq!(is_black(&frame), Some(true));
        frame.data[64 * 8 + 8] = 180;
        assert_eq!(is_black(&frame), Some(false));
    }

    fn at(x: i32, y: i32) -> Display {
        Display {
            id: format!("{x},{y}"),
            virtual_display: false,
            rect: Rect {
                x,
                y,
                width: 1920,
                height: 1080,
            },
        }
    }

    #[test]
    fn the_main_display_comes_first_then_left_to_right() {
        let list = numbered(vec![at(1920, 0), at(-1920, 200), at(0, 0), at(0, -1080)]);
        let order: Vec<_> = list.iter().map(|d| (d.rect.x, d.rect.y)).collect();
        assert_eq!(order, [(0, 0), (-1920, 200), (0, -1080), (1920, 0)]);
    }

    #[test]
    fn the_test_pattern_opens_each_display_where_it_is() {
        let list = displays(true).unwrap();
        assert!(list.len() > 1);
        for (index, display) in list.iter().enumerate() {
            let capturer = open(index, true, 30).unwrap();
            assert_eq!(capturer.rect(), display.rect);
        }
        assert!(matches!(
            open(list.len(), true, 30).err(),
            Some(CaptureError::NoSuchDisplay(_))
        ));
    }
}
