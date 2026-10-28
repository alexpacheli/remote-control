//! DXGI Desktop Duplication capture.
//!
//! The duplication hands out a GPU texture per desktop update. The GPU's
//! video processor converts it to NV12 into a texture the hardware encoder
//! reads directly ([`Frame::texture`]), so the image never leaves the GPU.
//! If the driver has no video processor, the texture is instead copied to a
//! CPU-readable staging texture and packed into a BGRA [`Frame`]. The
//! cursor is not part of the image: the viewer draws its own (zero-latency)
//! cursor.

// COM / Direct3D calls are unsafe by nature; each block below is a plain API call.
#![allow(unsafe_code)]

use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::core::Interface;

use crate::gpu_nv12::Nv12Converter;
use crate::{CaptureError, Capturer, Frame, PixelFormat, Rect, even};

fn platform(e: windows::core::Error) -> CaptureError {
    CaptureError::Platform(e.to_string())
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// `None` while duplication is unavailable (secure desktop, mode change).
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// GPU conversion to NV12, for the current desktop size.
    converter: Option<Nv12Converter>,
    /// No video support: always copy through system memory.
    cpu_only: bool,
    /// Desktop size at which the GPU conversion failed; it is tried again
    /// after a size change or a new duplication.
    gpu_failed_at: Option<(u32, u32)>,
    frame: Option<Frame>,
    rect: Rect,
    copy_time: Duration,
}

impl DxgiCapturer {
    pub fn new(index: usize) -> Result<Self, CaptureError> {
        // Work in physical pixels: capture size and input coordinates must agree.
        // Fails harmlessly if the process is already DPI aware.
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

        let (adapter, output) = find_output(index)?;
        // Video support lets the GPU convert to NV12 and the hardware
        // encoder use the same device; without it, the CPU path still works.
        let (device, context, video) = match create_device(
            &adapter,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        ) {
            Ok((device, context)) => (device, context, true),
            Err(e) => {
                tracing::info!("D3D11 sem suporte a vídeo ({e}); captura pela CPU");
                let (device, context) = create_device(&adapter, D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
                (device, context, false)
            }
        };
        // The encoder uses the device from its own threads.
        if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
            let _ = unsafe { multithread.SetMultithreadProtected(true) };
        }
        let output: IDXGIOutput1 = output.cast().map_err(platform)?;
        // `HEXT_ZERO_COPY=0` forces the copy through system memory, to
        // compare against the GPU path on the same machine.
        let disabled = std::env::var("HEXT_ZERO_COPY").is_ok_and(|v| v == "0");
        if disabled {
            tracing::info!("HEXT_ZERO_COPY=0: captura pela memória do sistema");
        }

        let mut capturer = Self {
            device,
            context,
            output,
            duplication: None,
            staging: None,
            converter: None,
            cpu_only: !video || disabled,
            gpu_failed_at: None,
            frame: None,
            rect: Rect::default(),
            copy_time: Duration::ZERO,
        };
        capturer.duplicate()?;
        Ok(capturer)
    }

    fn duplicate(&mut self) -> Result<(), CaptureError> {
        let desc = unsafe { self.output.GetDesc() }.map_err(platform)?;
        let r = desc.DesktopCoordinates;
        self.rect = Rect {
            x: r.left,
            y: r.top,
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
        };
        let duplication = unsafe { self.output.DuplicateOutput(&self.device) }.map_err(platform)?;
        // A new duplication (mode change, secure desktop over): worth
        // trying the GPU conversion again.
        self.gpu_failed_at = None;
        let rotation = unsafe { duplication.GetDesc() }.Rotation;
        if !matches!(
            rotation,
            DXGI_MODE_ROTATION_IDENTITY | DXGI_MODE_ROTATION_UNSPECIFIED
        ) {
            // TODO: rotate the image; until then the view is sideways and the
            // mouse mapping is off on portrait monitors.
            tracing::warn!("monitor girado ({rotation:?}): ainda não suportado");
        }
        self.duplication = Some(duplication);
        Ok(())
    }

    /// Copies the acquired desktop texture into `self.frame`.
    fn copy(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<bool, CaptureError> {
        // Only the pointer moved: the image is unchanged.
        if info.LastPresentTime == 0 {
            return Ok(false);
        }
        let texture: ID3D11Texture2D = resource
            .ok_or_else(|| CaptureError::Platform("frame without a texture".into()))?
            .cast()
            .map_err(platform)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::Platform(format!(
                "unsupported desktop format {:?}",
                desc.Format
            )));
        }

