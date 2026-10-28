//! Hardware H.264 encoding through VideoToolbox (macOS).
//!
//! A `VTCompressionSession` runs on the Mac's media engine (Apple silicon, or
//! the Intel GPU / T2 chip on Intel Macs). Frames go in as BGRA, copied once
//! into an IOSurface-backed pixel buffer from the session's own pool; the
//! media engine converts them to YUV itself, so the CPU does no colour
//! conversion. Handing over the captured IOSurface directly (zero-copy) can
//! come later.
//!
//! Low-latency rate control (macOS 11.3+) is requested when available: no
//! frame reordering and each frame's output right after its input, which
//! `VTCompressionSessionCompleteFrames` then collects synchronously.
//!
//! The output is AVCC (length-prefixed NAL units, SPS/PPS in the format
//! description). It is turned into Annex-B with SPS/PPS before every
//! keyframe, like the other encoders produce. The decoder on the other side
//! only handles Baseline, so the session asks for (Constrained) Baseline and
//! every SPS is checked: if the encoder ignores the request, it fails here
//! and `Auto` mode falls back to software.

// VideoToolbox / CoreFoundation calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool,
    CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_SMPTE_C,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::h264::{self, PROFILE_BASELINE};
use crate::nv12::bgra_to_nv12;
use crate::{
    CodecError, Encoded, EncoderSettings, SoftwareEncoder, VideoEncoder, check_frame, merge,
    profile_error,
};

/// A keyframe at least every minute (frames are only encoded when the screen
/// changes, so this is a duration, not a frame count), so a lost one can
/// never stall the picture for long even if the viewer's request goes missing.
const KEYFRAME_INTERVAL_SECS: f64 = 60.0;

/// Presentation timestamps are in microseconds.
const TIMESCALE: i32 = 1_000_000;

/// Size of the session opened at startup to find out whether there is a
/// hardware encoder at all (kept for the first frame if the size matches).
const PROBE_SIZE: (u32, u32) = (640, 480);

fn status_error(context: &str, status: i32) -> CodecError {
    CodecError::Hardware(format!("{context}: OSStatus {status}"))
}

/// Session options that trade speed against other things; the host uses
/// [`Tuning::HOST`], and [`benchmark`] compares the alternatives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tuning {
    /// Low-latency rate control.
    low_latency: bool,
    /// Input in NV12 (converted beforehand) instead of BGRA (converted by
    /// VideoToolbox).
    nv12: bool,
    /// `PrioritizeEncodingSpeedOverQuality`.
    speed: bool,
}

impl Tuning {
    const HOST: Self = Self {
        low_latency: true,
        nv12: false,
        speed: false,
    };
}

pub struct VtEncoder {
    settings: EncoderSettings,
    /// Whether the last session opened got low-latency rate control.
    low_latency: bool,
    /// Recreated whenever the input size changes.
    session: Option<Session>,
    force_keyframe: bool,
}

/// What [`on_output`] delivered since the last frame was collected.
#[derive(Default)]
struct Delivered {
    results: Vec<Result<Option<Encoded>, CodecError>>,
    /// When the callback last ran (for the timing breakdown).
    at: Option<Instant>,
}

type Outputs = Mutex<Delivered>;

/// How often the per-stage timing breakdown is logged (at debug level).
const TIMING_PERIOD: Duration = Duration::from_secs(2);

/// Per-stage encode times, summed over [`TIMING_PERIOD`], to find where a
/// frame's time goes (`RUST_LOG=hext_codec=debug`).
#[derive(Default)]
struct Timings {
    frames: u32,
    /// Getting a pixel buffer from the pool.
    buffer: Duration,
    /// Lock, copy of the BGRA rows, unlock.
    copy: Duration,
    /// `EncodeFrame` call.
    submit: Duration,
    /// `CompleteFrames` call.
    wait: Duration,
    /// From the start of `EncodeFrame` to the output callback.
    output: Duration,
    since: Option<Instant>,
}

