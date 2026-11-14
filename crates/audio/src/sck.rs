//! macOS: what the computer plays, through ScreenCaptureKit (macOS 13+).
//!
//! A stream of its own, apart from the screen's: it keeps going while the
//! technician switches displays. ScreenCaptureKit captures audio only with a
//! display in the filter, so the stream asks for the smallest picture, once a
//! second, and throws it away. It takes the permission the screen already
//! has (Gravação de Tela).

use std::ptr::NonNull;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSProcessInfo};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutputType,
};

use crate::capture::Chunks;
use crate::dsp::{Resampler, to_stereo};
use crate::{AudioError, SAMPLE_RATE};

/// How long to wait for ScreenCaptureKit to answer a request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Chunks waiting for the encoder: plenty.
const QUEUE: usize = 64;

/// `kAudioFormatFlagIsFloat` and `kAudioFormatFlagIsNonInterleaved`.
const FLAG_FLOAT: u32 = 1;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;

/// macOS 12 has ScreenCaptureKit, but no audio in it.
pub fn available() -> bool {
    NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
        >= 13
}

fn device_error(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// Where the delegate leaves the sound.
pub(crate) struct Sink {
    chunks: SyncSender<Vec<f32>>,
    /// For a rate other than the one asked for (never seen, but cheap).
    resampler: Mutex<Option<(u32, Resampler)>>,
}

impl Sink {
    /// Copies a delivered audio sample (on ScreenCaptureKit's queue).
    #[allow(unsafe_code)] // CoreMedia calls on the buffer ScreenCaptureKit hands over.
    fn store(&self, sample: &CMSampleBuffer) {
        let frames = unsafe { sample.num_samples() }.max(0) as usize;
        if frames == 0 {
            return;
        }
        let Some(format) = (unsafe { sample.format_description() }) else {
            return;
        };
        // SAFETY: an audio sample's format description; the pointer, if not
        // null, lives as long as `format`.
        let Some(asbd) =
            (unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref() })
        else {
            return;
        };
        let channels = asbd.mChannelsPerFrame as usize;
        if asbd.mFormatFlags & FLAG_FLOAT == 0 || asbd.mBitsPerChannel != 32 || channels == 0 {
            return;
        }
        let Some(block) = (unsafe { sample.data_buffer() }) else {
            return;
        };
        let len = frames * channels;
        if unsafe { block.data_length() } < len * 4 {
            return;
        }
        let mut raw = vec![0f32; len];
        let Some(dest) = NonNull::new(raw.as_mut_ptr().cast()) else {
            return;
        };
        // SAFETY: `raw` holds `len * 4` bytes, and the block at least that.
        if unsafe { block.copy_data_bytes(0, len * 4, dest) } != 0 {
            return;
        }
        let mut stereo = Vec::with_capacity(frames * 2);
        if asbd.mFormatFlags & FLAG_NON_INTERLEAVED != 0 {
            // One channel after the other.
            let right = if channels > 1 { frames } else { 0 };
            for (i, &left) in raw[..frames].iter().enumerate() {
                stereo.push(left);
                stereo.push(raw[right + i]);
            }
        } else {
            to_stereo(&raw, channels, &mut stereo);
        }
        let rate = asbd.mSampleRate.round() as u32;
        if rate != SAMPLE_RATE && rate > 0 {
            let mut resampler = self.resampler.lock().expect("lock");
            if resampler.as_ref().is_none_or(|(r, _)| *r != rate) {
                *resampler = Some((rate, Resampler::new(rate, SAMPLE_RATE, 2)));
            }
            let mut out = Vec::with_capacity(stereo.len() * 2);
            if let Some((_, resampler)) = resampler.as_mut() {
                resampler.process(&stereo, &mut out);
            }
            stereo = out;
        }
        let _ = self.chunks.try_send(stereo);
    }
}

#[allow(unsafe_code)] // An Objective-C class: ScreenCaptureKit's delegate.
mod output {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol};
    use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
    use objc2_core_media::CMSampleBuffer;
    use objc2_screen_capture_kit::{SCStream, SCStreamOutput, SCStreamOutputType};

    use super::Sink;

    pub struct Ivars {
        pub sink: Sink,
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; the class adds
        // no Drop-sensitive state beyond its ivars.
        #[unsafe(super(NSObject))]
        #[name = "HextAudioStreamOutput"]
        #[ivars = Ivars]
        pub struct AudioOutput;

        unsafe impl NSObjectProtocol for AudioOutput {}

        unsafe impl SCStreamOutput for AudioOutput {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output(
                &self,
                _stream: &SCStream,
                sample: &CMSampleBuffer,
                kind: SCStreamOutputType,
            ) {
                // (The pictures are only there for the sound to come.)
                if kind == SCStreamOutputType::Audio {
                    self.ivars().sink.store(sample);
                }
            }
        }
    );

    impl AudioOutput {
        pub fn new(sink: Sink) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { sink });
            unsafe { msg_send![super(this), init] }
        }
    }
}

