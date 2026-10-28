use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSlices};

use crate::{
    CodecError, Encoded, EncoderSettings, Image, PixelFormat, Pixels, VideoEncoder, check_frame,
    split_uv,
};

/// OpenH264 on the CPU.
pub struct SoftwareEncoder {
    settings: EncoderSettings,
    /// Recreated whenever the input size changes.
    inner: Option<(Encoder, u32, u32)>,
    yuv: Option<YUVBuffer>,
    /// U and V planes split out of NV12 input.
    u: Vec<u8>,
    v: Vec<u8>,
    /// GPU textures read back into `cpu`.
    #[cfg(windows)]
    readback: crate::d3d11::Readback,
    #[cfg(windows)]
    cpu: Vec<u8>,
}

impl SoftwareEncoder {
    pub fn new(settings: EncoderSettings) -> Self {
        Self {
            settings,
            inner: None,
            yuv: None,
            u: Vec::new(),
            v: Vec::new(),
            #[cfg(windows)]
            readback: Default::default(),
            #[cfg(windows)]
            cpu: Vec::new(),
        }
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn encode(&mut self, image: &Image<'_>) -> Result<Option<Encoded>, CodecError> {
        check_frame(image)?;
        let (width, height) = (image.width, image.height);

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

        let (w, h) = (width as usize, height as usize);
        // Only Windows has a second variant.
        #[cfg_attr(not(windows), allow(clippy::infallible_destructuring_match))]
        let data = match image.pixels {
            Pixels::Memory(data) => data,
            #[cfg(windows)]
            Pixels::D3d11(texture) => {
                self.readback.read(texture, width, height, &mut self.cpu)?;
                &self.cpu
            }
        };
        let (encoder, ..) = self.inner.as_mut().expect("encoder created above");
        let bitstream = match image.format {
            PixelFormat::Bgra => {
                let source = BgraSliceU8::new(data, (w, h));
                let yuv = match &mut self.yuv {
                    Some(yuv) => {
                        yuv.read_bgra8(source);
                        yuv
                    }
                    None => self.yuv.insert(YUVBuffer::from_bgra8_source(source)),
                };
                encoder.encode(yuv)?
            }
            PixelFormat::Nv12 => {
                // OpenH264 takes separate U and V planes (I420).
                let (y, uv) = data.split_at(w * h);
                split_uv(uv, &mut self.u, &mut self.v);
                let planes = YUVSlices::new((y, &self.u, &self.v), (w, h), (w, w / 2, w / 2));
                encoder.encode(&planes)?
            }
        };
        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);
        let data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Encoded { data, keyframe }))
    }

    fn force_keyframe(&mut self) {
        if let Some((encoder, ..)) = &mut self.inner {
            encoder.force_intra_frame();
        }
    }

    fn name(&self) -> String {
        "CPU (OpenH264)".to_owned()
    }

    fn is_hardware(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::image;

    #[test]
    fn forced_keyframe_and_resize() {
        let mut encoder = SoftwareEncoder::new(EncoderSettings::default());
        encoder
            .encode(&Image::bgra(64, 64, &image(64, 64, 0)))
            .unwrap();
        encoder.force_keyframe();
        assert!(
            encoder
                .encode(&Image::bgra(64, 64, &image(64, 64, 1)))
                .unwrap()
                .unwrap()
                .keyframe
        );
        // A new size restarts the stream with a keyframe.
        assert!(
            encoder
                .encode(&Image::bgra(128, 64, &image(128, 64, 2)))
                .unwrap()
                .unwrap()
                .keyframe
        );
    }

    #[test]
    fn rejects_bad_input() {
        let mut encoder = SoftwareEncoder::new(EncoderSettings::default());
        assert!(
            encoder
                .encode(&Image::bgra(63, 64, &[0; 63 * 64 * 4]))
                .is_err()
        );
        assert!(encoder.encode(&Image::bgra(64, 64, &[0; 10])).is_err());
    }

    #[test]
    fn output_is_baseline() {
        let mut encoder = SoftwareEncoder::new(EncoderSettings::default());
        let key = encoder
            .encode(&Image::bgra(64, 64, &image(64, 64, 0)))
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::h264::sps_profile(&key.data),
            Some(crate::h264::PROFILE_BASELINE)
        );
    }
}
