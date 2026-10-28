//! Hardware H.264 decoding through VideoToolbox (macOS).
//!
//! A `VTDecompressionSession` on the Mac's media engine, required to be
//! hardware. The Annex-B access units the host sends are turned into AVCC
//! (length-prefixed NAL units) with SPS/PPS moved into the session's format
//! description, which is recreated whenever they change. Frames come out as
//! NV12 in video (limited) range, the bitstream's own values, and are
//! copied into an I420 [`YuvFrame`] for the renderer.

// VideoToolbox / CoreMedia calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVReturnSuccess,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSessionSetProperty, kVTDecompressionPropertyKey_RealTime,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};

use crate::h264::{self, NAL_AUD, NAL_PPS, NAL_SPS};
use crate::{CodecError, VideoDecoder, YuvFrame};

/// SPS and PPS of a 64x64 Baseline stream (from OpenH264), to check for a
/// hardware decoder before the first real keyframe arrives.
const PROBE_SPS: [u8; 12] = [103, 66, 192, 30, 140, 104, 66, 104, 7, 132, 66, 53];
const PROBE_PPS: [u8; 4] = [104, 206, 60, 128];

fn status_error(context: &str, status: i32) -> CodecError {
    CodecError::Hardware(format!("{context}: OSStatus {status}"))
}

/// What [`on_output`] produced for the frame being decoded.
type Output = Mutex<Option<Result<YuvFrame, CodecError>>>;

pub struct VtDecoder {
    /// Opened at the first SPS/PPS, reopened when they change.
    session: Option<Session>,
}

struct Session {
    sps: Vec<u8>,
    pps: Vec<u8>,
    format: CFRetained<CMFormatDescription>,
    session: CFRetained<VTDecompressionSession>,
    /// Shared with the output callback through its refcon pointer: boxed so
    /// its address is stable, and outlives the session (see `Drop`).
    output: Box<Output>,
}

impl VtDecoder {
    pub fn new() -> Result<Self, CodecError> {
        // A session needs a stream's parameter sets; check right away that
        // there is a hardware decoder, with a small Baseline stream's.
        Session::open(PROBE_SPS.to_vec(), PROBE_PPS.to_vec())?;
        tracing::info!("decodificador por hardware: VideoToolbox");
        Ok(Self { session: None })
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        let mut sps = None;
        let mut pps = None;
        let mut avcc = Vec::with_capacity(data.len() + 16);
        for nal in h264::nal_units(data) {
            match h264::nal_type(nal) {
                Some(NAL_SPS) => sps = Some(nal),
                Some(NAL_PPS) => pps = Some(nal),
                // Carries nothing the decoder needs.
                Some(NAL_AUD) | None => {}
                Some(_) => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
            }
        }

        // New parameter sets (first keyframe, or a new stream): reopen. One
        // may come without the other; the current session's fills the gap.
        let current = self.session.as_ref();
        let sps = sps.or(current.map(|s| s.sps.as_slice()));
        let pps = pps.or(current.map(|s| s.pps.as_slice()));
        if let (Some(sps), Some(pps)) = (sps, pps)
            && !matches!(current, Some(s) if s.sps == sps && s.pps == pps)
        {
            let (sps, pps) = (sps.to_vec(), pps.to_vec());
            self.session = None;
            self.session = Some(Session::open(sps, pps)?);
        }
        let Some(session) = &mut self.session else {
            return Err(CodecError::InvalidFrame(
                "quadro antes do primeiro keyframe (sem SPS/PPS)".into(),
            ));
        };
        if avcc.is_empty() {
            return Ok(None);
        }
        let result = session.decode(&avcc);
        if result.is_err() {
            // Start over at the next keyframe.
            self.session = None;
        }
        result
    }

