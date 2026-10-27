//! Hardware H.264 encoding through Media Foundation.
//!
//! GPU vendors ship their encoders as asynchronous Media Foundation
//! transforms (MFTs). The transform asks for input (`METransformNeedInput`)
//! and announces output (`METransformHaveOutput`) through events. Frames go
//! in as NV12 in system memory; a zero-copy path from the captured GPU
//! texture can come later.
//!
//! The decoder on the other side only handles the Baseline profile, so the
//! encoder is asked for it and every keyframe's SPS is checked: an encoder
//! that ignores the request fails here and `Auto` mode falls back to software.

// COM / Media Foundation calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use openh264::formats::YUVBuffer;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
    MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL,
    MFSampleExtension_CleanPoint, MFShutdown, MFStartup, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaEvent, METransformDrainComplete, MFT_MESSAGE_COMMAND_DRAIN,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface, PWSTR};

use crate::h264::{self, PROFILE_BASELINE};
use crate::nv12::bgra_to_nv12;
use crate::{CodecError, Encoded, EncoderSettings, VideoEncoder, check_frame};

/// Longest wait for the encoder to ask for input or deliver output.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a still screen waits for frames left in the encoder before
/// asking it to flush them.
const DRAIN_WAIT: Duration = Duration::from_millis(30);

/// Pause between event polls (high-resolution timers make this precise).
const POLL_INTERVAL: Duration = Duration::from_micros(200);

/// Hundreds of nanoseconds per second (Media Foundation time unit).
const HNS_PER_SEC: i64 = 10_000_000;

/// A keyframe at least every minute, so a lost one can never stall the
/// picture for long even if the viewer's request goes missing.
const GOP_FRAMES: u32 = 60 * 60;

fn hw(context: &str, e: windows::core::Error) -> CodecError {
    CodecError::Hardware(format!("{context}: {e}"))
}

pub struct MfEncoder {
    settings: EncoderSettings,
    name: String,
    /// Recreated whenever the input size changes.
    session: Option<Session>,
    force_keyframe: bool,
    frame_index: i64,
    yuv: Option<YUVBuffer>,
    nv12: Vec<u8>,
}

struct Session {
    width: u32,
    height: u32,
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    /// `METransformNeedInput` events not yet answered with an input.
    need_input: u32,
    /// The transform allocates output samples itself.
    provides_samples: bool,
    output_size: u32,
    /// Inputs given to the transform whose output has not come back yet.
    in_flight: u32,
    /// A drain was requested and `METransformDrainComplete` has not arrived.
    draining: bool,
    /// SPS/PPS from the output type, for encoders that do not repeat them inline.
    sequence_header: Vec<u8>,
}

impl MfEncoder {
    pub fn new(settings: EncoderSettings) -> Result<Self, CodecError> {
        unsafe {
            // Media Foundation needs COM on this (the capture) thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| hw("MFStartup", e))?;
        }
        let name = match enumerate() {
            Ok(encoders) => encoders.first().map(friendly_name),
            Err(e) => {
                unsafe {
                    let _ = MFShutdown();
                }
                return Err(e);
            }
        };
        let Some(name) = name else {
            unsafe {
                let _ = MFShutdown();
            }
            return Err(CodecError::NoHardware);
        };
        tracing::info!("codificador por hardware: {name}");
        Ok(Self {
            settings,
            name,
            session: None,
            force_keyframe: false,
            frame_index: 0,
            yuv: None,
            nv12: Vec::new(),
        })
    }

