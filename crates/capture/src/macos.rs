//! ScreenCaptureKit capture (macOS 12.3+).
//!
//! An `SCStream` delivers a sample buffer whenever the screen changes; the
//! stream-output delegate copies the pixels into a [`Frame`] and wakes
//! [`Capturer::next_frame`]. The display is captured at its size in points
//! (e.g. 1920x1080 for a 4K "looks like 1080p" display): the GPU does the
//! downscale for free and the encoder gets a quarter of the pixels.
//! The cursor is not drawn into the image; the viewer shows its own.
//!
//! Frames come in NV12 (BT.601, limited range): the GPU converts while it
//! scales, so neither the CPU nor the video encoder has to, and the copy
//! out of the frame is 1.5 bytes per pixel instead of 4.
//!
//! Windows of this process can be left out of every capture
//! ([`exclude_windows`]): the privacy screen (hext-input) covers the Mac for
//! the person there, and the technician sees what is under it. The content
//! filter's `excludingWindows` and `SCStream::updateContentFilter` both come
//! with ScreenCaptureKit itself (macOS 12.3), so wherever this capture runs,
//! it can leave them out.

// Objective-C / CoreVideo calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_foundation::CFArray;
use objc2_core_graphics::{
    CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
    kCGDisplayStreamYCbCrMatrix_ITU_R_601_4,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType, SCWindow,
};

use crate::{CaptureError, Capturer, Display, Frame, PixelFormat, Rect, even};

/// How long to wait for ScreenCaptureKit to answer a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Widest image captured; bigger logical resolutions are scaled down.
const MAX_WIDTH: u32 = 2560;

fn platform(context: &str, error: &NSError) -> CaptureError {
    CaptureError::Platform(format!("{context}: {}", error.localizedDescription()))
}

/// The windows every capture leaves out, and the streams running, to change
/// their filter when that list changes. One lock for both: a stream either
/// opens with the new list or is running when it is applied.
struct Exclusion {
    windows: Vec<u32>,
    streams: Vec<Live>,
}

static EXCLUSION: Mutex<Exclusion> = Mutex::new(Exclusion {
    windows: Vec::new(),
    streams: Vec::new(),
});

/// Tells the running streams apart, to forget the one that stops.
static NEXT_STREAM: AtomicU64 = AtomicU64::new(0);

/// A running stream and the display it shows (its filter needs both).
struct Live {
    key: u64,
    stream: Retained<SCStream>,
    display: Retained<SCDisplay>,
}

// SAFETY: only used for `updateContentFilter`, which ScreenCaptureKit takes
// from any thread (it answers on its own queue), and to build the filter.
unsafe impl Send for Live {}

/// Leaves these windows (`CGWindowID`s) of this process out of every
/// capture, the running ones included; Ok once each running one confirmed.
/// An empty list only forgets the last one (its windows are gone by then).
pub fn exclude_windows(ids: &[u32]) -> Result<(), CaptureError> {
    if ids.is_empty() {
        EXCLUSION.lock().expect("lock").windows.clear();
        return Ok(());
    }
    let content = shareable_content()?;
    let windows = windows_of(&content, ids);
    if windows.count() != ids.len() {
        return Err(CaptureError::Platform(
            "a janela não aparece para o ScreenCaptureKit".into(),
        ));
    }
    let mut exclusion = EXCLUSION.lock().expect("lock");
    exclusion.windows = ids.to_vec();
    for live in &exclusion.streams {
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &live.display,
                &windows,
            )
        };
        update_filter(&live.stream, &filter)?;
    }
    Ok(())
}

/// The windows of `content` with these IDs (the ones still there).
fn windows_of(content: &SCShareableContent, ids: &[u32]) -> Retained<NSArray<SCWindow>> {
    if ids.is_empty() {
        return NSArray::new();
    }
    let all = unsafe { content.windows() };
    let picked: Vec<Retained<SCWindow>> = (0..all.count())
        .map(|i| all.objectAtIndex(i))
        .filter(|window| ids.contains(&unsafe { window.windowID() }))
        .collect();
    NSArray::from_retained_slice(&picked)
}

