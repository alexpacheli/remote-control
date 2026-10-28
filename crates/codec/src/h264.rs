//! Minimal H.264 Annex-B inspection.

/// `profile_idc` of the Baseline profile, the only one the decoder supports.
pub const PROFILE_BASELINE: u8 = 66;

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
/// Access unit delimiter.
pub const NAL_AUD: u8 = 9;

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

/// `nal_unit_type` of a NAL unit (start code removed).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

/// What one pass over an access unit found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Scan {
    pub has_idr: bool,
    pub has_sps: bool,
    /// `profile_idc` of the first SPS, if any.
    pub profile: Option<u8>,
}

/// Inspects the access unit in a single pass (cheaper than calling the
/// individual helpers in a row on large keyframes).
pub fn scan(data: &[u8]) -> Scan {
    let mut out = Scan::default();
    for nal in nal_units(data) {
        match nal_type(nal) {
            Some(NAL_IDR) => out.has_idr = true,
            Some(NAL_SPS) => {
                out.has_sps = true;
                if out.profile.is_none() {
                    out.profile = nal.get(1).copied();
                }
            }
            _ => {}
        }
    }
    out
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

/// Converts length-prefixed NAL units (AVCC, as VideoToolbox outputs them)
/// to Annex-B, appending them to `out`. `length_size` is the size of each
/// length prefix (1, 2 or 4 bytes). Returns `None` if the data is malformed.
pub fn avcc_to_annex_b(data: &[u8], length_size: usize, out: &mut Vec<u8>) -> Option<()> {
    if !matches!(length_size, 1 | 2 | 4) {
        return None;
    }
    let mut rest = data;
    while !rest.is_empty() {
        let (prefix, tail) = rest.split_at_checked(length_size)?;
        let len = prefix
            .iter()
            .fold(0usize, |len, &byte| (len << 8) | usize::from(byte));
        let (nal, tail) = tail.split_at_checked(len)?;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        rest = tail;
    }
    Some(())
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
    fn scan_matches_individual_helpers() {
        let data = [
            0, 0, 0, 1, 0x67, 66, 0xC0, // SPS Baseline
            0, 0, 0, 1, 0x65, 0x88, // IDR
        ];
        let s = scan(&data);
        assert_eq!(s.has_idr, has_idr(&data));
        assert_eq!(s.has_sps, has_sps(&data));
        assert_eq!(s.profile, sps_profile(&data));
        assert_eq!(s.profile, Some(PROFILE_BASELINE));
        assert_eq!(scan(&[0, 0, 1, 0x41]), Scan::default());
    }

    #[test]
    fn avcc_becomes_annex_b() {
        let avcc = [
            0, 0, 0, 3, 0x67, 66, 0xC0, // SPS
            0, 0, 0, 2, 0x65, 0x88, // IDR
        ];
        let mut out = vec![0xAA];
        avcc_to_annex_b(&avcc, 4, &mut out).unwrap();
        assert_eq!(
            out,
            [0xAA, 0, 0, 0, 1, 0x67, 66, 0xC0, 0, 0, 0, 1, 0x65, 0x88]
        );
        assert_eq!(scan(&out[1..]).profile, Some(PROFILE_BASELINE));

        let mut short = Vec::new();
        avcc_to_annex_b(&[2, 0x41, 0x9A], 1, &mut short).unwrap();
        assert_eq!(short, [0, 0, 0, 1, 0x41, 0x9A]);
    }

    #[test]
    fn malformed_avcc_is_rejected() {
        let mut out = Vec::new();
        // Length runs past the data.
        assert!(avcc_to_annex_b(&[0, 0, 0, 9, 0x65], 4, &mut out).is_none());
        // Truncated length prefix.
        assert!(avcc_to_annex_b(&[0, 0], 4, &mut out).is_none());
        assert!(avcc_to_annex_b(&[1, 0x65], 3, &mut out).is_none());
        assert!(avcc_to_annex_b(&[], 4, &mut out).is_some());
    }

    #[test]
    fn high_profile_is_detected() {
        let data = [0, 0, 0, 1, 0x67, 100, 0x00, 0x28];
        assert_eq!(sps_profile(&data), Some(100));
    }
}
