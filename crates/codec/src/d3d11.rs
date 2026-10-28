//! Reading an NV12 D3D11 texture back into system memory: for encoders that
//! cannot take it on the GPU (OpenH264, or a Media Foundation transform on
//! another adapter or without Direct3D 11 support), and for the frames the
//! hardware decoder outputs.

// Direct3D 11 calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use crate::CodecError;

fn d3d(context: &str, e: windows::core::Error) -> CodecError {
    CodecError::Hardware(format!("{context}: {e}"))
}

/// The planes of a mapped NV12 texture: `stride` bytes per row, the Y
/// plane's `rows` rows followed by `rows / 2` interleaved U/V rows.
pub(crate) struct Nv12Planes<'a> {
    pub y: &'a [u8],
    pub uv: &'a [u8],
    pub stride: usize,
}

/// Reads NV12 textures back, reusing one staging texture.
#[derive(Default)]
pub(crate) struct Readback {
    staging: Option<(ID3D11Texture2D, ID3D11Device, u32, u32)>,
}

impl Readback {
    /// Writes the `width`x`height` NV12 image in `texture` to `out`, tightly
    /// packed (Y plane, then the interleaved U/V plane).
    pub fn read(
        &mut self,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), CodecError> {
        let (w, h) = (width as usize, height as usize);
        self.map(texture, 0, width, height, |planes| {
            out.clear();
            out.reserve(w * h * 3 / 2);
            for (plane, rows) in [(planes.y, h), (planes.uv, h / 2)] {
                for row in plane.chunks(planes.stride).take(rows) {
                    out.extend_from_slice(&row[..w]);
                }
            }
        })
    }

    /// Copies one subresource of an NV12 `texture` (an array slice, for
    /// decoder output) to CPU memory and gives its planes to `f`. The texture
    /// must be at least `width`x`height`.
    pub fn map<R>(
        &mut self,
        texture: &ID3D11Texture2D,
        subresource: u32,
        width: u32,
        height: u32,
        f: impl FnOnce(Nv12Planes<'_>) -> R,
    ) -> Result<R, CodecError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 || desc.Width < width || desc.Height < height {
            return Err(CodecError::InvalidFrame(format!(
                "textura {:?} {}x{} para uma imagem {width}x{height}",
                desc.Format, desc.Width, desc.Height
            )));
        }
        let device = unsafe { texture.GetDevice() }.map_err(|e| d3d("GetDevice", e))?;
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|e| d3d("GetImmediateContext", e))?;
        let staging = self.staging(&device, &desc)?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, subresource, None);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| d3d("Map", e))?;
        }
        let stride = mapped.RowPitch as usize;
        // The UV plane starts right after the texture's full Y plane.
        let rows = desc.Height as usize;
        // SAFETY: the mapped NV12 staging texture holds `rows` Y rows then
        // `rows / 2` UV rows of `RowPitch` bytes, mapped until the Unmap below.
        let all = unsafe {
            std::slice::from_raw_parts(
                mapped.pData.cast::<u8>().cast_const(),
                stride * rows * 3 / 2,
            )
        };
        let (y, uv) = all.split_at(stride * rows);
        let result = f(Nv12Planes { y, uv, stride });
        unsafe { context.Unmap(&staging, 0) };
        Ok(result)
    }

    fn staging(
        &mut self,
        device: &ID3D11Device,
        src: &D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11Texture2D, CodecError> {
        if let Some((texture, on, w, h)) = &self.staging
            && on == device
            && (*w, *h) == (src.Width, src.Height)
        {
            return Ok(texture.clone());
        }
        // One plain texture, even when the source is an array (decoder output).
        let desc = D3D11_TEXTURE2D_DESC {
            MipLevels: 1,
            ArraySize: 1,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..*src
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(|e| d3d("CreateTexture2D (staging)", e))?;
        let texture =
            texture.ok_or_else(|| CodecError::Hardware("sem textura de leitura".into()))?;
        self.staging = Some((texture.clone(), device.clone(), src.Width, src.Height));
        Ok(texture)
    }
}