use output::AudioOutput;

pub struct Loopback {
    stream: Retained<SCStream>,
    output: Retained<AudioOutput>,
}

impl Loopback {
    #[allow(unsafe_code)] // ScreenCaptureKit calls.
    pub fn open() -> Result<(Self, Chunks), AudioError> {
        if !CGPreflightScreenCaptureAccess() {
            return Err(device_error("sem a permissão de Gravação de Tela"));
        }
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        if displays.count() == 0 {
            return Err(device_error("nenhum monitor para o ScreenCaptureKit"));
        }
        let display = displays.objectAtIndex(0);
        let (tx, rx) = sync_channel(QUEUE);
        let output = AudioOutput::new(Sink {
            chunks: tx,
            resampler: Mutex::new(None),
        });
        let stream = unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setSampleRate(SAMPLE_RATE as isize);
            config.setChannelCount(2);
            // A viewer open on this computer is not part of its sound.
            config.setExcludesCurrentProcessAudio(true);
            config.setWidth(2);
            config.setHeight(2);
            config.setShowsCursor(false);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            );
            for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        ProtocolObject::from_ref(&*output),
                        kind,
                        None,
                    )
                    .map_err(|e| ns_error("addStreamOutput", &e))?;
            }
            stream
        };
        let loopback = Self { stream, output };
        // (Dropped on failure: stopped, never left running.)
        start(&loopback.stream)?;
        Ok((loopback, rx))
    }

    /// ScreenCaptureKit follows the device by itself.
    pub fn needs_reopen(&mut self) -> bool {
        false
    }
}

impl Drop for Loopback {
    #[allow(unsafe_code)] // ScreenCaptureKit calls.
    fn drop(&mut self) {
        let (tx, rx) = mpsc::channel();
        let handler = RcBlock::new(move |_error: *mut NSError| {
            let _ = tx.send(());
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&handler)) };
        let _ = rx.recv_timeout(REQUEST_TIMEOUT);
        for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen] {
            let _ = unsafe {
                self.stream
                    .removeStreamOutput_type_error(ProtocolObject::from_ref(&*self.output), kind)
            };
        }
    }
}

fn ns_error(context: &str, error: &NSError) -> AudioError {
    AudioError::Device(format!("{context}: {}", error.localizedDescription()))
}

/// Asks ScreenCaptureKit for the displays, waiting for its answer.
#[allow(unsafe_code)] // ScreenCaptureKit calls and the pointers its answer brings.
fn shareable_content() -> Result<Retained<SCShareableContent>, AudioError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(content),
                None => Err(match unsafe { error.as_ref() } {
                    Some(error) => ns_error("SCShareableContent", error),
                    None => device_error("SCShareableContent: sem resposta"),
                }),
            };
            let _ = tx.send(result);
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| device_error("SCShareableContent: tempo esgotado"))?
}

/// Starts the stream, waiting for ScreenCaptureKit to confirm.
#[allow(unsafe_code)] // ScreenCaptureKit calls and the error pointer its answer brings.
fn start(stream: &SCStream) -> Result<(), AudioError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |error: *mut NSError| {
        let result = match unsafe { error.as_ref() } {
            Some(error) => Err(ns_error("startCapture", error)),
            None => Ok(()),
        };
        let _ = tx.send(result);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&handler)) };
    rx.recv_timeout(REQUEST_TIMEOUT)
        .map_err(|_| device_error("startCapture: tempo esgotado"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records 2 s of what this Mac plays: play something while it runs.
    /// `cargo test -p hext-audio sck -- --ignored --nocapture`
    #[test]
    #[ignore = "needs the Screen Recording permission, and something playing"]
    fn sck_records_what_plays() {
        let (_loopback, chunks) = Loopback::open().expect("loopback");
        let until = std::time::Instant::now() + Duration::from_secs(2);
        let mut samples = Vec::new();
        while std::time::Instant::now() < until {
            if let Ok(chunk) = chunks.recv_timeout(Duration::from_millis(100)) {
                samples.extend(chunk);
            }
        }
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        println!("{} ms gravados, pico {peak:.4}", samples.len() / 2 / 48);
    }
}