impl Timings {
    fn record(&mut self, stages: [Duration; 5]) {
        let [buffer, copy, submit, wait, output] = stages;
        self.frames += 1;
        self.buffer += buffer;
        self.copy += copy;
        self.submit += submit;
        self.wait += wait;
        self.output += output;
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < TIMING_PERIOD {
            return;
        }
        let avg = |d: Duration| d.as_secs_f64() * 1000.0 / f64::from(self.frames);
        tracing::debug!(
            "VideoToolbox, média de {} quadros: buffer {:.2} ms | cópia {:.2} ms | \
             EncodeFrame {:.2} ms | CompleteFrames {:.2} ms | saída após {:.2} ms",
            self.frames,
            avg(self.buffer),
            avg(self.copy),
            avg(self.submit),
            avg(self.wait),
            avg(self.output),
        );
        *self = Self::default();
    }
}

struct Session {
    width: u32,
    height: u32,
    /// Input is NV12 rather than BGRA.
    nv12: bool,
    session: CFRetained<VTCompressionSession>,
    /// Shared with the output callback through its refcon pointer: boxed so
    /// its address is stable, and outlives the session (see `Drop`).
    outputs: Box<Outputs>,
    started: Instant,
    /// Timestamp of the last frame, in [`TIMESCALE`] units.
    last_pts: Option<i64>,
    /// Minimum timestamp step: one frame at the configured frame rate.
    frame_step: i64,
    timings: Timings,
}

impl VtEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        // Fails right away (so `Auto` picks software) if there is no
        // hardware encoder or it refuses the settings.
        let (width, height) = PROBE_SIZE;
        let (session, low_latency) = Session::open_best(&settings, width, height)?;
        let encoder = Self {
            settings,
            low_latency,
            session: Some(session),
            force_keyframe: false,
        };
        tracing::info!("codificador por hardware: {}", encoder.description());
        Ok(encoder)
    }

    fn description(&self) -> &'static str {
        if self.low_latency {
            "VideoToolbox H.264, low latency"
        } else {
            "VideoToolbox H.264"
        }
    }
}