        let size = (desc.Width, desc.Height);
        if !self.cpu_only && self.gpu_failed_at != Some(size) {
            match self.convert(&texture, &desc) {
                Ok(()) => return Ok(true),
                Err(e) => {
                    tracing::warn!("conversão na GPU indisponível ({e}); captura pela CPU");
                    self.gpu_failed_at = Some(size);
                    self.converter = None;
                }
            }
        }

        let staging = self.staging_texture(&desc)?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.CopyResource(&staging, &texture);
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(platform)?;
        }

        let (width, height) = (even(desc.Width), even(desc.Height));
        let pitch = mapped.RowPitch as usize;
        let row_len = width as usize * 4;
        let frame = self.frame.get_or_insert_with(|| Frame {
            width: 0,
            height: 0,
            format: PixelFormat::Bgra,
            data: Vec::new(),
            texture: None,
        });
        frame.width = width;
        frame.height = height;
        frame.format = PixelFormat::Bgra;
        frame.texture = None;
        frame.data.resize(row_len * height as usize, 0);
        // SAFETY: the mapped staging texture holds `desc.Height` rows of `RowPitch`
        // bytes, and stays mapped until the Unmap below.
        let src = unsafe {
            std::slice::from_raw_parts(mapped.pData as *const u8, pitch * desc.Height as usize)
        };
        for (dst_row, src_row) in frame.data.chunks_exact_mut(row_len).zip(src.chunks(pitch)) {
            dst_row.copy_from_slice(&src_row[..row_len]);
        }
        unsafe { self.context.Unmap(&staging, 0) };
        Ok(true)
    }

    /// Converts the desktop texture to NV12 on the GPU into `self.frame`.
    fn convert(
        &mut self,
        texture: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
    ) -> Result<(), CaptureError> {
        let size = (desc.Width, desc.Height);
        let converter = match &mut self.converter {
            Some(converter) if converter.source_size() == size => converter,
            _ => self.converter.insert(Nv12Converter::new(
                &self.device,
                &self.context,
                desc.Format,
                size,
            )?),
        };
        let nv12 = converter.convert(texture)?;
        let (width, height) = converter.size();
        let frame = self.frame.get_or_insert_with(|| Frame {
            width,
            height,
            format: PixelFormat::Nv12,
            data: Vec::new(),
            texture: None,
        });
        frame.width = width;
        frame.height = height;
        frame.format = PixelFormat::Nv12;
        frame.data.clear();
        frame.texture = Some(nv12);
        Ok(())
    }

    fn staging_texture(
        &mut self,
        src: &D3D11_TEXTURE2D_DESC,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        if let Some((texture, w, h)) = &self.staging
            && (*w, *h) == (src.Width, src.Height)
        {
            return Ok(texture.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.Width,
            Height: src.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(platform)?;
        let texture = texture.ok_or_else(|| CaptureError::Platform("no staging texture".into()))?;
        self.staging = Some((texture.clone(), src.Width, src.Height));
        Ok(texture)
    }
}

impl Capturer for DxgiCapturer {
    fn next_frame(&mut self, timeout: Duration) -> Result<bool, CaptureError> {
        let Some(duplication) = self.duplication.clone() else {
            // Duplication is refused while the secure desktop (UAC, lock screen)
            // is shown; keep retrying until it comes back.
            if self.duplicate().is_err() {
                std::thread::sleep(timeout.min(Duration::from_millis(100)));
            }
            return Ok(false);
        };

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { duplication.AcquireNextFrame(millis, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Mode change or desktop switch: recreate on the next call.
                self.duplication = None;
                return Ok(false);
            }
            Err(e) => {
                self.duplication = None;
                return Err(platform(e));
            }
        }
        let started = Instant::now();
        let copied = self.copy(&info, resource);
        self.copy_time = started.elapsed();
        let _ = unsafe { duplication.ReleaseFrame() };
        copied
    }

    fn frame(&self) -> Option<&Frame> {
        self.frame.as_ref()
    }

    fn rect(&self) -> Rect {
        self.rect
    }

    fn copy_time(&self) -> Duration {
        self.copy_time
    }
}

fn create_device(
    adapter: &IDXGIAdapter1,
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(platform)?;
    let device = device.ok_or_else(|| CaptureError::Platform("no D3D11 device".into()))?;
    let context = context.ok_or_else(|| CaptureError::Platform("no D3D11 context".into()))?;
    Ok((device, context))
}

/// Finds the `index`-th output across all adapters (0 = first, usually primary).
fn find_output(index: usize) -> Result<(IDXGIAdapter1, IDXGIOutput), CaptureError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(platform)?;
    let mut seen = 0;
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else {
                break;
            };
            if seen == index {
                return Ok((adapter, output));
            }
            seen += 1;
        }
    }
    Err(CaptureError::NoSuchDisplay(index))
}
