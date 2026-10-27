//! Minimal H.264 Annex-B inspection.

/// `profile_idc` of the Baseline profile, the only one the decoder supports.
pub const PROFILE_BASELINE: u8 = 66;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;

/// NAL units of an Annex-B stream (start codes removed).
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    // Positions right after each 00 00 01 start code (also covers 00 00 00 01).
    let starts: Vec<usize> = data
        .windows(3)
        .enumerate()
        .filter(|(_, w)| *w == [0, 0, 1])
        .map(|(i, _)| i + 3)
        .collect();
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&next| {
            // Trim the next start code, including a 4-byte one's extra zero.
            let mut end = next - 3;
            if end > 0 && data[end - 1] == 0 {
                end -= 1;
            }
            end
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &data[start..end])
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

/// Whether the access unit contains an IDR (keyframe) slice.
pub fn has_idr(data: &[u8]) -> bool {
    nal_units(data).any(|nal| nal_type(nal) == Some(NAL_IDR))
}

/// Whether the access unit carries a sequence parameter set.
pub fn has_sps(data: &[u8]) -> bool {
    nal_units(data).any(|nal| nal_type(nal) == Some(NAL_SPS))
}

/// `profile_idc` from the first SPS in the access unit.
pub fn sps_profile(data: &[u8]) -> Option<u8> {
    nal_units(data)
        .find(|nal| nal_type(nal) == Some(NAL_SPS))
        .and_then(|sps| sps.get(1).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nal_units() {
        let data = [
            0, 0, 0, 1, 0x67, 66, 0xC0, // SPS (4-byte start code)
            0, 0, 1, 0x68, 0xCE, // PPS (3-byte start code)
            0, 0, 0, 1, 0x65, 0x88, 0x84, // IDR slice
        ];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(
            nals,
            vec![
                &[0x67, 66, 0xC0][..],
                &[0x68, 0xCE][..],
                &[0x65, 0x88, 0x84][..]
            ]
        );
        assert!(has_sps(&data));
        assert!(has_idr(&data));
        assert_eq!(sps_profile(&data), Some(PROFILE_BASELINE));
    }

    #[test]
    fn slice_without_sps() {
        let data = [0, 0, 0, 1, 0x41, 0x9A, 0x00];
        assert!(!has_sps(&data));
        assert!(!has_idr(&data));
        assert_eq!(sps_profile(&data), None);
        assert_eq!(nal_units(&[]).count(), 0);
    }

    #[test]
    fn high_profile_is_detected() {
        let data = [0, 0, 0, 1, 0x67, 100, 0x00, 0x28];
        assert_eq!(sps_profile(&data), Some(100));
    }
}