impl VideoEncoder for VtEncoder {
    fn encode(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Option<Encoded>, CodecError> {
        check_frame(width, height, bgra)?;
        if !matches!(&self.session, Some(s) if (s.width, s.height) == (width, height)) {
            self.session = None;
            // A new session starts with a keyframe anyway.
            self.force_keyframe = false;
            let (session, low_latency) = Session::open_best(&self.settings, width, height)?;
            self.session = Some(session);
            self.low_latency = low_latency;
        }

        let keyframe = std::mem::take(&mut self.force_keyframe);
        let session = self.session.as_mut().expect("session opened above");
        let result = session.encode(bgra, keyframe);
        match &result {
            // Start over (with a keyframe) if the caller keeps using us.
            Err(_) => self.session = None,
            // Rate control dropped the frame: the keyframe is still owed.
            Ok(encoded) if keyframe && !encoded.as_ref().is_some_and(|e| e.keyframe) => {
                self.force_keyframe = true;
            }
            Ok(_) => {}
        }
        result
    }

    fn force_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn name(&self) -> String {
        format!("GPU ({})", self.description())
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Session {
    /// Opens a session with low-latency rate control, or without it if the
    /// media engine refuses (some Macs, or sizes beyond its limits).
    /// Returns whether low latency is on.
    fn open_best(
        settings: &EncoderSettings,
        width: u32,
        height: u32,
    ) -> Result<(Self, bool), CodecError> {
        match Self::open(settings, width, height, Tuning::HOST) {
            Ok(session) => Ok((session, true)),
            Err(e) => {
                tracing::debug!("VideoToolbox sem baixa latência em {width}x{height}: {e}");
                let tuning = Tuning {
                    low_latency: false,
                    ..Tuning::HOST
                };
                Ok((Self::open(settings, width, height, tuning)?, false))
            }
        }
    }

    fn open(
        settings: &EncoderSettings,
        width: u32,
        height: u32,
        tuning: Tuning,
    ) -> Result<Self, CodecError> {
        let outputs: Box<Outputs> = Box::default();
        let yes: &CFType = CFBoolean::new(true);

        let mut spec_keys =
            vec![unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }];
        if tuning.low_latency {
            spec_keys.push(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl });
        }
        let spec_values = vec![yes; spec_keys.len()];
        let spec = CFDictionary::<CFString, CFType>::from_slices(&spec_keys, &spec_values);

        // The pool hands out IOSurface-backed buffers the media engine reads
        // directly.
        let pixel_format = if tuning.nv12 {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        } else {
            kCVPixelFormatType_32BGRA
        };
        let format = CFNumber::new_i32(pixel_format as i32);
        let (w, h) = (
            CFNumber::new_i32(width as i32),
            CFNumber::new_i32(height as i32),
        );
        let iosurface = CFDictionary::<CFString, CFType>::empty();
        let source = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferWidthKey,
                    kCVPixelBufferHeightKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&format, &w, &h, &iosurface],
            )
        };

        let mut raw: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                width as i32,
                height as i32,
                kCMVideoCodecType_H264,
                Some(spec.as_opaque()),
                Some(source.as_opaque()),
                None,
                Some(on_output),
                std::ptr::from_ref::<Outputs>(&outputs).cast_mut().cast(),
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            // kVTCouldNotFindVideoEncoderErr: no hardware H.264 encoder here.
            return Err(if status == -12908 {
                CodecError::NoHardware
            } else {
                status_error("VTCompressionSessionCreate", status)
            });
        };
        // SAFETY: a successful Create returns a +1 reference.
        let session = unsafe { CFRetained::from_raw(raw) };
        // From here on, dropping `this` invalidates the session before the
        // outputs it points to are freed.
        let fps = settings.max_fps.round().max(1.0) as i32;
        let this = Self {
            width,
            height,
            nv12: tuning.nv12,
            session,
            outputs,
            started: Instant::now(),
            last_pts: None,
            frame_step: i64::from(TIMESCALE / fps),
            timings: Timings::default(),
        };
        this.configure(settings, fps)?;
        if tuning.speed {
            this.set(
                unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
                CFBoolean::new(true),
            );
        }
        let status = unsafe { this.session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(status_error("PrepareToEncodeFrames", status));
        }
        Ok(this)
    }

    fn configure(&self, settings: &EncoderSettings, fps: i32) -> Result<(), CodecError> {
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        unsafe {
            // The profile is required: the viewer decodes nothing else.
            let profile_set = self.set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
            ) || self.set(
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_Baseline_AutoLevel,
            );
            if !profile_set {
                return Err(CodecError::Hardware(
                    "o encoder recusou o perfil Baseline".into(),
                ));
            }

            // The rest is best effort, as encoders support different subsets.
            self.set(kVTCompressionPropertyKey_RealTime, yes);
            self.set(kVTCompressionPropertyKey_AllowFrameReordering, no);
            self.set(
                kVTCompressionPropertyKey_AverageBitRate,
                &CFNumber::new_i64(i64::from(settings.bitrate_bps)),
            );
            self.set(
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &CFNumber::new_i32(fps),
            );
            self.set(
                kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                &CFNumber::new_f64(KEYFRAME_INTERVAL_SECS),
            );
            // Each frame's output before the next input.
            self.set(
                kVTCompressionPropertyKey_MaxFrameDelayCount,
                &CFNumber::new_i32(0),
            );
            // The viewer's shader expects BT.601 limited range, as from
            // OpenH264 and Media Foundation.
            self.set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            );
            self.set(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_SMPTE_C,
            );
            self.set(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            );
        }
        Ok(())
    }

    /// Sets a session property; returns whether the encoder accepted it.
    fn set(&self, key: &CFString, value: &CFType) -> bool {
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            tracing::debug!("VideoToolbox: propriedade {key} não suportada ({status})");
        }
        status == 0
    }

    /// Encodes one image: BGRA, or NV12 if the session was opened for it.
    fn encode(&mut self, image: &[u8], keyframe: bool) -> Result<Option<Encoded>, CodecError> {
        let started = Instant::now();
        let pixels = self.pixel_buffer()?;
        let buffered = Instant::now();
        copy_into(&pixels, self.width, self.height, image, self.nv12)?;
        let copied = Instant::now();

        let pts = unsafe { CMTime::new(self.next_pts(), TIMESCALE) };
        let yes: &CFType = CFBoolean::new(true);
        let options = keyframe.then(|| {
            CFDictionary::<CFString, CFType>::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[yes],
            )
        });
        let mut flags = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                &pixels,
                pts,
                kCMTimeInvalid,
                options.as_deref().map(CFDictionary::as_opaque),
                std::ptr::null_mut(),
                &mut flags,
            )
        };
        if status != 0 {
            return Err(status_error("EncodeFrame", status));
        }
        let submitted = Instant::now();
        // Returns once the callback has run for every frame up to this one.
        let status = unsafe { self.session.complete_frames(pts) };
        if status != 0 {
            return Err(status_error("CompleteFrames", status));
        }
        let completed = Instant::now();

        let delivered = std::mem::take(&mut *self.outputs.lock().expect("lock"));
        let output_at = delivered.at.unwrap_or(completed);
        self.timings.record([
            buffered - started,
            copied - buffered,
            submitted - copied,
            completed - submitted,
            output_at.saturating_duration_since(copied),
        ]);
        let mut ready = None;
        for output in delivered.results {
            merge(&mut ready, output?);
        }
        Ok(ready)
    }

    fn pixel_buffer(&self) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
        let pool = unsafe { self.session.pixel_buffer_pool() }
            .ok_or_else(|| CodecError::Hardware("sem pool de pixel buffers".into()))?;
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        let status =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut raw)) };
        match NonNull::new(raw) {
            // SAFETY: a successful Create returns a +1 reference.
            Some(raw) if status == kCVReturnSuccess => Ok(unsafe { CFRetained::from_raw(raw) }),
            _ => Err(status_error("CVPixelBufferPoolCreatePixelBuffer", status)),
        }
    }

    /// Real time since the session started, so rate control sees the actual
    /// gaps of a screen that changes now and then, but never closer together
    /// than one frame at the configured rate (the host never sends faster).
    fn next_pts(&mut self) -> i64 {
        let now = i64::try_from(self.started.elapsed().as_micros()).unwrap_or(i64::MAX);
        let pts = match self.last_pts {
            Some(last) => now.max(last + self.frame_step),
            None => now,
        };
        self.last_pts = Some(pts);
        pts
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Flush, then tear down: after this the callback no longer runs, so
        // `outputs` (dropped after this) is never used again.
        unsafe {
            let _ = self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
        }
    }
}

