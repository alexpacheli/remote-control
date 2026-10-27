use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

/// Converts tightly packed BGRA to NV12 (Y plane, then interleaved U/V),
/// the input format of hardware encoders.
///
/// Uses the same BT.601 limited-range conversion as the software path, so
/// the viewer's shader is right for both. `yuv` is a reusable scratch buffer.
pub fn bgra_to_nv12(
    width: u32,
    height: u32,
    bgra: &[u8],
    yuv: &mut Option<YUVBuffer>,
    out: &mut Vec<u8>,
) {
    let (w, h) = (width as usize, height as usize);
    let source = BgraSliceU8::new(bgra, (w, h));
    let yuv = match yuv {
        Some(buffer) if buffer.dimensions() == (w, h) => {
            buffer.read_bgra8(source);
            buffer
        }
        _ => yuv.insert(YUVBuffer::from_bgra8_source(source)),
    };

    let (y_stride, u_stride, v_stride) = yuv.strides();
    out.clear();
    out.reserve(w * h * 3 / 2);
    for row in yuv.y().chunks(y_stride).take(h) {
        out.extend_from_slice(&row[..w]);
    }
    let rows = yuv
        .u()
        .chunks(u_stride)
        .zip(yuv.v().chunks(v_stride))
        .take(h / 2);
    for (u_row, v_row) in rows {
        for (u, v) in u_row[..w / 2].iter().zip(&v_row[..w / 2]) {
            out.push(*u);
            out.push(*v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_values() {
        // 4x2 image: left half pure red, right half pure blue.
        let (w, h) = (4u32, 2u32);
        let mut bgra = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                bgra.extend_from_slice(if x < 2 {
                    &[0, 0, 255, 255]
                } else {
                    &[255, 0, 0, 255]
                });
            }
        }
        let mut scratch = None;
        let mut nv12 = Vec::new();
        bgra_to_nv12(w, h, &bgra, &mut scratch, &mut nv12);

        assert_eq!(nv12.len(), 4 * 2 + 4);
        // BT.601 limited: red Y≈81, blue Y≈41.
        let (red_y, blue_y) = (nv12[0], nv12[3]);
        assert!((78..=84).contains(&red_y), "red Y {red_y}");
        assert!((38..=44).contains(&blue_y), "blue Y {blue_y}");
        // Chroma pairs: red has V > U, blue has U > V.
        let (red_u, red_v, blue_u, blue_v) = (nv12[8], nv12[9], nv12[10], nv12[11]);
        assert!(red_v > 200 && red_u < 128, "red UV {red_u},{red_v}");
        assert!(blue_u > 200 && blue_v < 128, "blue UV {blue_u},{blue_v}");
    }

    #[test]
    fn reuses_scratch_across_sizes() {
        let mut scratch = None;
        let mut nv12 = Vec::new();
        bgra_to_nv12(4, 2, &[0; 4 * 2 * 4], &mut scratch, &mut nv12);
        bgra_to_nv12(8, 4, &[0; 8 * 4 * 4], &mut scratch, &mut nv12);
        assert_eq!(nv12.len(), 8 * 4 * 3 / 2);
    }
}