    fn name(&self) -> String {
        "GPU (VideoToolbox)".to_owned()
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Session {
    fn open(sps: Vec<u8>, pps: Vec<u8>) -> Result<Self, CodecError> {
        let format = format_description(&sps, &pps)?;
        let output: Box<Output> = Box::default();

        let yes: &CFType = CFBoolean::new(true);
        let spec = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder }],
            &[yes],
        );
        // NV12 in the bitstream's own (video) range: no conversion.
        let pixel_format =
            CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let attributes = CFDictionary::<CFString, CFType>::from_slices(
            &[unsafe { kCVPixelBufferPixelFormatTypeKey }],
            &[&pixel_format],
        );
        let callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_output),
            decompressionOutputRefCon: std::ptr::from_ref::<Output>(&output).cast_mut().cast(),
        };

        let mut raw: *mut VTDecompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(spec.as_opaque()),
                Some(attributes.as_opaque()),
                &callback,
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            // kVTCouldNotFindVideoDecoderErr: no hardware H.264 decoder here.
            return Err(if status == -12906 {
                CodecError::NoHardware
            } else {
                status_error("VTDecompressionSessionCreate", status)
            });
        };
        // SAFETY: a successful Create returns a +1 reference.
        let session = unsafe { CFRetained::from_raw(raw) };
        // Present each frame as soon as possible.
        let _ = unsafe {
            VTSessionSetProperty(&session, kVTDecompressionPropertyKey_RealTime, Some(yes))
        };
        Ok(Self {
            sps,
            pps,
            format,
            session,
            output,
        })
    }

    /// Decodes one access unit (AVCC NAL units, 4-byte lengths).
    fn decode(&mut self, avcc: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        let sample = sample_buffer(avcc, &self.format)?;
        let mut info = VTDecodeInfoFlags(0);
        // Synchronous: the callback runs before this returns.
        let status = unsafe {
            self.session.decode_frame(
                &sample,
                VTDecodeFrameFlags(0),
                std::ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            return Err(status_error("DecodeFrame", status));
        }
        let _ = unsafe { self.session.wait_for_asynchronous_frames() };
        match self.output.lock().expect("lock").take() {
            Some(result) => result.map(Some),
            None => Ok(None),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // After this the callback no longer runs, so `output` (dropped
        // after this) is never used again.
        unsafe {
            let _ = self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// Called by VideoToolbox with each decoded frame (or error).
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the refcon is the session's boxed `Output`, which outlives the
    // decompression session (invalidated first in `Session::drop`).
    let output = unsafe { &*refcon.cast_const().cast::<Output>() };
    let result = if status != 0 {
        Err(status_error("decode", status))
    } else {
        // SAFETY: VideoToolbox passes a valid image buffer, or NULL for a
        // dropped frame.
        match unsafe { image.as_ref() } {
            Some(image) => copy_frame(image),
            None => return,
        }
    };
    // Never unwind into VideoToolbox: a poisoned lock just loses the frame.
    if let Ok(mut output) = output.lock() {
        *output = Some(result);
    }
}

/// Copies a decoded NV12 pixel buffer into an I420 frame.
fn copy_frame(pixels: &CVImageBuffer) -> Result<YuvFrame, CodecError> {
    let lock = CVPixelBufferLockFlags::ReadOnly;
    if unsafe { CVPixelBufferLockBaseAddress(pixels, lock) } != kCVReturnSuccess {
        return Err(CodecError::Hardware("pixel buffer inacessível".into()));
    }
    let (width, height) = (
        CVPixelBufferGetWidth(pixels) as u32,
        CVPixelBufferGetHeight(pixels) as u32,
    );
    let plane = |index: usize| {
        let base = CVPixelBufferGetBaseAddressOfPlane(pixels, index).cast::<u8>();
        let stride = CVPixelBufferGetBytesPerRowOfPlane(pixels, index);
        let rows = CVPixelBufferGetHeightOfPlane(pixels, index);
        if base.is_null() {
            return (&[][..], stride);
        }
        // SAFETY: the buffer is locked; this plane holds `rows` rows of `stride` bytes.
        (
            unsafe { std::slice::from_raw_parts(base, stride * rows) },
            stride,
        )
    };
    let ((y, y_stride), (uv, uv_stride)) = (plane(0), plane(1));
    let frame = YuvFrame::from_nv12(width & !1, height & !1, y, y_stride, uv, uv_stride);
    unsafe { CVPixelBufferUnlockBaseAddress(pixels, lock) };
    frame
}

fn format_description(
    sps: &[u8],
    pps: &[u8],
) -> Result<CFRetained<CMFormatDescription>, CodecError> {
    if sps.is_empty() || pps.is_empty() {
        return Err(CodecError::InvalidFrame("SPS/PPS vazios".into()));
    }
    let pointers = [NonNull::from(&sps[0]), NonNull::from(&pps[0])];
    let sizes = [sps.len(), pps.len()];
    let mut raw: *const CMFormatDescription = std::ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&pointers).cast(),
            NonNull::from(&sizes).cast(),
            4,
            NonNull::from(&mut raw),
        )
    };
    match NonNull::new(raw.cast_mut()) {
        // SAFETY: a successful Create returns a +1 reference.
        Some(raw) if status == 0 => Ok(unsafe { CFRetained::from_raw(raw) }),
        _ => Err(status_error("formato H.264 (SPS/PPS)", status)),
    }
}

