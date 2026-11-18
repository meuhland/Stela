//! Decoded pictures out of NVDEC's surfaces.
//!
//! NVDEC writes its decoded-picture buffer block-linear (`tileFormat` 1,
//! KBL; `gob_height` 0, GOB_2): GOBs of 64 bytes by 8 rows in Turing's line
//! order, two GOBs to a 64x16 block, blocks row-major across the pitch. That
//! is NVENC's surface layout (`nvenc::surface`), which the encoder's
//! reconstructions were read back with on the GPU.
//!
//! The CPU reads a picture back in two steps: [`super::session::Session`]
//! copies each plane's raw bytes out of the BAR1 mapping in address order,
//! and [`picture`] de-tiles them in host memory into the displayed NV12
//! picture: the luma rows, then the interleaved chroma rows, cropped. That
//! is the layout of the golden checksums, which are FFmpeg's `framemd5` of
//! the software decode as `nv12` (`sw-native.framemd5`): the MD5 of the
//! cropped NV12 bytes.
//!
//! Since whether NVDEC used that block height is read off the setups, not
//! seen on the GPU, [`Tiling`] also names the other block heights and a
//! pitch-linear plane: a picture that does not match is tried under each,
//! which tells a wrong layout from a wrong decode in one boot.
use alloc::vec::Vec;

/// How a plane is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tiling {
    /// Block-linear, blocks of this many 8-row GOBs.
    Blocks(u32),
    /// Row after row.
    Pitch,
}

impl Tiling {
    /// What NVDEC writes: GOB_2.
    pub const NVDEC: Self = Self::Blocks(2);
    /// Every layout a mismatch is tried under.
    pub const ALL: [Self; 7] = [
        Self::Blocks(2),
        Self::Blocks(1),
        Self::Blocks(4),
        Self::Blocks(8),
        Self::Blocks(16),
        Self::Blocks(32),
        Self::Pitch,
    ];

    /// Rows a plane of `rows` rows takes.
    pub fn padded_rows(self, rows: u32) -> u32 {
        match self {
            Self::Blocks(gobs) => rows.next_multiple_of(8 * gobs),
            Self::Pitch => rows,
        }
    }

    /// Bytes a plane of `rows` rows of `pitch` bytes takes.
    pub fn plane_bytes(self, pitch: u32, rows: u32) -> usize {
        pitch as usize * self.padded_rows(rows) as usize
    }

    /// A word for the lines.
    pub fn name(self) -> &'static str {
        match self {
            Self::Blocks(1) => "GOB_1",
            Self::Blocks(2) => "GOB_2",
            Self::Blocks(4) => "GOB_4",
            Self::Blocks(8) => "GOB_8",
            Self::Blocks(16) => "GOB_16",
            Self::Blocks(32) => "GOB_32",
            Self::Blocks(_) => "GOB_?",
            Self::Pitch => "pitch",
        }
    }

    /// The offset in a plane of the 16-byte line holding byte `x` (a
    /// multiple of 16) of row `y`.
    pub fn offset(self, x: usize, y: usize, pitch: usize) -> usize {
        match self {
            Self::Pitch => y * pitch + x,
            Self::Blocks(gobs) => {
                let rows = 8 * gobs as usize;
                let block = (y / rows) * (pitch / 64) + x / 64;
                let column = (x % 64) / 16;
                let line = y % 8;
                block * 64 * rows
                    + ((y % rows) / 8) * 512
                    + (column >> 1) * 256
                    + (line >> 2) * 128
                    + (column & 1) * 64
                    + (line & 3) * 16
            }
        }
    }
}

/// A picture's displayed rectangle in its coded frame, in luma samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
}

/// Appends rows `top..top + rows` of a plane at `raw` laid out by `tiling`
/// with `pitch`, bytes `left..left + width` of each, to `out`.
fn rows_of(
    out: &mut Vec<u8>,
    raw: &[u8],
    tiling: Tiling,
    pitch: u32,
    (left, top): (u32, u32),
    (width, rows): (u32, u32),
) {
    let pitch = pitch as usize;
    let mut line = [0u8; 16];
    for y in top as usize..(top + rows) as usize {
        let first = left as usize / 16 * 16;
        let end = (left + width) as usize;
        let mut x = first;
        while x < end {
            let at = tiling.offset(x, y, pitch);
            match raw.get(at..at + 16) {
                Some(bytes) => line.copy_from_slice(bytes),
                None => line = [0; 16],
            }
            let from = (left as usize).max(x) - x;
            let to = end.min(x + 16) - x;
            out.extend_from_slice(&line[from..to]);
            x += 16;
        }
    }
}

/// The displayed NV12 picture of the planes `luma` and `chroma` (raw, laid
/// out by `tiling` with `pitch`): the cropped luma rows, then the cropped
/// interleaved chroma rows.
pub fn picture(luma: &[u8], chroma: &[u8], tiling: Tiling, pitch: u32, crop: &Crop) -> Vec<u8> {
    let mut out = Vec::with_capacity((crop.width * crop.height * 3 / 2) as usize);
    rows_of(
        &mut out,
        luma,
        tiling,
        pitch,
        (crop.left, crop.top),
        (crop.width, crop.height),
    );
    rows_of(
        &mut out,
        chroma,
        tiling,
        pitch,
        (crop.left, crop.top / 2),
        (crop.width, crop.height / 2),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvenc::surface::block_linear_offset;
    use alloc::vec;

    #[test]
    fn gob_2_is_nvencs_layout() {
        for y in [0, 1, 7, 8, 15, 16, 31, 543, 1087] {
            for x in (0..1920).step_by(16) {
                assert_eq!(
                    Tiling::NVDEC.offset(x, y, 1920),
                    block_linear_offset(x, y, 1920)
                );
            }
        }
    }

    #[test]
    fn a_picture_comes_out_of_any_tiling_cropped() {
        let (pitch, rows) = (128u32, 48u32);
        for tiling in Tiling::ALL {
            let mut luma = vec![0u8; tiling.plane_bytes(pitch, rows)];
            let mut chroma = vec![0u8; tiling.plane_bytes(pitch, rows / 2)];
            for (plane, height, seed) in [(&mut luma, rows, 1u32), (&mut chroma, rows / 2, 7)] {
                for y in 0..height as usize {
                    for x in (0..pitch as usize).step_by(16) {
                        let at = tiling.offset(x, y, pitch as usize);
                        for i in 0..16 {
                            plane[at + i] = ((x + i) as u32 * seed + y as u32 * 3) as u8;
                        }
                    }
                }
            }
            let crop = Crop {
                left: 0,
                top: 0,
                width: 112,
                height: 40,
            };
            let out = picture(&luma, &chroma, tiling, pitch, &crop);
            assert_eq!(out.len(), 112 * 40 * 3 / 2, "{}", tiling.name());
            assert_eq!(out[113], (1 + 3) as u8);
            let chroma_start = 112 * 40;
            assert_eq!(out[chroma_start + 112 + 5], (5 * 7 + 3) as u8);
        }
    }
}
