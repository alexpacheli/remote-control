//! H.264 encoding and decoding.
//!
//! F1 uses OpenH264 in software on both ends: it builds and behaves the same
//! on Windows, macOS and Linux. Hardware encoders (Media Foundation,
//! VideoToolbox) will sit behind the same API later.

use openh264::OpenH264API;
use openh264::decoder::Decoder;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("H.264: {0}")]
    OpenH264(#[from] openh264::Error),
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
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

#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 8_000_000,
            max_fps: 60.0,
        }
    }
}

/// Output of one [`H264Encoder::encode`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    /// Annex-B access unit.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub struct H264Encoder {
    settings: EncoderSettings,
    /// Recreated whenever the input size changes.
    inner: Option<(Encoder, u32, u32)>,
    yuv: Option<YUVBuffer>,
}

impl H264Encoder {
    pub fn new(settings: EncoderSettings) -> Self {
        Self {
            settings,
            inner: None,
            yuv: None,
        }
    }

    /// The next encoded frame will be a keyframe.
    pub fn force_keyframe(&mut self) {
        if let Some((encoder, ..)) = &mut self.inner {
            encoder.force_intra_frame();
        }
    }

    /// Encodes a tightly packed BGRA image. Width and height must be even.
    ///
    /// Returns `None` when rate control skipped the frame: encode the
    /// (latest) image again soon, or that update never reaches the viewer.
    pub fn encode(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<Option<Encoded>, CodecError> {
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

        if !matches!(self.inner, Some((_, w, h)) if (w, h) == (width, height)) {
            let config = EncoderConfig::new()
                .usage_type(UsageType::ScreenContentRealTime)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(self.settings.bitrate_bps))
                .max_frame_rate(FrameRate::from_hz(self.settings.max_fps))
                // Rate control only holds the bitrate if it may skip frames;
                // the pipeline re-encodes after a skip so no update is lost.
                .skip_frames(true)
                // Not supported for screen content; set explicitly to avoid warnings.
                .adaptive_quantization(false)
                .background_detection(false);
            let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)?;
            self.inner = Some((encoder, width, height));
            self.yuv = None;
        }

        let source = BgraSliceU8::new(bgra, (width as usize, height as usize));
        let yuv = match &mut self.yuv {
            Some(yuv) => {
                yuv.read_bgra8(source);
                yuv
            }
            None => self.yuv.insert(YUVBuffer::from_bgra8_source(source)),
        };

        let (encoder, ..) = self.inner.as_mut().expect("encoder created above");
        let bitstream = encoder.encode(yuv)?;
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Encoded { data, keyframe }))
    }
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
mod tests {
    use super::*;

    /// Solid-colour BGRA image with a brighter block that moves with `t`.
    fn image(w: u32, h: u32, t: u32) -> Vec<u8> {
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
        let mut encoder = H264Encoder::new(EncoderSettings::default());
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
    fn forced_keyframe_and_resize() {
        let mut encoder = H264Encoder::new(EncoderSettings::default());
        encoder.encode(64, 64, &image(64, 64, 0)).unwrap();
        encoder.force_keyframe();
        assert!(
            encoder
                .encode(64, 64, &image(64, 64, 1))
                .unwrap()
                .unwrap()
                .keyframe
        );
        // A new size restarts the stream with a keyframe.
        assert!(
            encoder
                .encode(128, 64, &image(128, 64, 2))
                .unwrap()
                .unwrap()
                .keyframe
        );
    }

    #[test]
    fn rejects_bad_input() {
        let mut encoder = H264Encoder::new(EncoderSettings::default());
        assert!(encoder.encode(63, 64, &[0; 63 * 64 * 4]).is_err());
        assert!(encoder.encode(64, 64, &[0; 10]).is_err());
    }
}
