//! Hardware H.264 encoding through Media Foundation.
//!
//! GPU vendors ship their encoders as asynchronous Media Foundation
//! transforms (MFTs). The transform asks for input (`METransformNeedInput`)
//! and announces output (`METransformHaveOutput`) through events. Frames go
//! in as NV12: straight from the capture's GPU texture when the transform
//! runs on the capture's adapter and supports Direct3D 11 (zero-copy: the
//! image never leaves the GPU), otherwise from system memory.
//!
//! The decoder on the other side only handles the Baseline profile, so the
//! encoder is asked for it and every keyframe's SPS is checked: an encoder
//! that ignores the request fails here and `Auto` mode falls back to software.

// COM / Media Foundation calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use openh264::formats::YUVBuffer;
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFShutdown, IMFTransform,
    METransformHaveOutput, METransformNeedInput, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_FULL, MFSampleExtension_CleanPoint, MFShutdown, MFStartup,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Base,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFAttributes,
    IMFDXGIDeviceManager, METransformDrainComplete, MF_SA_D3D11_AWARE, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFT_ENUM_ADAPTER_LUID,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_SET_D3D_MANAGER, MFTEnum2,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::{
    VARENUM, VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4,
};
use windows::core::{GUID, Interface, PWSTR, Ref, implement};

use crate::d3d11::Readback;
use crate::h264::{self, PROFILE_BASELINE};
use crate::nv12::bgra_to_nv12;
use crate::{
    CodecError, Encoded, EncoderSettings, Image, PixelFormat, Pixels, VideoEncoder, check_frame,
    merge, profile_error,
};

/// Most GPU-texture frames the transform may hold at once. The capture
/// reuses a ring of textures (6), so a texture must be encoded before the
/// capture comes back around to it.
const MAX_TEXTURES_IN_FLIGHT: u32 = 3;

/// Longest wait for the encoder to ask for input or deliver output.
const EVENT_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a still screen waits for frames left in the encoder before
/// asking it to flush them.
const DRAIN_WAIT: Duration = Duration::from_millis(30);

/// Hundreds of nanoseconds per second (Media Foundation time unit).
pub(crate) const HNS_PER_SEC: i64 = 10_000_000;

/// A keyframe at least every minute, so a lost one can never stall the
/// picture for long even if the viewer's request goes missing.
const GOP_FRAMES: u32 = 60 * 60;

pub(crate) fn hw(context: &str, e: windows::core::Error) -> CodecError {
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
    /// GPU textures the transform cannot take, read back into `nv12`.
    readback: Readback,
    /// Encoders that failed taking textures directly: fed through memory.
    zero_copy_failed: Vec<String>,
    /// The last try gave the encoder textures (for `degrade`).
    tried_gpu_input: bool,
    /// Encoders that failed even through memory: the next ones are tried.
    failed: Vec<String>,
}

/// How many times one image is retried on another way to encode it
/// (without zero-copy, then another encoder) before giving up on the GPU.
const MAX_ATTEMPTS: usize = 4;