/// Copies a tightly packed BGRA or NV12 image into a (possibly padded)
/// pixel buffer of the same format.
fn copy_into(
    pixels: &CVPixelBuffer,
    width: u32,
    height: u32,
    image: &[u8],
    nv12: bool,
) -> Result<(), CodecError> {
    let (width, height) = (width as usize, height as usize);
    if CVPixelBufferGetWidth(pixels) < width || CVPixelBufferGetHeight(pixels) < height {
        return Err(CodecError::Hardware(
            "pixel buffer menor que a imagem".into(),
        ));
    }
    let status = unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags(0)) };
    if status != kCVReturnSuccess {
        return Err(status_error("CVPixelBufferLockBaseAddress", status));
    }
    let ok = if nv12 {
        // Y plane, then the half-height interleaved UV plane.
        let (y, uv) = image.split_at(width * height);
        copy_plane(pixels, Some(0), y, width, height)
            && copy_plane(pixels, Some(1), uv, width, height / 2)
    } else {
        copy_plane(pixels, None, image, width * 4, height)
    };
    unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags(0)) };
    if ok {
        Ok(())
    } else {
        Err(CodecError::Hardware("pixel buffer inacessível".into()))
    }
}

/// Copies `rows` rows of `row_len` bytes into a locked buffer's plane (or
/// the whole buffer for a non-planar format). Returns false if it won't fit.
fn copy_plane(
    pixels: &CVPixelBuffer,
    plane: Option<usize>,
    src: &[u8],
    row_len: usize,
    rows: usize,
) -> bool {
    let (base, stride) = match plane {
        Some(plane) => (
            CVPixelBufferGetBaseAddressOfPlane(pixels, plane),
            CVPixelBufferGetBytesPerRowOfPlane(pixels, plane),
        ),
        None => (
            CVPixelBufferGetBaseAddress(pixels),
            CVPixelBufferGetBytesPerRow(pixels),
        ),
    };
    let base = base.cast::<u8>();
    if base.is_null() || stride < row_len || src.len() < row_len * rows {
        return false;
    }
    for (y, row) in src.chunks_exact(row_len).take(rows).enumerate() {
        // SAFETY: the buffer is locked and this plane holds at least `rows`
        // rows of `stride >= row_len` bytes.
        unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), base.add(y * stride), row_len) };
    }
    true
}

