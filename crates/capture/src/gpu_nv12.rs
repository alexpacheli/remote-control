//! BGRA → NV12 conversion on the GPU, with the D3D11 video processor.
//!
//! The desktop texture is copied (on the GPU) into a texture the video
//! processor can read, then converted into one of a small ring of NV12
//! textures that the hardware encoder takes as they are: the image never
//! goes through system memory.

// Direct3D 11 video calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::CaptureError;

/// NV12 textures handed out in turn. The encoder may still be reading one
/// while the next frames are converted; the Media Foundation encoder keeps
/// at most 3 of them in flight, so a texture is never overwritten while in use.
const RING: usize = 6;

/// Color space bits (`D3D11_VIDEO_PROCESSOR_COLOR_SPACE`): `RGB_Range` 0 is
/// full range (the desktop); `YCbCr_Matrix` 0 is BT.601 and `Nominal_Range`
/// 1 (bits 4-5) is 16-235: what the viewer's shader decodes.
const INPUT_COLOR_SPACE: u32 = 0;
const OUTPUT_COLOR_SPACE: u32 = 1 << 4;

fn platform(context: &str, e: windows::core::Error) -> CaptureError {
    CaptureError::Platform(format!("{context}: {e}"))
}

pub struct Nv12Converter {
    /// Size of the desktop texture.
    source_size: (u32, u32),
    /// Size of the NV12 images: the desktop's, rounded down to even.
    width: u32,
    height: u32,
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    /// Copy of the desktop image the processor reads from.
    source: ID3D11Texture2D,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl Nv12Converter {
    /// A converter for desktop textures of `source_size` in `format`; the
    /// NV12 images are that size rounded down to even, as 4:2:0 requires.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        format: DXGI_FORMAT,
        source_size: (u32, u32),
    ) -> Result<Self, CaptureError> {
        let (source_width, source_height) = source_size;
        let (width, height) = (crate::even(source_width), crate::even(source_height));
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| platform("sem processador de vídeo D3D11", e))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| platform("ID3D11VideoContext", e))?;

        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: source_width,
            InputHeight: source_height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|e| platform("CreateVideoProcessorEnumerator", e))?;
        check_format(
            &enumerator,
            format,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0,
        )?;
        check_format(
            &enumerator,
            DXGI_FORMAT_NV12,
            D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0,
        )?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| platform("CreateVideoProcessor", e))?;

        let source = texture(
            device,
            format,
            source_width,
            source_height,
            (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        )?;
        let input = input_view(&video_device, &enumerator, &source)?;
        // Some encoders want to read their input as a shader resource; not
        // every driver allows that on NV12, so fall back to render target only.
        let render = D3D11_BIND_RENDER_TARGET.0 as u32;
        let readable = render | D3D11_BIND_SHADER_RESOURCE.0 as u32;
        let bind = if texture(device, DXGI_FORMAT_NV12, width, height, readable).is_ok() {
            readable
        } else {
            render
        };
        let outputs = (0..RING)
            .map(|_| {
                let nv12 = texture(device, DXGI_FORMAT_NV12, width, height, bind)?;
                let view = output_view(&video_device, &enumerator, &nv12)?;
                Ok((nv12, view))
            })
            .collect::<Result<Vec<_>, CaptureError>>()?;

        let full = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // No denoising, edge enhancement or the like: text must stay sharp.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: INPUT_COLOR_SPACE,
                },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: OUTPUT_COLOR_SPACE,
                },
            );
        }

        Ok(Self {
            source_size,
            width,
            height,
            context: context.clone(),
            video_context,
            processor,
            source,
            input,
            outputs,
            next: 0,
        })
    }

    /// Size of the desktop textures this converter takes.
    pub fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    /// Size of the NV12 images.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Converts the desktop image into the next NV12 texture of the ring.
    pub fn convert(&mut self, desktop: &ID3D11Texture2D) -> Result<ID3D11Texture2D, CaptureError> {
        let (nv12, view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        unsafe { self.context.CopyResource(&self.source, desktop) };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
            ..Default::default()
        };
        let streams = [stream];
        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, view, 0, &streams)
        };
        // Release the reference the stream description took.
        let [mut stream] = streams;
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result.map_err(|e| platform("VideoProcessorBlt", e))?;
        Ok(nv12.clone())
    }
}