    fn open_session(&self, width: u32, height: u32) -> Result<Session, CodecError> {
        let activate = enumerate()?
            .into_iter()
            .next()
            .ok_or(CodecError::NoHardware)?;
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(|e| hw("ActivateObject", e))?;
        let fps = self.settings.max_fps.round().max(1.0) as u32;

        unsafe {
            let attributes = transform
                .GetAttributes()
                .map_err(|e| hw("GetAttributes", e))?;
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| hw("async unlock", e))?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // Encoder settings are best effort: drivers support different subsets.
        let codec: Option<ICodecAPI> = transform.cast().ok();
        if let Some(codec) = &codec {
            set(codec, &CODECAPI_AVLowLatencyMode, bool_variant());
            set(
                codec,
                &CODECAPI_AVEncCommonRateControlMode,
                u32_variant(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set(
                codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                u32_variant(self.settings.bitrate_bps),
            );
            set(codec, &CODECAPI_AVEncMPVGOPSize, u32_variant(GOP_FRAMES));
            set(
                codec,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                u32_variant(0),
            );
        }

        // Encoders want the output type before the input type.
        unsafe {
            let output = video_type(&MFVideoFormat_H264, width, height, fps)?;
            output
                .SetUINT32(&MF_MT_AVG_BITRATE, self.settings.bitrate_bps)
                .map_err(|e| hw("bitrate", e))?;
            output
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)
                .map_err(|e| hw("profile", e))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| hw("SetOutputType", e))?;

            let input = video_type(&MFVideoFormat_NV12, width, height, fps)?;
            input
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, width)
                .map_err(|e| hw("stride", e))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| hw("SetInputType (NV12)", e))?;

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| hw("begin streaming", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| hw("start of stream", e))?;
        }

        let info = unsafe { transform.GetOutputStreamInfo(0) }.map_err(|e| hw("stream info", e))?;
        let provides =
            (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        let events: IMFMediaEventGenerator =
            transform.cast().map_err(|e| hw("event generator", e))?;
        let mut session = Session {
            width,
            height,
            activate,
            transform,
            events,
            codec,
            need_input: 0,
            provides_samples: info.dwFlags & provides != 0,
            output_size: info.cbSize.max(width * height * 3 / 2),
            in_flight: 0,
            draining: false,
            sequence_header: Vec::new(),
        };
        session.refresh_sequence_header();
        Ok(session)
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Option<Encoded>, CodecError> {
        check_frame(width, height, bgra)?;
        if !matches!(&self.session, Some(s) if (s.width, s.height) == (width, height)) {
            self.session = None;
            self.session = Some(self.open_session(width, height)?);
            self.frame_index = 0;
        }

        bgra_to_nv12(width, height, bgra, &mut self.yuv, &mut self.nv12);
        let fps = i64::from(self.settings.max_fps.round().max(1.0) as u32);
        let duration = HNS_PER_SEC / fps;
        let sample = memory_sample(&self.nv12, self.frame_index * duration, duration)?;
        self.frame_index += 1;

        if std::mem::take(&mut self.force_keyframe) {
            let session = self.session.as_ref().expect("session opened above");
            let forced = session
                .codec
                .as_ref()
                .is_some_and(|codec| set(codec, &CODECAPI_AVEncVideoForceKeyFrame, u32_variant(1)));
            if !forced {
                // The driver cannot force one; a new session starts with a keyframe.
                self.session = None;
                self.session = Some(self.open_session(width, height)?);
            }
        }

        let session = self.session.as_mut().expect("session opened above");
        let result = session.encode(&sample);
        if result.is_err() {
            // Start over (with a keyframe) if the caller keeps using us.
            self.session = None;
        }
        result
    }

    fn pending(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.in_flight > 0)
    }

    fn drain(&mut self) -> Result<Option<Encoded>, CodecError> {
        let Some(session) = &mut self.session else {
            return Ok(None);
        };
        let result = session.drain();
        if result.is_err() {
            self.session = None;
        }
        result
    }

    fn force_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn name(&self) -> String {
        format!("GPU ({})", self.name)
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        self.session = None;
        unsafe {
            let _ = MFShutdown();
        }
    }
}