/// Called by VideoToolbox for each encoded (or dropped) frame; with
/// `CompleteFrames` this happens before `encode` returns.
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the refcon is the session's boxed `Outputs`, which outlives the
    // compression session (invalidated first in `Session::drop`).
    let outputs = unsafe { &*refcon.cast_const().cast::<Outputs>() };
    let result = if status != 0 {
        Err(status_error("encode", status))
    } else {
        // SAFETY: VideoToolbox passes a valid sample buffer, or NULL.
        match unsafe { sample.as_ref() } {
            // Dropped by rate control: the caller encodes the next image again.
            None => Ok(None),
            Some(_) if flags.0 & VTEncodeInfoFlags::FrameDropped.0 != 0 => Ok(None),
            Some(sample) => annex_b(sample).map(Some),
        }
    };
    // Never unwind into VideoToolbox: a poisoned lock just loses the frame.
    if let Ok(mut outputs) = outputs.lock() {
        outputs.results.push(result);
        outputs.at = Some(Instant::now());
    }
}

/// Turns an encoded sample (AVCC) into an Annex-B access unit, with SPS/PPS
/// in front of keyframes.
fn annex_b(sample: &CMSampleBuffer) -> Result<Encoded, CodecError> {
    let missing = |what: &str| CodecError::Hardware(format!("amostra sem {what}"));
    let format = unsafe { sample.format_description() }.ok_or_else(|| missing("formato"))?;
    let block = unsafe { sample.data_buffer() }.ok_or_else(|| missing("dados"))?;

    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    if len > 0 {
        let status = unsafe {
            block.copy_data_bytes(0, len, NonNull::new_unchecked(avcc.as_mut_ptr()).cast())
        };
        if status != 0 {
            return Err(status_error("CMBlockBufferCopyDataBytes", status));
        }
    }

    // Number of parameter sets and the size of each NAL length prefix.
    let mut count = 0usize;
    let mut length_size: c_int = 4;
    let status = unsafe {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            &format,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut length_size,
        )
    };
    if status != 0 {
        return Err(status_error("parameter sets", status));
    }

    let mut body = Vec::with_capacity(len + 16);
    h264::avcc_to_annex_b(&avcc, length_size as usize, &mut body)
        .ok_or_else(|| CodecError::Hardware("NAL units malformadas".into()))?;
    let scan = h264::scan(&body);
    if !scan.has_idr || scan.has_sps {
        if let Some(profile) = scan.profile
            && profile != PROFILE_BASELINE
        {
            return Err(profile_error(profile));
        }
        return Ok(Encoded {
            data: body,
            keyframe: scan.has_idr,
        });
    }

    // A keyframe without inline SPS/PPS: put them in front so the viewer can
    // start (or restart) decoding here.
    let mut data = Vec::with_capacity(body.len() + 64);
    for index in 0..count {
        let mut ptr: *const u8 = std::ptr::null();
        let mut size = 0usize;
        let status = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &format,
                index,
                &mut ptr,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || ptr.is_null() {
            return Err(status_error("parameter set", status));
        }
        data.extend_from_slice(&[0, 0, 0, 1]);
        // SAFETY: points to `size` bytes inside `format`, which is alive.
        data.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, size) });
    }
    // Checked on every keyframe, so a profile switch later on is caught too.
    match h264::sps_profile(&data) {
        Some(PROFILE_BASELINE) => {}
        Some(profile) => return Err(profile_error(profile)),
        None => return Err(missing("SPS")),
    }
    data.extend_from_slice(&body);
    Ok(Encoded {
        data,
        keyframe: true,
    })
}

