//! H.264 and HEVC bitstreams read on the host, for NVDEC.
//!
//! NVC7B0 parses slice data itself but not the parameter sets or the
//! reference bookkeeping: its picture setup (`nvdec_h264_pic_s`,
//! `nvdec_hevc_pic_s`) carries the sequence and picture parameters, the
//! picture's order count and its decoded-picture buffer, which NVIDIA's
//! driver fills from its own parser. This module is that parser: Annex B
//! NAL units ([`nal_units`]), Exp-Golomb reads over the RBSP ([`Bits`]),
//! and per codec ([`h264`], [`hevc`]) the parameter sets, the slice headers
//! the setup needs, the picture order count and the reference pictures, to
//! the decoded-picture buffer slot each picture is written to.
//!
//! It is integer arithmetic only, as the soft-float host wants, and every
//! value a setup takes from it is checked against the field's range first:
//! a malformed setup can wedge GSP-RM. What it does not take (interlaced
//! pictures, MVC, scaling matrices, H.264 frame-number gaps, HEVC tiles and
//! range extensions) is refused as [`Error::Unsupported`], never guessed.
use alloc::vec::Vec;

pub mod h264;
pub mod hevc;

/// Why a stream was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A syntax element ran past the end of its NAL unit.
    Truncated,
    /// A value outside the range the standard allows.
    Invalid(&'static str),
    /// Valid, but not something this decoder takes yet.
    Unsupported(&'static str),
    /// A slice names a parameter set or a reference that is not there.
    Missing(&'static str),
}

impl Error {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Truncated => "a syntax element runs past its NAL unit",
            Self::Invalid(why) | Self::Unsupported(why) | Self::Missing(why) => why,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::Invalid(_) => "invalid",
            Self::Unsupported(_) => "unsupported",
            Self::Missing(_) => "missing",
        }
    }
}

pub type Result<T> = core::result::Result<T, Error>;

/// One NAL unit of an Annex B stream: its header and payload as they are
/// in the stream (emulation prevention bytes kept), without its start code
/// and trailing zero bytes, and where it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nal<'a> {
    pub bytes: &'a [u8],
    /// The offset in the stream of its first header byte.
    pub offset: usize,
}

impl Nal<'_> {
    /// H.264's `nal_unit_type`.
    pub fn h264_type(&self) -> u8 {
        self.bytes.first().map_or(0, |b| b & 0x1f)
    }

    /// H.264's `nal_ref_idc`.
    pub fn h264_ref_idc(&self) -> u8 {
        self.bytes.first().map_or(0, |b| (b >> 5) & 3)
    }

    /// HEVC's `nal_unit_type`.
    pub fn hevc_type(&self) -> u8 {
        self.bytes.first().map_or(0, |b| (b >> 1) & 0x3f)
    }

    /// HEVC's `TemporalId`.
    pub fn hevc_temporal_id(&self) -> u8 {
        self.bytes.get(1).map_or(0, |b| (b & 7).saturating_sub(1))
    }
}

/// The NAL units of an Annex B stream, in order. Bytes before the first
/// start code are skipped.
pub fn nal_units(stream: &[u8]) -> Vec<Nal<'_>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        // The next start code's own 00 00 01, then this unit's trailing
        // zeros (and a 4-byte start code's zero_byte) come off its end.
        let mut end = starts.get(k + 1).map_or(stream.len(), |next| next - 3);
        while end > start && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(Nal {
                bytes: &stream[start..end],
                offset: start,
            });
        }
    }
    nals
}