fn check_format(
    enumerator: &ID3D11VideoProcessorEnumerator,
    format: DXGI_FORMAT,
    needed: i32,
) -> Result<(), CaptureError> {
    let support = unsafe { enumerator.CheckVideoProcessorFormat(format) }
        .map_err(|e| platform("CheckVideoProcessorFormat", e))?;
    if support & needed as u32 == 0 {
        return Err(CaptureError::Platform(format!(
            "o processador de vídeo não aceita {format:?}"
        )));
    }
    Ok(())
}

fn texture(
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    width: u32,
    height: u32,
    bind: u32,
) -> Result<ID3D11Texture2D, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| platform("CreateTexture2D", e))?;
    texture.ok_or_else(|| CaptureError::Platform("CreateTexture2D: sem textura".into()))
}

fn input_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorInputView, CaptureError> {
    let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPIV {
                MipSlice: 0,
                ArraySlice: 0,
            },
        },
    };
    let mut view = None;
    unsafe {
        video_device.CreateVideoProcessorInputView(texture, enumerator, &desc, Some(&mut view))
    }
    .map_err(|e| platform("CreateVideoProcessorInputView", e))?;
    view.ok_or_else(|| CaptureError::Platform("sem input view".into()))
}

fn output_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
) -> Result<ID3D11VideoProcessorOutputView, CaptureError> {
    let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
        },
    };
    let mut view = None;
    unsafe {
        video_device.CreateVideoProcessorOutputView(texture, enumerator, &desc, Some(&mut view))
    }
    .map_err(|e| platform("CreateVideoProcessorOutputView", e))?;
    view.ok_or_else(|| CaptureError::Platform("sem output view".into()))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
        D3D11_USAGE_STAGING, D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    use super::*;

    /// A device with video support: the GPU, or else WARP (software).
    fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP]
            .into_iter()
            .find_map(|driver: D3D_DRIVER_TYPE| {
                let (mut device, mut context) = (None, None);
                unsafe {
                    D3D11CreateDevice(
                        None,
                        driver,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                }
                .ok()?;
                Some((device?, context?))
            })
    }

    /// Solid BGRA texture of `size`.
    fn solid(device: &ID3D11Device, size: (u32, u32), bgra: [u8; 4]) -> ID3D11Texture2D {
        let pixels = bgra.repeat((size.0 * size.1) as usize);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: size.0 * 4,
            SysMemSlicePitch: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)) }.unwrap();
        texture.unwrap()
    }

    /// First Y, U and V values of an NV12 texture.
    fn first_yuv(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        nv12: &ID3D11Texture2D,
    ) -> (u8, u8, u8) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { nv12.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..desc
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }.unwrap();
        let staging = staging.unwrap();
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopyResource(&staging, nv12);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let base = mapped.pData.cast::<u8>();
            let uv = base.add(mapped.RowPitch as usize * desc.Height as usize);
            let values = (*base, *uv, *uv.add(1));
            context.Unmap(&staging, 0);
            values
        }
    }

    #[test]
    fn converts_to_bt601_limited_nv12() {
        let Some((device, context)) = device() else {
            eprintln!("sem dispositivo D3D11 com vídeo; teste ignorado");
            return;
        };
        // Odd size: the NV12 image is rounded down to even.
        let size = (65, 33);
        let mut converter =
            match Nv12Converter::new(&device, &context, DXGI_FORMAT_B8G8R8A8_UNORM, size) {
                Ok(converter) => converter,
                Err(e) => {
                    eprintln!("sem processador de vídeo ({e}); teste ignorado");
                    return;
                }
            };
        assert_eq!(converter.source_size(), size);
        assert_eq!(converter.size(), (64, 32));

        // Pure red in BT.601 limited range: Y≈81, U≈90, V≈240
        // (BT.709 would give Y≈63; full range Y≈76).
        let red = solid(&device, size, [0, 0, 255, 255]);
        let nv12 = converter.convert(&red).unwrap();
        let (y, u, v) = first_yuv(&device, &context, &nv12);
        assert!((78..=84).contains(&y), "red Y {y}");
        assert!((85..=96).contains(&u), "red U {u}");
        assert!(v >= 232, "red V {v}");

        // White stays within the limited range (Y≈235, not 255).
        let white = solid(&device, size, [255, 255, 255, 255]);
        let (y, u, v) = first_yuv(&device, &context, &converter.convert(&white).unwrap());
        assert!((230..=240).contains(&y), "white Y {y}");
        assert!(
            u.abs_diff(128) <= 3 && v.abs_diff(128) <= 3,
            "white UV {u},{v}"
        );

        // Consecutive frames go to different textures of the ring.
        let next = converter.convert(&red).unwrap();
        assert_ne!(next, nv12);
    }
}
