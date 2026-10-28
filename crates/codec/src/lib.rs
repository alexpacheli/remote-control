//! H.264 encoding and decoding.
//!
//! Encoders sit behind [`VideoEncoder`]:
//! - [`SoftwareEncoder`]: OpenH264, identical on every OS;
//! - a hardware encoder on Windows (a Media Foundation transform from the
//!   GPU vendor: NVIDIA, AMD or Intel);
//! - a hardware encoder on macOS (VideoToolbox, on the Mac's media engine).
//!
//! [`open_encoder`] picks one; in `Auto` mode a failing hardware encoder is
//! replaced by the software one on the fly. Decoding is OpenH264 everywhere,
//! which only understands the Baseline profile: hardware encoders are
//! configured (and checked) to produce it.

use openh264::decoder::Decoder;
use openh264::formats::YUVSource;

#[cfg(windows)]
mod d3d11;
pub mod h264;
#[cfg(windows)]
mod mf;
mod nv12;
mod software;
#[cfg(target_os = "macos")]
mod vt;

pub use nv12::{bgra_to_nv12, split_uv};
pub use software::SoftwareEncoder;
#[cfg(target_os = "macos")]
pub use vt::{BenchStats, benchmark};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("H.264: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
    #[error("no hardware H.264 encoder available")]
    NoHardware,
    #[error("hardware encoder: {0}")]
    Hardware(String),
}

/// A decoded picture in I420: full-resolution Y plane, quarter-size U and V
/// planes, all tightly packed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YuvFrame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Which encoder to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderPreference {
    /// Hardware when available, software otherwise or after a failure.
    #[default]
    Auto,
    Software,
    /// Hardware only; fails if there is none.
    Hardware,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    pub preference: EncoderPreference,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 8_000_000,
            max_fps: 60.0,
            preference: EncoderPreference::Auto,
        }
    }
}

/// Output of one [`VideoEncoder::encode`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// Annex-B access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Layout of an [`Image`]'s pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Tightly packed BGRA rows (`width * 4` bytes each).
    Bgra,
    /// 4:2:0 YUV (BT.601, limited range): a `width * height` Y plane, then a
    /// half-height plane of interleaved U/V pairs (`width` bytes per row).
    Nv12,
}

/// Where an [`Image`]'s pixels are.
#[derive(Debug, Clone, Copy)]
pub enum Pixels<'a> {
    /// Tightly packed, in system memory.
    Memory(&'a [u8]),
    /// Windows: an NV12 texture, encoded without leaving the GPU when the
    /// encoder can (otherwise read back). Its device must have video
    /// support and multithread protection.
    #[cfg(windows)]
    D3d11(&'a windows::Win32::Graphics::Direct3D11::ID3D11Texture2D),
}

/// An image to encode. Width and height must be even.
#[derive(Debug, Clone, Copy)]
pub struct Image<'a> {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pixels: Pixels<'a>,
}

impl<'a> Image<'a> {
    pub fn bgra(width: u32, height: u32, data: &'a [u8]) -> Self {
        Self {
            width,
            height,
            format: PixelFormat::Bgra,
            pixels: Pixels::Memory(data),
        }
    }

    pub fn nv12(width: u32, height: u32, data: &'a [u8]) -> Self {
        Self {
            width,
            height,
            format: PixelFormat::Nv12,
            pixels: Pixels::Memory(data),
        }
    }

    /// The pixels if they are in system memory.
    pub fn memory(&self) -> Option<&'a [u8]> {
        match self.pixels {
            Pixels::Memory(data) => Some(data),
            #[cfg(windows)]
            Pixels::D3d11(_) => None,
        }
    }
}