/// Encode times of one configuration in [`benchmark`].
#[derive(Debug, Clone, Copy)]
pub struct BenchStats {
    /// Frames that produced output (after the warm-up).
    pub frames: u32,
    pub avg_ms: f64,
    pub p95_ms: f64,
    /// Average encoded size.
    pub avg_kb: f64,
}

/// Frames at the start of each configuration left out of the stats (the
/// first keyframe, and the media engine ramping up its clock).
const BENCH_WARMUP: u32 = 10;

/// Encodes the same images with OpenH264 and with each VideoToolbox
/// configuration, paced at `settings.max_fps` like the host, and reports
/// how long one `encode` takes in each.
///
/// `image_at(t, buffer)` writes frame `t` (tightly packed BGRA) into
/// `buffer`; producing it, and the NV12 conversion where needed, are not
/// counted.
pub fn benchmark(
    settings: EncoderSettings,
    width: u32,
    height: u32,
    frames: u32,
    mut image_at: impl FnMut(u32, &mut Vec<u8>),
) -> Vec<(String, Result<BenchStats, CodecError>)> {
    let mut results = Vec::new();
    let mut bgra = Vec::new();
    let mut nv12 = Vec::new();
    let mut yuv = None;
    let interval = Duration::from_secs_f32(1.0 / settings.max_fps.max(1.0));

    let mut software = SoftwareEncoder::new(settings);
    let stats = measure(frames, interval, |t| {
        image_at(t, &mut bgra);
        let started = Instant::now();
        let encoded = software.encode(width, height, &bgra)?;
        Ok((started.elapsed(), encoded))
    });
    results.push(("CPU (OpenH264), BGRA".to_owned(), stats));

    for low_latency in [true, false] {
        for nv12_input in [false, true] {
            for speed in [false, true] {
                let tuning = Tuning {
                    low_latency,
                    nv12: nv12_input,
                    speed,
                };
                let label = format!(
                    "GPU, {}, {}{}",
                    if nv12_input { "NV12" } else { "BGRA" },
                    if low_latency {
                        "baixa latência"
                    } else {
                        "normal"
                    },
                    if speed { ", velocidade" } else { "" },
                );
                let stats =
                    Session::open(&settings, width, height, tuning).and_then(|mut session| {
                        measure(frames, interval, |t| {
                            image_at(t, &mut bgra);
                            let image = if nv12_input {
                                bgra_to_nv12(width, height, &bgra, &mut yuv, &mut nv12);
                                &nv12
                            } else {
                                &bgra
                            };
                            let started = Instant::now();
                            let encoded = session.encode(image, t == 0)?;
                            Ok((started.elapsed(), encoded))
                        })
                    });
                results.push((label, stats));
            }
        }
    }
    results
}