/// Stops changing the filter of stream `key`: it is stopping.
fn forget_stream(key: u64) {
    EXCLUSION
        .lock()
        .expect("lock")
        .streams
        .retain(|live| live.key != key);
}

/// Gives a running stream another filter, waiting for ScreenCaptureKit.
fn update_filter(stream: &SCStream, filter: &SCContentFilter) -> Result<(), CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let result = match unsafe { error.as_ref() } {
            Some(error) => Err(platform("updateContentFilter", error)),
            None => Ok(()),
        };
        let _ = tx.send(result);
    });
    unsafe { stream.updateContentFilter_completionHandler(filter, Some(&handler)) };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| CaptureError::Platform("updateContentFilter: tempo esgotado".into()))?
}

/// The newest frame from the delegate, picked up by `next_frame`.
#[derive(Default)]
struct Latest {
    frame: Option<Frame>,
    copy_time: Duration,
    /// Increases with every delivered frame.
    seq: u64,
    /// A buffer the consumer is done with, reused for the next copy.
    spare: Option<Vec<u8>>,
}

#[derive(Default)]
struct Shared {
    latest: Mutex<Latest>,
    ready: Condvar,
}

impl Shared {
    /// Copies a delivered sample (on ScreenCaptureKit's queue).
    fn store(&self, sample: &CMSampleBuffer) {
        if !is_complete(sample) {
            return; // idle frame: nothing changed
        }
        let started = Instant::now();
        let Some(pixels) = (unsafe { sample.image_buffer() }) else {
            return;
        };
        if CVPixelBufferGetPixelFormatType(&pixels) != PIXEL_FORMAT {
            return;
        }
        if unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return;
        }
        let (width, height) = (
            even(CVPixelBufferGetWidth(&pixels) as u32),
            even(CVPixelBufferGetHeight(&pixels) as u32),
        );
        let mut data = self
            .latest
            .lock()
            .expect("lock")
            .spare
            .take()
            .unwrap_or_default();
        data.clear();
        data.reserve(width as usize * height as usize * 3 / 2);
        // Y plane, then the half-height interleaved U/V plane; both `width`
        // bytes per row.
        let copied = copy_plane(&pixels, 0, width as usize, height as usize, &mut data)
            && copy_plane(&pixels, 1, width as usize, height as usize / 2, &mut data);
        unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
        if !copied {
            return;
        }

        let mut latest = self.latest.lock().expect("lock");
        latest.frame = Some(Frame {
            width,
            height,
            format: PixelFormat::Nv12,
            data,
        });
        latest.copy_time = started.elapsed();
        latest.seq += 1;
        self.ready.notify_all();
    }
}

/// Pixel format asked of ScreenCaptureKit: NV12, video (limited) range.
const PIXEL_FORMAT: u32 = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

/// Appends `rows` rows of `row_len` bytes from a plane of a locked buffer.
/// Returns false if the plane is missing or smaller than that.
fn copy_plane(
    pixels: &CVPixelBuffer,
    plane: usize,
    row_len: usize,
    rows: usize,
    out: &mut Vec<u8>,
) -> bool {
    let base = CVPixelBufferGetBaseAddressOfPlane(pixels, plane) as *const u8;
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pixels, plane);
    if base.is_null() || stride < row_len {
        return false;
    }
    for y in 0..rows {
        // SAFETY: the buffer is locked and this plane holds at least `rows`
        // rows of `stride >= row_len` bytes.
        let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), row_len) };
        out.extend_from_slice(row);
    }
    true
}