pub trait VideoEncoder {
    /// Encodes one image, in either pixel format.
    ///
    /// Returns `None` when no output is ready for this image (rate control
    /// skipped it, or the encoder is still working on it): encode the latest
    /// image again soon, or that update never reaches the viewer.
    fn encode(&mut self, image: &Image<'_>) -> Result<Option<Encoded>, CodecError>;

    /// The next encoded frame will be a keyframe.
    fn force_keyframe(&mut self);

    /// Frames went in whose output has not come out yet. The caller must
    /// encode again (the latest image) even if the screen does not change,
    /// or the last update stays inside the encoder.
    fn pending(&self) -> bool {
        false
    }

    /// Collects output for frames already given, without new input, waiting
    /// briefly for it. Call when [`VideoEncoder::pending`] and the screen
    /// has not changed.
    fn drain(&mut self) -> Result<Option<Encoded>, CodecError> {
        Ok(None)
    }

    /// Human-readable description, e.g. "GPU (NVIDIA H.264 Encoder MFT)".
    fn name(&self) -> String;

    /// Whether frames are encoded by the GPU.
    fn is_hardware(&self) -> bool;
}

/// Opens the encoder chosen by `settings.preference`.
pub fn open_encoder(settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>, CodecError> {
    match settings.preference {
        EncoderPreference::Software => Ok(Box::new(SoftwareEncoder::new(settings))),
        EncoderPreference::Hardware => hardware(settings),
        EncoderPreference::Auto => Ok(Box::new(AutoEncoder {
            hardware: hardware(settings)
                .inspect_err(|e| tracing::info!("sem codificação por hardware: {e}"))
                .ok(),
            software: SoftwareEncoder::new(settings),
        })),
    }
}

fn hardware(settings: EncoderSettings) -> Result<Box<dyn VideoEncoder>, CodecError> {
    #[cfg(windows)]
    {
        Ok(Box::new(mf::MfEncoder::new(settings)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(vt::VtEncoder::new(settings)?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = settings;
        Err(CodecError::NoHardware)
    }
}

/// Hardware encoder with a software fallback.
struct AutoEncoder {
    hardware: Option<Box<dyn VideoEncoder>>,
    /// Created lazily, so its first frame after a fallback is a keyframe.
    software: SoftwareEncoder,
}

impl VideoEncoder for AutoEncoder {
    fn encode(&mut self, image: &Image<'_>) -> Result<Option<Encoded>, CodecError> {
        if let Some(hardware) = &mut self.hardware {
            match hardware.encode(image) {
                Ok(encoded) => return Ok(encoded),
                Err(e) => {
                    tracing::warn!("codificação por hardware falhou ({e}); usando software");
                    self.hardware = None;
                }
            }
        }
        self.software.encode(image)
    }

    fn force_keyframe(&mut self) {
        match &mut self.hardware {
            Some(hardware) => hardware.force_keyframe(),
            None => self.software.force_keyframe(),
        }
    }

    fn pending(&self) -> bool {
        self.hardware.as_ref().is_some_and(|h| h.pending())
    }

    fn drain(&mut self) -> Result<Option<Encoded>, CodecError> {
        let Some(hardware) = &mut self.hardware else {
            return Ok(None);
        };
        hardware.drain().inspect_err(|e| {
            tracing::warn!("codificação por hardware falhou ({e}); usando software");
            self.hardware = None;
        })
    }

    fn name(&self) -> String {
        match &self.hardware {
            Some(hardware) => hardware.name(),
            None => self.software.name(),
        }
    }

    fn is_hardware(&self) -> bool {
        self.hardware.is_some()
    }
}

/// Error for a hardware encoder whose output the viewer cannot decode.
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn profile_error(profile: u8) -> CodecError {
    CodecError::Hardware(format!(
        "produz perfil H.264 {profile}, o visualizador precisa de Baseline ({})",
        h264::PROFILE_BASELINE
    ))
}

#[cfg(any(windows, target_os = "macos"))]
/// Adds `next` to the outputs collected in `ready`. Several access units in
/// one payload decode in order, so none is dropped, except that a keyframe
/// makes everything before it unnecessary: the payload then starts with it,
/// and its keyframe flag stays truthful.
pub(crate) fn merge(ready: &mut Option<Encoded>, next: Option<Encoded>) {
    match (ready.as_mut(), next) {
        (_, None) => {}
        (None, Some(next)) => *ready = Some(next),
        (Some(_), Some(next)) if next.keyframe => *ready = Some(next),
        (Some(ready), Some(next)) => ready.data.extend_from_slice(&next.data),
    }
}

fn check_frame(image: &Image<'_>) -> Result<(), CodecError> {
    let (width, height) = (image.width, image.height);
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CodecError::InvalidFrame(format!(
            "{width}x{height} must be even and non-zero"
        )));
    }
    let Some(data) = image.memory() else {
        // GPU textures are only produced in NV12.
        return match image.format {
            PixelFormat::Nv12 => Ok(()),
            PixelFormat::Bgra => Err(CodecError::InvalidFrame("BGRA texture".into())),
        };
    };
    let pixels = width as usize * height as usize;
    let expected = match image.format {
        PixelFormat::Bgra => pixels * 4,
        PixelFormat::Nv12 => pixels * 3 / 2,
    };
    if data.len() != expected {
        return Err(CodecError::InvalidFrame(format!(
            "{} bytes for {width}x{height} {:?}",
            data.len(),
            image.format
        )));
    }
    Ok(())
}

pub struct H264Decoder {
    inner: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            inner: Decoder::new()?,
        })
    }