/// A sample buffer holding a copy of `avcc`.
fn sample_buffer(
    avcc: &[u8],
    format: &CMFormatDescription,
) -> Result<CFRetained<CMSampleBuffer>, CodecError> {
    let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            std::ptr::null_mut(),
            avcc.len(),
            None,
            std::ptr::null(),
            0,
            avcc.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut block),
        )
    };
    let block = match NonNull::new(block) {
        // SAFETY: a successful Create returns a +1 reference.
        Some(block) if status == 0 => unsafe { CFRetained::from_raw(block) },
        _ => return Err(status_error("CMBlockBufferCreateWithMemoryBlock", status)),
    };
    let status = unsafe {
        CMBlockBuffer::replace_data_bytes(NonNull::from(avcc).cast(), &block, 0, avcc.len())
    };
    if status != 0 {
        return Err(status_error("CMBlockBufferReplaceDataBytes", status));
    }

    let size = avcc.len();
    let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(&block),
            Some(format),
            1,
            0,
            std::ptr::null(),
            1,
            &size,
            NonNull::from(&mut sample),
        )
    };
    match NonNull::new(sample) {
        // SAFETY: a successful Create returns a +1 reference.
        Some(sample) if status == 0 => Ok(unsafe { CFRetained::from_raw(sample) }),
        _ => Err(status_error("CMSampleBufferCreateReady", status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::image;
    use crate::{EncoderSettings, Image, SoftwareEncoder, VideoEncoder};

    fn decoder() -> Option<VtDecoder> {
        match VtDecoder::new() {
            Ok(decoder) => Some(decoder),
            Err(e) => {
                eprintln!("VideoToolbox (decode) indisponível, teste ignorado: {e}");
                None
            }
        }
    }

    #[test]
    fn decodes_openh264_stream() {
        let Some(mut decoder) = decoder() else {
            return;
        };
        let (w, h) = (320, 240);
        let mut encoder = SoftwareEncoder::new(EncoderSettings::default());
        let mut decoded = 0;
        for t in 0..10 {
            let Some(encoded) = encoder.encode(&Image::bgra(w, h, &image(w, h, t))).unwrap() else {
                continue;
            };
            if let Some(frame) = decoder.decode(&encoded.data).unwrap() {
                assert_eq!((frame.width, frame.height), (w, h));
                assert_eq!(frame.u.len(), (w * h / 4) as usize);
                // Background 40 → ~51 in BT.601 limited range.
                let corner = frame.y[(h as usize - 1) * w as usize];
                assert!((45..=58).contains(&corner), "luma {corner}");
                decoded += 1;
            }
        }
        assert!(decoded >= 8, "decoded {decoded} of 10");
    }

    #[test]
    fn probe_parameter_sets_are_baseline() {
        let mut stream = vec![0, 0, 0, 1];
        stream.extend_from_slice(&PROBE_SPS);
        assert_eq!(h264::sps_profile(&stream), Some(h264::PROFILE_BASELINE));
        assert_eq!(h264::nal_type(&PROBE_PPS), Some(NAL_PPS));
    }

    #[test]
    fn frame_before_keyframe_is_an_error() {
        let Some(mut decoder) = decoder() else {
            return;
        };
        // A P slice without any SPS/PPS seen yet.
        assert!(decoder.decode(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]).is_err());
    }
}