/// Runs `encode_frame(t)` for `frames` frames, one every `interval`, and
/// summarizes the times it reports.
fn measure(
    frames: u32,
    interval: Duration,
    mut encode_frame: impl FnMut(u32) -> Result<(Duration, Option<Encoded>), CodecError>,
) -> Result<BenchStats, CodecError> {
    let start = Instant::now();
    let mut times = Vec::new();
    let mut bytes = 0usize;
    for t in 0..frames {
        let due = start + interval * t;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let (time, encoded) = encode_frame(t)?;
        if let Some(encoded) = encoded
            && t >= BENCH_WARMUP
        {
            times.push(time);
            bytes += encoded.data.len();
        }
    }
    if times.is_empty() {
        return Err(CodecError::Hardware("nenhum quadro codificado".into()));
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let n = times.len();
    Ok(BenchStats {
        frames: n as u32,
        avg_ms: ms(times.iter().sum::<Duration>()) / n as f64,
        p95_ms: ms(times[(n * 95 / 100).min(n - 1)]),
        avg_kb: bytes as f64 / 1024.0 / n as f64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::H264Decoder;
    use crate::tests::image;

    /// CI machines may have no media engine (virtual Macs): skip there.
    fn encoder() -> Option<VtEncoder> {
        match VtEncoder::new(EncoderSettings::default()) {
            Ok(encoder) => Some(encoder),
            Err(e) => {
                eprintln!("VideoToolbox indisponível, teste ignorado: {e}");
                None
            }
        }
    }

    #[test]
    fn hardware_roundtrip() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        let mut decoder = H264Decoder::new().unwrap();
        let (w, h) = (640, 360);
        let mut decoded = 0;
        for t in 0..20 {
            if t == 10 {
                encoder.force_keyframe();
            }
            let Some(encoded) = encoder.encode(w, h, &image(w, h, t)).unwrap() else {
                continue; // dropped by rate control
            };
            if t == 0 || t == 10 {
                assert!(encoded.keyframe, "frame {t} should be a keyframe");
                let scan = h264::scan(&encoded.data);
                assert!(scan.has_sps && scan.has_idr, "frame {t}: {scan:?}");
                assert_eq!(scan.profile, Some(PROFILE_BASELINE));
            }
            if let Some(frame) = decoder.decode(&encoded.data).unwrap() {
                assert_eq!((frame.width, frame.height), (w, h));
                // BT.601 limited range: background 40 → ~51.
                let corner = frame.y[(h as usize - 1) * w as usize];
                assert!((35..=70).contains(&corner), "luma {corner}");
                decoded += 1;
            }
        }
        assert!(decoded >= 15, "decoded {decoded} of 20");

        // A new size starts a new stream with a keyframe.
        let encoded = encoder
            .encode(320, 240, &image(320, 240, 0))
            .unwrap()
            .unwrap();
        assert!(encoded.keyframe);
        assert!(h264::scan(&encoded.data).has_sps);
    }

    #[test]
    fn colours_are_bt601() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        let (w, h) = (320, 240);
        let red: Vec<u8> = [0, 0, 255, 255].repeat((w * h) as usize);
        let encoded = encoder.encode(w, h, &red).unwrap().unwrap();
        let frame = H264Decoder::new()
            .unwrap()
            .decode(&encoded.data)
            .unwrap()
            .unwrap();
        let (w, h) = (w as usize, h as usize);
        // Centre pixel; chroma planes are half size in each direction.
        let y = frame.y[h / 2 * w + w / 2];
        let v = frame.v[h / 4 * (w / 2) + w / 4];
        // BT.601 limited range: Y≈81, V≈240 (BT.709 would give Y≈63).
        assert!((75..=88).contains(&y), "red Y {y}");
        assert!(v > 200, "red V {v}");
    }

    #[test]
    fn rejects_bad_input() {
        let Some(mut encoder) = encoder() else {
            return;
        };
        assert!(encoder.encode(63, 64, &[0; 63 * 64 * 4]).is_err());
        assert!(encoder.encode(64, 64, &[0; 10]).is_err());
    }
}