impl Session {
    fn encode(&mut self, sample: &IMFSample) -> Result<Option<Encoded>, CodecError> {
        // Wait until the transform accepts input. Outputs of earlier frames
        // that arrive meanwhile are kept: every one of them must be sent.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut ready = None;
        while self.need_input == 0 {
            merge(&mut ready, self.next_event(deadline)?);
        }
        unsafe { self.transform.ProcessInput(0, sample, 0) }.map_err(|e| hw("ProcessInput", e))?;
        self.need_input -= 1;
        self.in_flight += 1;

        // Usually this frame's output comes next. If the transform asks for
        // more input first, it keeps frames in flight; `pending()` then tells
        // the caller to come back even if the screen stays still.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while self.in_flight > 0 && self.need_input == 0 {
            merge(&mut ready, self.next_event(deadline)?);
        }
        Ok(ready)
    }

    fn drain(&mut self) -> Result<Option<Encoded>, CodecError> {
        // Most encoders just need a moment.
        let mut ready = None;
        let soft_deadline = Instant::now() + DRAIN_WAIT;
        while self.in_flight > 0 && Instant::now() < soft_deadline {
            merge(&mut ready, self.poll_event()?);
        }
        if self.in_flight == 0 {
            return Ok(ready);
        }
        // Others hold frames until told to flush.
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0) }
            .map_err(|e| hw("drain", e))?;
        self.draining = true;
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while self.draining {
            merge(&mut ready, self.next_event(deadline)?);
        }
        self.in_flight = 0;
        Ok(ready)
    }

    /// Like [`Session::next_event`], but returns right away when there is none.
    fn poll_event(&mut self) -> Result<Option<Encoded>, CodecError> {
        match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => self.handle(&event),
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                std::thread::sleep(POLL_INTERVAL);
                Ok(None)
            }
            Err(e) => Err(hw("GetEvent", e)),
        }
    }

    /// Waits for the next transform event and handles it; returns an encoded
    /// frame if the event delivered one.
    ///
    /// Polls instead of blocking, so a driver that stops answering fails the
    /// encoder (and `Auto` falls back to software) instead of hanging the host.
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Encoded>, CodecError> {
        let event = loop {
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => break event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() > deadline {
                        return Err(CodecError::Hardware("o encoder parou de responder".into()));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => return Err(hw("GetEvent", e)),
            }
        };
        self.handle(&event)
    }

    fn handle(&mut self, event: &IMFMediaEvent) -> Result<Option<Encoded>, CodecError> {
        let kind = unsafe { event.GetType() }.map_err(|e| hw("event type", e))?;
        if kind == METransformDrainComplete.0 as u32 {
            self.draining = false;
            Ok(None)
        } else if kind == METransformNeedInput.0 as u32 {
            self.need_input += 1;
            Ok(None)
        } else if kind == METransformHaveOutput.0 as u32 {
            self.take_output()
        } else {
            Ok(None)
        }
    }

    fn take_output(&mut self) -> Result<Option<Encoded>, CodecError> {
        let mut sample = None;
        // A format change is answered by accepting the new type and asking for
        // the same output again; it is not announced a second time.
        for _ in 0..3 {
            let provided = if self.provides_samples {
                None
            } else {
                Some(empty_sample(self.output_size)?)
            };
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };
            // Take back ownership so the COM references are released.
            let output = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));

            match result {
                Ok(()) => {
                    sample = output;
                    break;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => unsafe {
                    let new_type = self
                        .transform
                        .GetOutputAvailableType(0, 0)
                        .map_err(|e| hw("new output type", e))?;
                    self.transform
                        .SetOutputType(0, &new_type, 0)
                        .map_err(|e| hw("SetOutputType after change", e))?;
                    self.refresh_sequence_header();
                },
                Err(e) => return Err(hw("ProcessOutput", e)),
            }
        }
        let Some(sample) = sample else {
            return Err(CodecError::Hardware("output format kept changing".into()));
        };
        self.in_flight = self.in_flight.saturating_sub(1);

        let mut data = read_sample(&sample)?;
        // Not every driver sets the clean-point attribute: look at the NALs too.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let keyframe = clean_point || h264::has_idr(&data);
        if keyframe && !h264::has_sps(&data) && !self.sequence_header.is_empty() {
            let mut with_header = self.sequence_header.clone();
            with_header.extend_from_slice(&data);
            data = with_header;
        }
        // Checked on every SPS, so a profile switch later on is caught too.
        if let Some(profile) = h264::sps_profile(&data)
            && profile != PROFILE_BASELINE
        {
            return Err(CodecError::Hardware(format!(
                "produz perfil H.264 {profile}, o visualizador precisa de Baseline ({PROFILE_BASELINE})"
            )));
        }
        Ok(Some(Encoded { data, keyframe }))
    }

    fn refresh_sequence_header(&mut self) {
        self.sequence_header = unsafe { self.transform.GetOutputCurrentType(0) }
            .ok()
            .and_then(|t| blob(&t, &MF_MT_MPEG_SEQUENCE_HEADER))
            .unwrap_or_default();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Async transforms hold driver resources until shut down explicitly.
        unsafe {
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

/// Adds `next` to the outputs collected in `ready`. Several access units in
/// one payload decode in order, so none is dropped, except that a keyframe
/// makes everything before it unnecessary: the payload then starts with it,
/// and its keyframe flag stays truthful.
fn merge(ready: &mut Option<Encoded>, next: Option<Encoded>) {
    match (ready.as_mut(), next) {
        (_, None) => {}
        (None, Some(next)) => *ready = Some(next),
        (Some(_), Some(next)) if next.keyframe => *ready = Some(next),
        (Some(ready), Some(next)) => ready.data.extend_from_slice(&next.data),
    }
}

/// Hardware H.264 encoders that accept NV12, best first.
fn enumerate() -> Result<Vec<IMFActivate>, CodecError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )
        .map_err(|e| hw("MFTEnumEx", e))?;
    }
    if array.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: MFTEnumEx returned `count` activation objects in a CoTaskMem
    // array; each is moved out once, then the array itself is freed.
    let activates = unsafe {
        let items = (0..count as usize)
            .filter_map(|i| std::ptr::read(array.add(i)))
            .collect();
        CoTaskMemFree(Some(array as *const _));
        items
    };
    Ok(activates)
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
            .is_err()
        {
            return "H.264 MFT".to_owned();
        }
        let text = name.to_string().unwrap_or_else(|_| "H.264 MFT".to_owned());
        CoTaskMemFree(Some(name.0 as *const _));
        text
    }
}