struct Session {
    width: u32,
    height: u32,
    /// The capture's D3D11 device this session was opened for, if any.
    device: Option<ID3D11Device>,
    /// The transform takes textures of `device` directly (zero-copy).
    gpu_input: bool,
    /// Keeps the device manager handed to the transform alive.
    _manager: Option<IMFDXGIDeviceManager>,
    activate: IMFActivate,
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    /// Receives the type of each event, delivered by [`EventSink`].
    event_rx: mpsc::Receiver<windows::core::Result<u32>>,
    callback: IMFAsyncCallback,
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
        let name = match enumerate(None) {
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
            readback: Readback::default(),
            zero_copy_failed: Vec::new(),
            tried_gpu_input: false,
            failed: Vec::new(),
        })
    }

    /// After a failure with the current session, the next way to try:
    /// the same encoder through memory, then the other encoders (opening
    /// the session finds there is none left, with `NoHardware`).
    fn degrade(&mut self, gpu_input: bool, error: &CodecError) {
        // The next try opens a session the new way.
        self.session = None;
        if gpu_input && !self.zero_copy_failed.contains(&self.name) {
            tracing::warn!(
                "{}: zero-copy falhou ({error}); imagem pela memória",
                self.name
            );
            self.zero_copy_failed.push(self.name.clone());
        } else if !self.failed.contains(&self.name) {
            tracing::warn!(
                "{} falhou ({error}); tentando outro codificador da GPU",
                self.name
            );
            self.failed.push(self.name.clone());
        }
    }

    /// Replaces the session (the new one starts with a keyframe).
    fn reopen(
        &mut self,
        width: u32,
        height: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<(), CodecError> {
        self.session = None;
        let session = self.open_session(width, height, device)?;
        self.session = Some(session);
        self.frame_index = 0;
        Ok(())
    }

    /// Opens an encoder for `width`x`height` NV12 images. With `device` (the
    /// capture's), an encoder on the same adapter is preferred, and is given
    /// the device so it can read the capture's textures directly.
    fn open_session(
        &mut self,
        width: u32,
        height: u32,
        device: Option<&ID3D11Device>,
    ) -> Result<Session, CodecError> {
        // An encoder on the capture's adapter, or else any (fed from
        // memory); none that already failed.
        let usable = |list: Vec<IMFActivate>| {
            list.into_iter()
                .find(|a| !self.failed.contains(&friendly_name(a)))
        };
        let on_adapter = device.and_then(|device| {
            enumerate(Some(device))
                .inspect_err(|e| tracing::debug!("encoders do adaptador da captura: {e}"))
                .ok()
                .and_then(usable)
        });
        let same_adapter = on_adapter.is_some();
        let activate = match on_adapter {
            Some(activate) => activate,
            None => usable(enumerate(None)?).ok_or(CodecError::NoHardware)?,
        };
        // Named before it can fail, so a failure counts against this one.
        let name = friendly_name(&activate);
        if name != self.name {
            tracing::info!("codificador por hardware: {name}");
            self.name = name;
        }
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.map_err(|e| hw("ActivateObject", e))?;

        let fps = self.settings.max_fps.round().max(1.0) as u32;

        // An async transform refuses every message until unlocked.
        unsafe {
            let attributes = transform
                .GetAttributes()
                .map_err(|e| hw("GetAttributes", e))?;
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .map_err(|e| hw("async unlock", e))?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        // The device goes in before the media types.
        let manager = match device {
            Some(device) if same_adapter && !self.zero_copy_failed.contains(&self.name) => {
                attach_device(&transform, device)
                    .inspect_err(|e| {
                        tracing::info!(
                            "o encoder não lê texturas da GPU ({e}); imagem pela memória"
                        );
                    })
                    .ok()
            }
            _ => None,
        };
        // A failure from here on, with textures, may be the zero-copy's.
        self.tried_gpu_input = manager.is_some();
        if manager.is_some() {
            tracing::info!("zero-copy: o encoder lê a imagem direto da GPU");
        } else if device.is_some() && !same_adapter {
            tracing::info!("encoder em outro adaptador que a captura; imagem pela memória");
        } else if device.is_some() {
            tracing::info!("imagem pela memória (o zero-copy falhou antes)");
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
        let (event_tx, event_rx) = mpsc::channel();
        let callback: IMFAsyncCallback = EventSink {
            generator: events.clone(),
            tx: event_tx,
        }
        .into();
        let mut session = Session {
            width,
            height,
            device: device.cloned(),
            gpu_input: manager.is_some(),
            _manager: manager,
            activate,
            transform,
            events,
            event_rx,
            callback,
            codec,
            need_input: 0,
            provides_samples: info.dwFlags & provides != 0,
            output_size: info.cbSize.max(width * height * 3 / 2),
            in_flight: 0,
            draining: false,
            sequence_header: Vec::new(),
        };
        session.refresh_sequence_header();
        session.request_event()?;
        Ok(session)
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, image: &Image<'_>) -> Result<Option<Encoded>, CodecError> {
        check_frame(image)?;
        // The capture's device (not the encoder's fault if this fails).
        let device = match image.pixels {
            Pixels::D3d11(texture) => {
                Some(unsafe { texture.GetDevice() }.map_err(|e| hw("GetDevice", e))?)
            }
            Pixels::Memory(_) => None,
        };
        let mut first_error = None;
        for _ in 0..MAX_ATTEMPTS {
            self.tried_gpu_input = self.session.as_ref().is_some_and(|s| s.gpu_input);
            match self.encode_once(image, device.as_ref()) {
                Ok(encoded) => return Ok(encoded),
                // No encoder left to try: the first failure says why.
                Err(CodecError::NoHardware) if first_error.is_some() => break,
                Err(e) => {
                    self.degrade(self.tried_gpu_input, &e);
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or(CodecError::NoHardware))
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

impl MfEncoder {
    /// One try at encoding `image` with the current way (opening it if
    /// needed).
    fn encode_once(
        &mut self,
        image: &Image<'_>,
        device: Option<&ID3D11Device>,
    ) -> Result<Option<Encoded>, CodecError> {
        let (width, height) = (image.width, image.height);
        let device = device.cloned();
        // A new size, the first texture (or a new capture device), or memory
        // images after textures: the encoder must be opened for it.
        let reopen = match &self.session {
            Some(s) => {
                (s.width, s.height) != (width, height)
                    || (device.is_some() && s.device != device)
                    || (device.is_none() && s.gpu_input)
            }
            None => true,
        };
        if reopen {
            self.reopen(width, height, device.as_ref())?;
        }

        // A session just opened starts with a keyframe anyway.
        if std::mem::take(&mut self.force_keyframe) && !reopen {
            let session = self.session.as_ref().expect("session opened above");
            let forced = session
                .codec
                .as_ref()
                .is_some_and(|codec| set(codec, &CODECAPI_AVEncVideoForceKeyFrame, u32_variant(1)));
            if !forced {
                // The driver cannot force one; a new session starts with a keyframe.
                self.reopen(width, height, device.as_ref())?;
            }
        }

        // Built for the session that will take it (after any reopen above).
        let gpu_input = self.session.as_ref().is_some_and(|s| s.gpu_input);
        self.tried_gpu_input = gpu_input;
        let fps = i64::from(self.settings.max_fps.round().max(1.0) as u32);
        let duration = HNS_PER_SEC / fps;
        let time = self.frame_index * duration;
        let sample = match (image.pixels, image.format) {
            (Pixels::D3d11(texture), _) if gpu_input => texture_sample(texture, time, duration)?,
            (Pixels::D3d11(texture), _) => {
                self.readback.read(texture, width, height, &mut self.nv12)?;
                memory_sample(&self.nv12, time, duration)?
            }
            (Pixels::Memory(data), PixelFormat::Nv12) => memory_sample(data, time, duration)?,
            // The transform takes NV12; BGRA is converted on the CPU first.
            (Pixels::Memory(data), PixelFormat::Bgra) => {
                bgra_to_nv12(width, height, data, &mut self.yuv, &mut self.nv12);
                memory_sample(&self.nv12, time, duration)?
            }
        };
        self.frame_index += 1;

        let session = self.session.as_mut().expect("session opened above");
        let result = session.encode(&sample);
        if result.is_err() {
            // Start over (with a keyframe) if the caller keeps using us.
            self.session = None;
        }
        result
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
        // Wait until the transform accepts input (and, for textures, has few
        // enough frames in flight). Outputs of earlier frames that arrive
        // meanwhile are kept: every one of them must be sent.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut ready = None;
        while self.need_input == 0 || (self.gpu_input && self.in_flight >= MAX_TEXTURES_IN_FLIGHT) {
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
            merge(&mut ready, self.poll_event(soft_deadline)?);
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
        // A drained transform must be restarted; the NeedInput events it sent
        // before the drain are stale, so discard them and ask for fresh ones.
        self.need_input = 0;
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        }
        .map_err(|e| hw("restart after drain", e))?;
        Ok(ready)
    }

    /// Asks Media Foundation for the next event; [`EventSink`] receives it.
    /// Only one request may be outstanding, so this follows each delivery.
    fn request_event(&self) -> Result<(), CodecError> {
        unsafe { self.events.BeginGetEvent(&self.callback, None) }
            .map_err(|e| hw("BeginGetEvent", e))
    }

    /// The type of the next event, or `None` if none arrives by `deadline`.
    ///
    /// The callback wakes this thread as soon as the event exists (no polling
    /// sleeps, whose ~15 ms granularity on Windows would add latency), and the
    /// deadline keeps a stuck driver from hanging the host.
    fn wait_event(&mut self, deadline: Instant) -> Result<Option<u32>, CodecError> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match self.event_rx.recv_timeout(timeout) {
            Ok(Ok(kind)) => {
                self.request_event()?;
                Ok(Some(kind))
            }
            Ok(Err(e)) => Err(hw("GetEvent", e)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(CodecError::Hardware("event callback gone".into()))
            }
        }
    }

    /// Like [`Session::next_event`], but a timeout just means "nothing yet".
    fn poll_event(&mut self, deadline: Instant) -> Result<Option<Encoded>, CodecError> {
        match self.wait_event(deadline)? {
            Some(kind) => self.handle(kind),
            None => Ok(None),
        }
    }

    /// Waits for the next transform event and handles it; returns an encoded
    /// frame if the event delivered one. Fails if the encoder stays silent
    /// past `deadline`, so `Auto` falls back to software.
    fn next_event(&mut self, deadline: Instant) -> Result<Option<Encoded>, CodecError> {
        match self.wait_event(deadline)? {
            Some(kind) => self.handle(kind),
            None => Err(CodecError::Hardware("o encoder parou de responder".into())),
        }
    }

    fn handle(&mut self, kind: u32) -> Result<Option<Encoded>, CodecError> {
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
        // One pass over the NALs instead of three.
        let scan = h264::scan(&data);
        // Not every driver sets the clean-point attribute: look at the NALs too.
        let clean_point =
            unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }.unwrap_or(0) != 0;
        let keyframe = clean_point || scan.has_idr;
        if keyframe && !scan.has_sps && !self.sequence_header.is_empty() {
            let mut with_header = self.sequence_header.clone();
            with_header.extend_from_slice(&data);
            data = with_header;
            // Checked on every SPS, so a profile switch later on is caught too.
            if let Some(profile) = h264::sps_profile(&data)
                && profile != PROFILE_BASELINE
            {
                return Err(profile_error(profile));
            }
        } else if let Some(profile) = scan.profile
            && profile != PROFILE_BASELINE
        {
            return Err(profile_error(profile));
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

/// Receives Media Foundation's asynchronous event deliveries (on an MF work
/// thread) and forwards each event's type to the encoding thread.
#[implement(IMFAsyncCallback)]
struct EventSink {
    generator: IMFMediaEventGenerator,
    tx: mpsc::Sender<windows::core::Result<u32>>,
}

impl IMFAsyncCallback_Impl for EventSink_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        // Default work queue and behavior.
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let kind = unsafe { self.generator.EndGetEvent(result.ok()?) }
            .and_then(|event| unsafe { event.GetType() });
        // The receiver is gone once the session ended; nothing to do then.
        let _ = self.tx.send(kind);
        Ok(())
    }
}

/// Hardware H.264 encoders that accept NV12, best first; with `device`,
/// only those on its adapter.
fn enumerate(device: Option<&ID3D11Device>) -> Result<Vec<IMFActivate>, CodecError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        match device {
            Some(device) => MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &adapter_filter(device)?,
                &mut array,
                &mut count,
            )
            .map_err(|e| hw("MFTEnum2", e))?,
            None => MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &mut array,
                &mut count,
            )
            .map_err(|e| hw("MFTEnumEx", e))?,
        }
    }
    if array.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: the enumeration returned `count` activation objects in a
    // CoTaskMem array; each is moved out once, then the array itself is freed.
    let activates = unsafe {
        let items = (0..count as usize)
            .filter_map(|i| std::ptr::read(array.add(i)))
            .collect();
        CoTaskMemFree(Some(array as *const _));
        items
    };
    Ok(activates)
}

/// Enumeration attributes selecting the adapter `device` runs on.
fn adapter_filter(device: &ID3D11Device) -> Result<IMFAttributes, CodecError> {
    unsafe {
        let luid = device
            .cast::<IDXGIDevice>()
            .and_then(|dxgi| dxgi.GetAdapter())
            .and_then(|adapter| adapter.GetDesc())
            .map_err(|e| hw("adaptador da captura", e))?
            .AdapterLuid;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1).map_err(|e| hw("MFCreateAttributes", e))?;
        let attributes =
            attributes.ok_or_else(|| CodecError::Hardware("MFCreateAttributes".into()))?;
        // A LUID blob: LowPart, then HighPart, both little-endian.
        let mut blob = luid.LowPart.to_le_bytes().to_vec();
        blob.extend_from_slice(&luid.HighPart.to_le_bytes());
        attributes
            .SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob)
            .map_err(|e| hw("MFT_ENUM_ADAPTER_LUID", e))?;
        Ok(attributes)
    }
}