/// The RBSP of `escaped`: each `00 00 03` loses its `03`.
pub fn unescape(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for &byte in escaped {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// The most RBSP bytes [`Bits`] unescapes: headers need far fewer.
const HEADER_BYTES: usize = 4096;

/// A reader of the bits of an RBSP, most significant first, counting the
/// bits read.
pub struct Bits {
    data: Vec<u8>,
    position: usize,
}

impl Bits {
    /// The RBSP of `escaped` (the bytes after the NAL header), up to its
    /// first [`HEADER_BYTES`] bytes.
    pub fn new(escaped: &[u8]) -> Self {
        let mut data = unescape(&escaped[..escaped.len().min(HEADER_BYTES + HEADER_BYTES / 2)]);
        data.truncate(HEADER_BYTES);
        Self { data, position: 0 }
    }

    /// Bits read so far.
    pub fn position(&self) -> usize {
        self.position
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = *self.data.get(self.position / 8).ok_or(Error::Truncated)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(u32::from(bit))
    }

    /// `u(n)`, n at most 32.
    pub fn u(&mut self, n: u32) -> Result<u32> {
        let mut value = 0u64;
        for _ in 0..n {
            value = value << 1 | u64::from(self.bit()?);
        }
        Ok(value as u32)
    }

    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? == 1)
    }

    /// Skips `n` bits.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.position + n > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        self.position += n;
        Ok(())
    }

    /// `ue(v)`, at most 2^32 - 2.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("an Exp-Golomb code longer than 32 bits"));
            }
        }
        let rest = self.u(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(rest)) as u32)
    }

    /// `ue(v)` no larger than `most`.
    pub fn ue_max(&mut self, most: u32, what: &'static str) -> Result<u32> {
        let value = self.ue()?;
        if value > most {
            return Err(Error::Invalid(what));
        }
        Ok(value)
    }

    /// `se(v)`.
    pub fn se(&mut self) -> Result<i32> {
        let code = self.ue()?;
        let magnitude = code.div_ceil(2) as i32;
        Ok(if code % 2 == 1 { magnitude } else { -magnitude })
    }

    /// `se(v)` within `range`.
    pub fn se_in(
        &mut self,
        range: core::ops::RangeInclusive<i32>,
        what: &'static str,
    ) -> Result<i32> {
        let value = self.se()?;
        if !range.contains(&value) {
            return Err(Error::Invalid(what));
        }
        Ok(value)
    }

    /// `more_rbsp_data()`: whether anything but the stop bit and its
    /// trailing zeros is left.
    pub fn more_rbsp_data(&self) -> bool {
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else {
            return false;
        };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.position < stop
    }
}

/// Ceil(Log2(n)), the bits of a `u(v)` index below `n`.
pub fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn start_codes_of_three_and_four_bytes_split_units() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9, 0,
        ];
        let nals = nal_units(&stream);
        assert_eq!(nals.len(), 3);
        assert_eq!((nals[0].bytes, nals[0].offset), (&[0x67, 1, 2][..], 4));
        assert_eq!(nals[1].bytes, &[0x68, 3]);
        assert_eq!(nals[2].bytes, &[0x65, 9]);
        assert_eq!(nals[2].h264_type(), 5);
        assert_eq!(nals[0].h264_ref_idc(), 3);
    }

    #[test]
    fn emulation_prevention_bytes_come_out() {
        assert_eq!(
            unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 3]),
            vec![0, 0, 1, 0, 0, 0, 3]
        );
    }

    #[test]
    fn exp_golomb_reads_as_the_standard_says() {
        // 1 | 010 | 011 | 00100 | 00101: ue 0, 1, 2, then se of codes 3
        // and 4, +2 and -2. As bytes: 1010 0110 0100 0010 1(000 0000).
        let mut bits = Bits::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        assert_eq!(bits.ue(), Ok(0));
        assert_eq!(bits.ue(), Ok(1));
        assert_eq!(bits.ue(), Ok(2));
        assert_eq!(bits.se(), Ok(2));
        assert_eq!(bits.position(), 12);
        assert!(bits.more_rbsp_data());
        assert_eq!(bits.se(), Ok(-2));
        assert!(!bits.more_rbsp_data());
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(8160), 13);
    }

    #[test]
    fn reads_past_the_end_are_refused() {
        let mut bits = Bits::new(&[0]);
        assert_eq!(bits.ue(), Err(Error::Truncated));
        let mut bits = Bits::new(&[0xff]);
        assert_eq!(bits.u(8), Ok(0xff));
        assert_eq!(bits.flag(), Err(Error::Truncated));
    }
}
