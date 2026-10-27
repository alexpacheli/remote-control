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

pub mod h264;
#[cfg(windows)]
mod mf;
mod nv12;
mod software;
#[cfg(target_os = "macos")]
mod vt;

pub use nv12::bgra_to_nv12;
pub use software::SoftwareEncoder;

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

pub trait VideoEncoder {
    /// Encodes a tightly packed BGRA image. Width and height must be even.
    ///
    /// Returns `None` when no output is ready for this image (rate control
    /// skipped it, or the encoder is still working on it): encode the latest
    /// image again soon, or that update never reaches the viewer.
    fn encode(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Option<Encoded>, CodecError>;

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
    fn encode(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Option<Encoded>, CodecError> {
        if let Some(hardware) = &mut self.hardware {
            match hardware.encode(width, height, bgra) {
                Ok(encoded) => return Ok(encoded),
                Err(e) => {
                    tracing::warn!("codificação por hardware falhou ({e}); usando software");
                    self.hardware = None;
                }
            }
        }
        self.software.encode(width, height, bgra)
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

fn check_frame(width: u32, height: u32, bgra: &[u8]) -> Result<(), CodecError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(CodecError::InvalidFrame(format!(
            "{width}x{height} must be even and non-zero"
        )));
    }
    if bgra.len() != (width * height * 4) as usize {
        return Err(CodecError::InvalidFrame(format!(
            "{} bytes for {width}x{height} BGRA",
            bgra.len()
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

    #[test]
    fn encode_decode_roundtrip() {
        let (w, h) = (320, 240);
        let mut encoder = open_encoder(EncoderSettings::default()).unwrap();
        let mut decoder = H264Decoder::new().unwrap();

        let mut decoded = 0;
        for t in 0..10 {
            let encoded = encoder.encode(w, h, &image(w, h, t)).unwrap().unwrap();
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
            assert!(encoder.name().contains("OpenH264"));
        }
    }

    #[test]
    fn forced_hardware_fails_without_it() {
        if !cfg!(any(windows, target_os = "macos")) {
            let settings = EncoderSettings {
                preference: EncoderPreference::Hardware,
                ..Default::default()
            };
            assert!(matches!(
                open_encoder(settings),
                Err(CodecError::NoHardware)
            ));
        }
    }
}