/// Gives the transform the capture's D3D11 device, so it reads the
/// capture's textures directly. Fails if the transform cannot.
fn attach_device(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, CodecError> {
    unsafe {
        let aware = transform
            .GetAttributes()
            .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE))
            .unwrap_or(0);
        if aware == 0 {
            return Err(CodecError::Hardware("sem suporte a Direct3D 11".into()));
        }
        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| hw("MFCreateDXGIDeviceManager", e))?;
        let manager =
            manager.ok_or_else(|| CodecError::Hardware("MFCreateDXGIDeviceManager".into()))?;
        manager
            .ResetDevice(device, token)
            .map_err(|e| hw("ResetDevice", e))?;
        transform
            .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
            .map_err(|e| hw("MFT_MESSAGE_SET_D3D_MANAGER", e))?;
        Ok(manager)
    }
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

/// A sample wrapping an NV12 texture, read by the transform on the GPU.
fn texture_sample(
    texture: &ID3D11Texture2D,
    time: i64,
    duration: i64,
) -> Result<IMFSample, CodecError> {
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)
            .map_err(|e| hw("MFCreateDXGISurfaceBuffer", e))?;
        // Some encoders check the buffer's length even for textures.
        let length = buffer
            .cast::<IMF2DBuffer>()
            .and_then(|buffer| buffer.GetContiguousLength())
            .map_err(|e| hw("GetContiguousLength", e))?;
        buffer
            .SetCurrentLength(length)
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

/// A sample holding a copy of `data` in system memory.
pub(crate) fn memory_sample(
    data: &[u8],
    time: i64,
    duration: i64,
) -> Result<IMFSample, CodecError> {
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
pub(crate) fn set(codec: &ICodecAPI, api: &GUID, value: VARIANT) -> bool {
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

pub(crate) fn bool_variant() -> VARIANT {
    variant(
        VT_BOOL,
        VARIANT_0_0_0 {
            boolVal: VARIANT_TRUE,
        },
    )
}
