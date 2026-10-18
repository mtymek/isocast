//! Decoder configuration records (avcC, hvcC) from the Annex B parameter
//! sets libavcodec encoders put in their extradata.

use anyhow::{Context, Result, bail};

/// NAL units in an Annex B stream (three- or four-byte start codes), without
/// their start codes.
pub fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |next| next - 3);
        // A four-byte start code leaves a zero before the next one; so may
        // trailing_zero_8bits.
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&data[start..end]);
        }
    }
    nals
}

/// Whether `nal` is a parameter set: H.264 SPS/PPS, or HEVC VPS/SPS/PPS.
pub fn is_parameter_set(nal: &[u8], hevc: bool) -> bool {
    if hevc {
        matches!((nal[0] >> 1) & 0x3f, 32..=34)
    } else {
        matches!(nal[0] & 0x1f, 7 | 8)
    }
}

/// The payload without emulation prevention bytes (`00 00 03`).
fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Reads bits, most significant first, and Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.at / 8)
            .context("parameter set is truncated")?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(u32::from(bit))
    }

    fn bits(&mut self, count: u32) -> Result<u64> {
        let mut value = 0u64;
        for _ in 0..count {
            value = (value << 1) | u64::from(self.bit()?);
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) {
        self.at += count;
    }

    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("parameter set has an invalid Exp-Golomb code");
            }
        }
        Ok(((1u64 << zeros) - 1 + self.bits(zeros)?) as u32)
    }
}

/// An AVCDecoderConfigurationRecord with four-byte NAL lengths.
pub fn avc_config(nals: &[&[u8]]) -> Result<Vec<u8>> {
    let sps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 7).collect();
    let pps: Vec<&[u8]> = nals.iter().copied().filter(|n| n[0] & 0x1f == 8).collect();
    let first = *sps.first().context("H.264 encoder gave no SPS")?;
    if pps.is_empty() {
        bail!("H.264 encoder gave no PPS");
    }
    if first.len() < 4 {
        bail!("H.264 SPS is truncated");
    }
    let mut out = vec![
        1,
        first[1],
        first[2],
        first[3],
        0xff,
        0xe0 | sps.len() as u8,
    ];
    for nal in &sps {
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out.push(pps.len() as u8);
    for nal in &pps {
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    let profile = first[1];
    if matches!(profile, 100 | 110 | 122 | 144) {
        let payload = rbsp(&first[1..]);
        let mut bits = Bits::new(&payload);
        bits.skip(24);
        bits.ue()?; // seq_parameter_set_id
        let chroma = bits.ue()?;
        if chroma == 3 {
            bits.skip(1);
        }
        let luma_depth = bits.ue()?;
        let chroma_depth = bits.ue()?;
        out.extend_from_slice(&[
            0xfc | chroma as u8,
            0xf8 | luma_depth as u8,
            0xf8 | chroma_depth as u8,
            0,
        ]);
    }
    Ok(out)
}

/// An HEVCDecoderConfigurationRecord with four-byte NAL lengths.
pub fn hevc_config(nals: &[&[u8]]) -> Result<Vec<u8>> {
    let kind = |nal: &[u8]| (nal[0] >> 1) & 0x3f;
    let of = |wanted: u8| -> Vec<&[u8]> {
        nals.iter()
            .copied()
            .filter(|n| n.len() > 2 && kind(n) == wanted)
            .collect()
    };
    let (vps, sps, pps) = (of(32), of(33), of(34));
    let first = *sps.first().context("HEVC encoder gave no SPS")?;
    if vps.is_empty() || pps.is_empty() {
        bail!("HEVC encoder gave no VPS or PPS");
    }
    let payload = rbsp(&first[2..]);
    let mut bits = Bits::new(&payload);
    bits.skip(4); // sps_video_parameter_set_id
    let sub_layers = bits.bits(3)? as u32;
    let nested = bits.bit()?;
    // profile_tier_level: the general part as stored in hvcC.
    let ptl_start = bits.at / 8;
    if !bits.at.is_multiple_of(8) {
        bail!("HEVC SPS: unexpected alignment");
    }
    let general = payload
        .get(ptl_start..ptl_start + 12)
        .context("HEVC SPS is truncated")?
        .to_vec();
    bits.skip(96);
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((bits.bit()?, bits.bit()?));
    }
    if sub_layers > 0 {
        bits.skip(2 * (8 - sub_layers as usize));
    }
    for (profile, level) in present {
        if profile == 1 {
            bits.skip(88);
        }
        if level == 1 {
            bits.skip(8);
        }
    }
    bits.ue()?; // sps_seq_parameter_set_id
    let chroma = bits.ue()?;
    if chroma == 3 {
        bits.skip(1);
    }
    bits.ue()?; // pic_width_in_luma_samples
    bits.ue()?; // pic_height_in_luma_samples
    if bits.bit()? == 1 {
        for _ in 0..4 {
            bits.ue()?;
        }
    }
    let luma_depth = bits.ue()?;
    let chroma_depth = bits.ue()?;

    let mut out = vec![1];
    out.extend_from_slice(&general); // profile, compatibility, constraints, level
    out.extend_from_slice(&[
        0xf0,
        0x00, // min_spatial_segmentation_idc 0
        0xfc, // parallelismType 0
        0xfc | chroma as u8,
        0xf8 | luma_depth as u8,
        0xf8 | chroma_depth as u8,
        0,
        0, // avgFrameRate unknown
        (((sub_layers + 1) as u8) << 3) | ((nested as u8) << 2) | 3,
        3, // arrays: VPS, SPS, PPS
    ]);
    for (nal_type, list) in [(32u8, &vps), (33, &sps), (34, &pps)] {
        out.push(0x80 | nal_type);
        out.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for nal in list.iter() {
            out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            out.extend_from_slice(nal);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        assert_eq!(
            split_annex_b(&stream),
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
    }

    #[test]
    fn removes_emulation_prevention() {
        assert_eq!(rbsp(&[1, 0, 0, 3, 1, 0, 0, 3]), vec![1, 0, 0, 1, 0, 0]);
    }
}