/// Whether ScreenCaptureKit marked the sample as a new, complete frame.
fn is_complete(sample: &CMSampleBuffer) -> bool {
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // CFArray of CFDictionary, toll-free bridged to NSArray of NSDictionary.
    let attachments: &CFArray = &attachments;
    let attachments = unsafe {
        &*(attachments as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    let Some(info) = attachments.firstObject() else {
        return true;
    };
    let Some(status) = info.objectForKey(unsafe { SCStreamFrameInfoStatus }) else {
        return true;
    };
    match status.downcast::<NSNumber>() {
        Ok(number) => number.integerValue() == SCFrameStatus::Complete.0,
        Err(_) => true,
    }
}

struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the class adds no
    // Drop-sensitive state beyond its ivars.
    #[unsafe(super(NSObject))]
    #[name = "HextScreenStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind == SCStreamOutputType::Screen {
                self.ivars().shared.store(sample);
            }
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct ScreenCaptureKitCapturer {
    stream: Retained<SCStream>,
    _output: Retained<StreamOutput>,
    shared: Arc<Shared>,
    frame: Option<Frame>,
    rect: Rect,
    copy_time: Duration,
    seen: u64,
    /// Its place among the streams [`exclude_windows`] reaches.
    key: u64,
}

/// macOS only re-reads the permission when the process starts, so a grant
/// made while the host runs needs a restart.
fn permission_error() -> CaptureError {
    CaptureError::PermissionDenied(
        "libere \"Gravação de Tela\" para o Terminal (ou o app que abriu o Hext Remote) \
         em Ajustes do Sistema → Privacidade e Segurança, feche o Terminal (Cmd+Q), \
         abra de novo e reinicie o host"
            .into(),
    )
}

/// Fails, after asking macOS to show its permission prompt, if this process
/// may not record the screen. Meant to run once, at startup.
pub fn preflight() -> Result<(), CaptureError> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // Shows the system prompt / adds the app to the settings list.
    CGRequestScreenCaptureAccess();
    Err(permission_error())
}

impl ScreenCaptureKitCapturer {
    /// Opens display `id` ([`Display::id`]).
    pub fn new(id: &str, max_fps: u32) -> Result<Self, CaptureError> {
        if !CGPreflightScreenCaptureAccess() {
            return Err(permission_error());
        }
        let content = shareable_content()?;
        let display = screens_of(&content)
            .into_iter()
            .find(|display| display_id(display) == id)
            .ok_or_else(|| CaptureError::DisplayGone(id.into()))?;

        let rect = rect_of(&display);
        // Size in points, scaled down further if very wide.
        let (mut width, mut height) =
            (
                unsafe { display.width() } as u32,
                unsafe { display.height() } as u32,
            );
        if width > MAX_WIDTH {
            height = height * MAX_WIDTH / width;
            width = MAX_WIDTH;
        }
        let (width, height) = (even(width.max(2)), even(height.max(2)));

        let shared = Arc::new(Shared::default());
        let output = StreamOutput::new(shared.clone());
        let key = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        // Held until the stream is listed: an exclusion asked meanwhile
        // waits, and then reaches this stream too.
        let mut exclusion = EXCLUSION.lock().expect("lock");
        let stream = unsafe {
            // (A window that closed in the meantime is simply not there.)
            let excluded = windows_of(&content, &exclusion.windows);
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &excluded,
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setPixelFormat(PIXEL_FORMAT);
            // The viewer's shader decodes BT.601, like the encoders produce.
            config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_601_4);
            config.setShowsCursor(false);
            config.setQueueDepth(3);
            config.setMinimumFrameInterval(CMTime::new(1, max_fps.clamp(1, 240) as i32));
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };
        exclusion.streams.push(Live {
            key,
            stream: stream.clone(),
            display: display.clone(),
        });
        drop(exclusion);
        let added = unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                None,
            )
        };
        if let Err(e) = added {
            forget_stream(key);
            return Err(platform("addStreamOutput", &e));
        }
        if let Err(e) = start(&stream) {
            // It may still start after the timeout: never leave it running.
            forget_stream(key);
            stop(&stream, &output);
            return Err(e);
        }
        tracing::info!(
            "capturando o monitor {id}: {}x{} pontos, imagem {width}x{height}",
            rect.width,
            rect.height
        );

        Ok(Self {
            stream,
            _output: output,
            shared,
            frame: None,
            rect,
            copy_time: Duration::ZERO,
            seen: 0,
            key,
        })
    }
}

