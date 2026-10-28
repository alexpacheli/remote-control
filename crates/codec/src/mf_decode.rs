//! Hardware H.264 decoding through Media Foundation (Windows).
//!
//! Microsoft's H.264 decoder transform, given a Direct3D 11 device, decodes
//! on the GPU (DXVA) into NV12 textures, which are read back into an I420
//! [`YuvFrame`] for the renderer. Without a device it would decode on the
//! CPU, so failing to attach one counts as "no hardware": `Auto` then uses
//! OpenH264. Low-latency mode makes it output each frame right after its
//! input instead of buffering a few.

// COM / Media Foundation calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFDXGIBuffer,
    IMFDXGIDeviceManager, IMFSample, IMFTransform, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_SA_D3D11_AWARE, MF_VERSION,
    MFCreateDXGIDeviceManager, MFCreateMediaType, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown,
    MFStartup, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
    MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::core::Interface;

use crate::d3d11::Readback;
use crate::mf::{HNS_PER_SEC, bool_variant, hw, memory_sample, set};
use crate::{CodecError, VideoDecoder, YuvFrame};

pub struct MfDecoder {
    /// `None` only while dropping, so the COM objects go before `MFShutdown`.
    inner: Option<Inner>,
}

struct Inner {
    transform: IMFTransform,
    /// Kept alive for the transform.
    _device: ID3D11Device,
    _manager: IMFDXGIDeviceManager,
    /// Displayed size of the frames (the decoded textures may be taller,
    /// e.g. 1088 rows for 1080p).
    size: (u32, u32),
    readback: Readback,
    frame_index: i64,
}

impl MfDecoder {
    pub fn new() -> Result<Self, CodecError> {
        unsafe {
            // Media Foundation needs COM on this (the decode) thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| hw("MFStartup", e))?;
        }
        match Inner::new() {
            Ok(inner) => {
                tracing::info!("decodificador por hardware: Media Foundation (Direct3D 11)");
                Ok(Self { inner: Some(inner) })
            }
            Err(e) => {
                unsafe {
                    let _ = MFShutdown();
                }
                Err(e)
            }
        }
    }
}

impl Drop for MfDecoder {
    fn drop(&mut self) {
        self.inner = None;
        unsafe {
            let _ = MFShutdown();
        }
    }
}

impl VideoDecoder for MfDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        self.inner
            .as_mut()
            .expect("alive until dropped")
            .decode(data)
    }

    fn name(&self) -> String {
        "GPU (Media Foundation)".to_owned()
    }

    fn is_hardware(&self) -> bool {
        true
    }
}

impl Inner {
    fn new() -> Result<Self, CodecError> {
        let device = create_device()?;
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| hw("decodificador H.264", e))?;