fn video_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<IMFMediaType, CodecError> {
    unsafe {
        let media_type = MFCreateMediaType().map_err(|e| hw("MFCreateMediaType", e))?;
        let set = |r: windows::core::Result<()>, what| r.map_err(|e| hw(what, e));
        set(
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video),
            "major type",
        )?;
        set(media_type.SetGUID(&MF_MT_SUBTYPE, subtype), "subtype")?;
        set(
            media_type.SetUINT64(
                &MF_MT_FRAME_SIZE,
                (u64::from(width) << 32) | u64::from(height),
            ),
            "frame size",
        )?;
        set(
            media_type.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps) << 32) | 1),
            "frame rate",
        )?;
        set(
            media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1),
            "aspect ratio",
        )?;
        set(
            media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32),
            "interlace mode",
        )?;
        Ok(media_type)
    }
}

/// An empty sample with room for `size` bytes, for transforms that want the
/// caller to provide output samples.
fn empty_sample(size: u32) -> Result<IMFSample, CodecError> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(size).map_err(|e| hw("MFCreateMemoryBuffer", e))?;
        let sample = MFCreateSample().map_err(|e| hw("MFCreateSample", e))?;
        sample.AddBuffer(&buffer).map_err(|e| hw("AddBuffer", e))?;
        Ok(sample)
    }
}

/// A sample holding a copy of `data` in system memory.
fn memory_sample(data: &[u8], time: i64, duration: i64) -> Result<IMFSample, CodecError> {
    unsafe {
        let buffer =
            MFCreateMemoryBuffer(data.len() as u32).map_err(|e| hw("MFCreateMemoryBuffer", e))?;
        let mut ptr = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .map_err(|e| hw("Lock", e))?;
        // SAFETY: the buffer was created with room for `data.len()` bytes.
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().map_err(|e| hw("Unlock", e))?;
        buffer
            .SetCurrentLength(data.len() as u32)
            .map_err(|e| hw("SetCurrentLength", e))?;
        let sample = MFCreateSample().map_err(|e| hw("MFCreateSample", e))?;
        sample.AddBuffer(&buffer).map_err(|e| hw("AddBuffer", e))?;
        sample
            .SetSampleTime(time)
            .map_err(|e| hw("SetSampleTime", e))?;
        sample
            .SetSampleDuration(duration)
            .map_err(|e| hw("SetSampleDuration", e))?;
        Ok(sample)
    }
}

fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, CodecError> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| hw("ConvertToContiguousBuffer", e))?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| hw("Lock output", e))?;
        // SAFETY: Lock returned a pointer to `len` valid bytes until Unlock.
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        buffer.Unlock().map_err(|e| hw("Unlock output", e))?;
        Ok(data)
    }
}

fn blob(media_type: &IMFMediaType, key: &GUID) -> Option<Vec<u8>> {
    unsafe {
        let size = media_type.GetBlobSize(key).ok()?;
        let mut data = vec![0; size as usize];
        media_type.GetBlob(key, &mut data, None).ok()?;
        Some(data)
    }
}

/// Applies an encoder setting; returns whether the driver accepted it.
fn set(codec: &ICodecAPI, api: &GUID, value: VARIANT) -> bool {
    match unsafe { codec.SetValue(api, &value) } {
        Ok(()) => true,
        Err(e) => {
            tracing::debug!("ICodecAPI {api:?} não suportado: {e}");
            false
        }
    }
}

fn variant(vt: VARENUM, value: VARIANT_0_0_0) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: value,
            }),
        },
    }
}

fn u32_variant(value: u32) -> VARIANT {
    variant(VT_UI4, VARIANT_0_0_0 { ulVal: value })
}

fn bool_variant() -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_TRUE,
        },
    )
}
