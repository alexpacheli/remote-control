//! Reading an NV12 D3D11 texture back into system memory, for encoders that
//! cannot take it on the GPU (OpenH264, or a Media Foundation transform on
//! another adapter or without Direct3D 11 support).

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
            context.CopyResource(&staging, texture);
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| d3d("Map", e))?;
        }
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData.cast::<u8>().cast_const();
        let (w, h) = (width as usize, height as usize);
        out.clear();
        out.reserve(w * h * 3 / 2);
        // The UV plane starts right after the texture's full Y plane.
        let uv_offset = pitch * desc.Height as usize;
        for (offset, rows) in [(0, h), (uv_offset, h / 2)] {
            for y in 0..rows {
                // SAFETY: the mapped NV12 staging texture holds `desc.Height`
                // Y rows then `desc.Height / 2` UV rows, each `RowPitch >= w`
                // bytes, and stays mapped until the Unmap below.
                let row = unsafe { std::slice::from_raw_parts(base.add(offset + y * pitch), w) };
                out.extend_from_slice(row);
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        Ok(())
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
        let desc = D3D11_TEXTURE2D_DESC {
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