/// The displays ScreenCaptureKit can capture, if this process may.
fn screens() -> Result<Vec<Retained<SCDisplay>>, CaptureError> {
    if !CGPreflightScreenCaptureAccess() {
        return Err(permission_error());
    }
    let content = shareable_content()?;
    Ok(screens_of(&content))
}

fn screens_of(content: &SCShareableContent) -> Vec<Retained<SCDisplay>> {
    let displays = unsafe { content.displays() };
    (0..displays.count())
        .map(|i| displays.objectAtIndex(i))
        .collect()
}

fn display_id(display: &SCDisplay) -> String {
    unsafe { display.displayID() }.to_string()
}

/// Where a display sits on the virtual desktop, in points, as input takes it.
fn rect_of(display: &SCDisplay) -> Rect {
    let bounds = unsafe { display.frame() };
    Rect {
        x: bounds.origin.x.round() as i32,
        y: bounds.origin.y.round() as i32,
        width: bounds.size.width.round() as u32,
        height: bounds.size.height.round() as u32,
    }
}

pub fn displays() -> Result<Vec<Display>, CaptureError> {
    Ok(screens()?
        .iter()
        .map(|display| Display {
            id: display_id(display),
            rect: rect_of(display),
            virtual_display: crate::virtual_display::is_virtual(&display_id(display)),
        })
        .collect())
}

impl Capturer for ScreenCaptureKitCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        let latest = self.shared.latest.lock().expect("lock");
        let (mut latest, _) = self
            .shared
            .ready
            .wait_timeout_while(latest, timeout, |l| l.seq == self.seen)
            .expect("lock");
        if latest.seq == self.seen {
            return Ok(false);
        }
        self.seen = latest.seq;
        self.copy_time = latest.copy_time;
        // Hand the previous buffer back for reuse.
        if let Some(old) = self.frame.take() {
            latest.spare = Some(old.data);
        }
        self.frame = latest.frame.take();
        Ok(self.frame.is_some())
    }

    fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    fn rect(&self) -> Rect {
        self.rect
    }

    fn copy_time(&self) -> Duration {
        self.copy_time
    }
}

impl Drop for ScreenCaptureKitCapturer {
    fn drop(&mut self) {
        forget_stream(self.key);
        stop(&self.stream, &self._output);
    }
}

/// Stops the stream and detaches the delegate, waiting (bounded) for
/// ScreenCaptureKit to confirm, so a quick reconnect never overlaps streams.
fn stop(stream: &SCStream, output: &StreamOutput) {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |_error: *mut NSError| {
        let _ = tx.send(());
    });
    unsafe { stream.stopCaptureWithCompletionHandler(Some(&handler)) };
    let _ = rx.recv_timeout(REQUEST_TIMEOUT);
    let _ = unsafe {
        stream.removeStreamOutput_type_error(
            ProtocolObject::from_ref(output),
            SCStreamOutputType::Screen,
        )
    };
}

/// Asks ScreenCaptureKit for the displays and windows, waiting for its
/// answer. Every window, also the ones not on screen yet: the privacy
/// screen's are left out before they show.
fn shareable_content() -> Result<Retained<SCShareableContent>, CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(content),
                None => Err(match unsafe { error.as_ref() } {
                    Some(error) => platform("SCShareableContent", error),
                    None => CaptureError::Platform("SCShareableContent: sem resposta".into()),
                }),
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            false, false, &handler,
        )
    };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| CaptureError::Platform("SCShareableContent: tempo esgotado".into()))?
}

/// Starts the stream, waiting for ScreenCaptureKit to confirm.
fn start(stream: &SCStream) -> Result<(), CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let result = match unsafe { error.as_ref() } {
            Some(error) => Err(platform("startCapture", error)),
            None => Ok(()),
        };
        let _ = tx.send(result);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| CaptureError::Platform("startCapture: tempo esgotado".into()))?
}