        unsafe {
            let attributes = transform
                .GetAttributes()
                .map_err(|e| hw("GetAttributes", e))?;
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err(CodecError::NoHardware);
            }
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            set(&codec, &CODECAPI_AVLowLatencyMode, bool_variant());
        }

        // With the device it decodes on the GPU; refused means no DXVA.
        let manager = unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .map_err(|e| hw("MFCreateDXGIDeviceManager", e))?;
            let manager =
                manager.ok_or_else(|| CodecError::Hardware("MFCreateDXGIDeviceManager".into()))?;
            manager
                .ResetDevice(&device, token)
                .map_err(|e| hw("ResetDevice", e))?;
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .map_err(|_| CodecError::NoHardware)?;
            manager
        };

        unsafe {
            let input = MFCreateMediaType().map_err(|e| hw("MFCreateMediaType", e))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| hw("major type", e))?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| hw("subtype", e))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| hw("SetInputType (H.264)", e))?;
        }
        let mut inner = Self {
            transform,
            _device: device,
            _manager: manager,
            size: (0, 0),
            readback: Readback::default(),
            frame_index: 0,
        };
        inner.set_output_type()?;
        unsafe {
            inner
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| hw("begin streaming", e))?;
            inner
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| hw("start of stream", e))?;
        }
        // The textures it decodes into are its own.
        let info =
            unsafe { inner.transform.GetOutputStreamInfo(0) }.map_err(|e| hw("stream info", e))?;
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
            return Err(CodecError::NoHardware);
        }
        Ok(inner)
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        let duration = HNS_PER_SEC / 60;
        let sample = memory_sample(data, self.frame_index * duration, duration)?;
        self.frame_index += 1;

        let mut latest = None;
        let accepted = unsafe { self.transform.ProcessInput(0, &sample, 0) };
        if let Err(e) = accepted {
            if e.code() != MF_E_NOTACCEPTING {
                return Err(hw("ProcessInput", e));
            }
            // Output is waiting: collect it, then the input fits.
            latest = self.collect()?;
            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .map_err(|e| hw("ProcessInput", e))?;
        }
        // Low-latency mode outputs this frame now; keep the newest if more came.
        Ok(self.collect()?.or(latest))
    }

    /// Takes every frame the transform has ready; reads back only the newest.
    fn collect(&mut self) -> Result<Option<YuvFrame>, CodecError> {
        let mut latest: Option<IMFSample> = None;
        loop {
            let mut buffer = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status)
            };
            // Take back ownership so the COM references are released.
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => latest = sample.or(latest),
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    return latest.map(|sample| self.read(&sample)).transpose();
                }
                // First frame, or a new size: accept the new output type.
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_output_type()?,
                Err(e) => return Err(hw("ProcessOutput", e)),
            }
        }
    }

    /// Picks NV12 among the output types and notes the displayed size.
    fn set_output_type(&mut self) -> Result<(), CodecError> {
        let mut index = 0;
        let output = loop {
            let candidate = unsafe { self.transform.GetOutputAvailableType(0, index) }
                .map_err(|e| hw("sem saída NV12", e))?;
            if unsafe { candidate.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
                break candidate;
            }
            index += 1;
        };
        unsafe { self.transform.SetOutputType(0, &output, 0) }
            .map_err(|e| hw("SetOutputType (NV12)", e))?;

        let frame = unsafe { output.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
        let (mut width, mut height) = ((frame >> 32) as u32, frame as u32);
        // The aperture crops the coded size (1088) to the picture (1080).
        let mut area = MFVideoArea::default();
        let blob = unsafe {
            std::slice::from_raw_parts_mut(
                std::ptr::from_mut(&mut area).cast::<u8>(),
                size_of::<MFVideoArea>(),
            )
        };
        if unsafe { output.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, blob, None) }.is_ok()
            && area.Area.cx > 0
            && area.Area.cy > 0
        {
            width = width.min(area.Area.cx as u32);
            height = height.min(area.Area.cy as u32);
        }
        self.size = (width & !1, height & !1);
        Ok(())
    }

    /// Reads a decoded frame (an NV12 texture array slice) back to memory.
    fn read(&mut self, sample: &IMFSample) -> Result<YuvFrame, CodecError> {
        let (width, height) = self.size;
        unsafe {
            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(|e| hw("GetBufferByIndex", e))?;
            let dxgi: IMFDXGIBuffer = buffer.cast().map_err(|e| hw("saída fora da GPU", e))?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)
                .map_err(|e| hw("GetResource", e))?;
            let texture = ID3D11Texture2D::from_raw(raw);
            let index = dxgi
                .GetSubresourceIndex()
                .map_err(|e| hw("GetSubresourceIndex", e))?;
            self.readback
                .map(&texture, index, width, height, |planes| {
                    YuvFrame::from_nv12(
                        width,
                        height,
                        planes.y,
                        planes.stride,
                        planes.uv,
                        planes.stride,
                    )
                })?
        }
    }
}

/// A hardware D3D11 device with video support, shared with the decoder's
/// own threads.
fn create_device() -> Result<ID3D11Device, CodecError> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|_| CodecError::NoHardware)?;
    let device = device.ok_or(CodecError::NoHardware)?;
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        let _ = unsafe { multithread.SetMultithreadProtected(true) };
    }
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::image;
    use crate::{EncoderSettings, Image, SoftwareEncoder, VideoEncoder};

    #[test]
    fn decodes_openh264_stream() {
        let mut decoder = match MfDecoder::new() {
            Ok(decoder) => decoder,
            Err(e) => {
                eprintln!("sem decodificação por hardware, teste ignorado: {e}");
                return;
            }
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
                // Background 40 → ~51 in BT.601 limited range.
                let corner = frame.y[(h as usize - 1) * w as usize];
                assert!((45..=58).contains(&corner), "luma {corner}");
                decoded += 1;
            }
        }
        assert!(decoded >= 8, "decoded {decoded} of 10");
    }
}
