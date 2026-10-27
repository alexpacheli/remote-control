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
use std::time::Instant;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, kCMTimeInvalid,
    kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_SMPTE_C, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_32BGRA, kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::h264::{self, PROFILE_BASELINE};
use crate::{
    CodecError, Encoded, EncoderSettings, VideoEncoder, check_frame, merge, profile_error,
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

pub struct VtEncoder {
    settings: EncoderSettings,
    /// Whether the last session opened got low-latency rate control.
    low_latency: bool,
    /// Recreated whenever the input size changes.
    session: Option<Session>,
    force_keyframe: bool,
}

/// Results delivered by [`on_output`], collected after each frame.
type Outputs = Mutex<Vec<Result<Option<Encoded>, CodecError>>>;

struct Session {
    width: u32,
    height: u32,
    session: CFRetained<VTCompressionSession>,
    /// Shared with the output callback through its refcon pointer: boxed so
    /// its address is stable, and outlives the session (see `Drop`).
    outputs: Box<Outputs>,
    started: Instant,
    /// Timestamp of the last frame, in [`TIMESCALE`] units.
    last_pts: Option<i64>,
    /// Minimum timestamp step: one frame at the configured frame rate.
    frame_step: i64,
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
        match Self::open(settings, width, height, true) {
            Ok(session) => Ok((session, true)),
            Err(e) => {
                tracing::debug!("VideoToolbox sem baixa latência em {width}x{height}: {e}");
                Ok((Self::open(settings, width, height, false)?, false))
            }
        }
    }

    fn open(
        settings: &EncoderSettings,
        width: u32,
        height: u32,
        low_latency: bool,
    ) -> Result<Self, CodecError> {
        let outputs: Box<Outputs> = Box::default();
        let yes: &CFType = CFBoolean::new(true);

        let mut spec_keys =
            vec![unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }];
        if low_latency {
            spec_keys.push(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl });
        }
        let spec_values = vec![yes; spec_keys.len()];
        let spec = CFDictionary::<CFString, CFType>::from_slices(&spec_keys, &spec_values);

        // The pool hands out IOSurface-backed BGRA buffers the media engine
        // reads directly.
        let format = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
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
            session,
            outputs,
            started: Instant::now(),
            last_pts: None,
            frame_step: i64::from(TIMESCALE / fps),
        };
        this.configure(settings, fps)?;
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

    fn encode(&mut self, bgra: &[u8], keyframe: bool) -> Result<Option<Encoded>, CodecError> {
        let pixels = self.pixel_buffer()?;
        copy_into(&pixels, self.width, self.height, bgra)?;

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
        // Returns once the callback has run for every frame up to this one.
        let status = unsafe { self.session.complete_frames(pts) };
        if status != 0 {
            return Err(status_error("CompleteFrames", status));
        }

        let outputs = std::mem::take(&mut *self.outputs.lock().expect("lock"));
        let mut ready = None;
        for output in outputs {
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

/// Copies tightly packed BGRA rows into a (possibly padded) pixel buffer.
fn copy_into(
    pixels: &CVPixelBuffer,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<(), CodecError> {
    let (width, height) = (width as usize, height as usize);
    let row_len = width * 4;
    if CVPixelBufferGetWidth(pixels) < width || CVPixelBufferGetHeight(pixels) < height {
        return Err(CodecError::Hardware(
            "pixel buffer menor que a imagem".into(),
        ));
    }
    let status = unsafe { CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags(0)) };
    if status != kCVReturnSuccess {
        return Err(status_error("CVPixelBufferLockBaseAddress", status));
    }
    let stride = CVPixelBufferGetBytesPerRow(pixels);
    let base = CVPixelBufferGetBaseAddress(pixels).cast::<u8>();
    let ok = !base.is_null() && stride >= row_len;
    if ok {
        for (y, row) in bgra.chunks_exact(row_len).take(height).enumerate() {
            // SAFETY: the buffer is locked and holds at least `height` rows of
            // `stride >= row_len` bytes.
            unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), base.add(y * stride), row_len) };
        }
    }
    unsafe { CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags(0)) };
    if ok {
        Ok(())
    } else {
        Err(CodecError::Hardware("pixel buffer inacessível".into()))
    }
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
        outputs.push(result);
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