    /// Decodes one access unit. `None` means more data is needed.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<YuvFrame>, CodecError> {
        let Some(picture) = self.inner.decode(data)? else {
            return Ok(None);
        };
        let (w, h) = picture.dimensions();
        let (y_stride, u_stride, v_stride) = picture.strides();
        Ok(Some(YuvFrame {
            width: w as u32,
            height: h as u32,
            y: pack(picture.y(), y_stride, w, h),
            u: pack(picture.u(), u_stride, w / 2, h / 2),
            v: pack(picture.v(), v_stride, w / 2, h / 2),
        }))
    }
}

/// Copies `rows` rows of `width` bytes out of a plane with the given stride.
fn pack(plane: &[u8], stride: usize, width: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * rows);
    for row in plane.chunks(stride).take(rows) {
        out.extend_from_slice(&row[..width]);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Solid-colour BGRA image with a brighter block that moves with `t`.
    pub(crate) fn image(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let block = (x / 16 + t).is_multiple_of(4) && y < h / 2;
                let v = if block { 230 } else { 40 };
                data.extend_from_slice(&[v, v, v, 255]);
            }
        }
        data
    }

    /// [`image`] converted to NV12, as the macOS capture delivers it.
    pub(crate) fn nv12_image(w: u32, h: u32, t: u32) -> Vec<u8> {
        let mut nv12 = Vec::new();
        bgra_to_nv12(w, h, &image(w, h, t), &mut None, &mut nv12);
        nv12
    }

    #[test]
    fn software_takes_nv12() {
        let (w, h) = (320, 240);
        let mut encoder = SoftwareEncoder::new(EncoderSettings::default());
        let mut decoder = H264Decoder::new().unwrap();
        let nv12 = nv12_image(w, h, 0);
        let encoded = encoder.encode(&Image::nv12(w, h, &nv12)).unwrap().unwrap();
        assert!(encoded.keyframe);
        let frame = decoder.decode(&encoded.data).unwrap().unwrap();
        // Same luma as the source NV12 (block 230 → ~213, background 40 → ~51).
        let (block, background) = (frame.y[0], frame.y[(h as usize - 1) * w as usize]);
        assert!(block.abs_diff(nv12[0]) <= 6, "block {block} vs {}", nv12[0]);
        assert!((45..=58).contains(&background), "background {background}");
        // Wrong sizes are rejected for NV12 too.
        assert!(encoder.encode(&Image::nv12(w, h, &nv12[1..])).is_err());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (320, 240);
        let mut encoder = open_encoder(EncoderSettings::default()).unwrap();
        let mut decoder = H264Decoder::new().unwrap();

        let mut decoded = 0;
        for t in 0..10 {
            let encoded = encoder
                .encode(&Image::bgra(w, h, &image(w, h, t)))
                .unwrap()
                .unwrap();
            assert_eq!(encoded.keyframe, t == 0, "frame {t}");
            if let Some(frame) = decoder.decode(&encoded.data).unwrap() {
                assert_eq!((frame.width, frame.height), (w, h));
                assert_eq!(frame.y.len(), (w * h) as usize);
                assert_eq!(frame.u.len(), (w * h / 4) as usize);
                // Background luma stays close to its source value (40 → ~51 in BT.601).
                let corner = frame.y[(h as usize - 1) * w as usize];
                assert!((35..=70).contains(&corner), "luma {corner}");
                decoded += 1;
            }
        }
        assert!(decoded >= 9, "decoded {decoded} of 10");
    }

    #[test]
    fn auto_without_hardware_is_software() {
        let encoder = open_encoder(EncoderSettings::default()).unwrap();
        if !cfg!(any(windows, target_os = "macos")) {
            assert!(!encoder.is_hardware());
        }
        // Without a hardware encoder (or on a machine whose one failed to
        // open), Auto must have fallen back to OpenH264.
        if !encoder.is_hardware() {
            assert!(encoder.name().contains("OpenH264"));
        }
    }

    #[test]
    fn forced_hardware_fails_without_it() {
        let settings = EncoderSettings {
            preference: EncoderPreference::Hardware,
            ..Default::default()
        };
        match open_encoder(settings) {
            // Only where a hardware encoder can exist, and then it is one.
            Ok(encoder) => assert!(
                cfg!(any(windows, target_os = "macos")) && encoder.is_hardware(),
                "{}",
                encoder.name()
            ),
            Err(CodecError::NoHardware) => {}
            // Platforms with hardware encoders may also report why it failed.
            Err(e) => assert!(
                cfg!(any(windows, target_os = "macos")) && matches!(e, CodecError::Hardware(_)),
                "{e}"
            ),
        }
    }
}
