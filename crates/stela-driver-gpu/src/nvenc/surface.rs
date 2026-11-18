//! Pictures in and out of the surface layout NVENC reads, and the
//! conversion from the BGRA a desktop capture gives.
//!
//! **Input formats.** NVC7B7 reads YUV only. Its surface configuration
//! (`nvenc_h264_surface_cfg_s`, `nvenc_drv.h`) has `memory_mode` 0
//! (semi-planar, NV12) or 1 (planar), `nv21_enable` to swap the chroma, and
//! the sequence's `chroma_format_idc` (4:2:0; HEVC also 4:4:4); `clc7b7.h`
//! has no colour-conversion method and no RGB surface format, and NVIDIA's
//! captured sessions fed NV12. NVIDIA's SDK accepts ARGB/ABGR by converting
//! them before the engine (with CUDA), not in it. So a BGRA frame becomes
//! NV12 first: [`store_bgra`] does it on the host CPU, BT.709 limited range,
//! straight into the block-linear surface. That is the first, measured
//! version (the self-test logs its microseconds); a GPU compute pass is the
//! later one.
//!
//! **Layout.** Input pictures are "either all in BL format or all in tiled
//! 16x16 format" and there is no pitch-linear mode, so surfaces are
//! block-linear: GOBs of 64 bytes by 8 rows in Turing's line order (NVK's
//! `TuringColor2D`), stacked `block_height` GOBs high (the captured 2, a
//! 64x16 block: the engine counts GOBs, as found on the GPU on
//! 2026-11-15), blocks in row-major order across the pitch. The chroma
//! plane follows the luma at `pitch * coded_height`.
use super::layout::Geometry;
use alloc::vec;
use alloc::vec::Vec;

/// Rows per block: `block_height` 2 GOBs of 8.
const BLOCK_ROWS: usize = 16;
const BLOCK_BYTES: usize = 64 * BLOCK_ROWS;

/// The offset in a GOB of the 16-byte line holding row `y` (0..8) and
/// 16-byte column `column` (0..4): Turing's layout.
const fn gob_line(column: usize, y: usize) -> usize {
    (column >> 1) * 256 + (y >> 2) * 128 + (column & 1) * 64 + (y & 3) * 16
}

/// The offset in a block-linear plane of `pitch` bytes per row of the
/// 16-byte line at (`x`, `y`), `x` a multiple of 16.
pub const fn block_linear_offset(x: usize, y: usize, pitch: usize) -> usize {
    let block = (y / BLOCK_ROWS) * (pitch / 64) + x / 64;
    block * BLOCK_BYTES + ((y % BLOCK_ROWS) / 8) * 512 + gob_line((x % 64) / 16, y % 8)
}

/// Stores rows `rows` of a block-linear plane at `plane`, pitch-wide, each
/// given by `row(y, buffer)` into a pitch-wide buffer.
///
/// # Safety
/// `plane` is writable for `pitch * rows.end` bytes.
unsafe fn store_plane(
    plane: *mut u8,
    pitch: usize,
    rows: core::ops::Range<usize>,
    mut row: impl FnMut(usize, &mut [u8]),
) {
    let mut line = vec![0u8; pitch];
    for y in rows {
        row(y, &mut line);
        for x in (0..pitch).step_by(16) {
            // SAFETY: the offset of a line inside the plane, which the
            // caller's bound covers.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    line.as_ptr().add(x),
                    plane.add(block_linear_offset(x, y, pitch)),
                    16,
                );
            }
        }
    }
}

/// Copies `source` (a row of `available` bytes) into `out`, repeating its
/// last `unit`-byte pixel past them, as NVENC pads nothing itself.
fn padded(out: &mut [u8], source: &[u8], available: usize, unit: usize) {
    let available = available.min(source.len()).min(out.len());
    out[..available].copy_from_slice(&source[..available]);
    if available >= unit {
        let last = available - unit;
        for x in (available..out.len()).step_by(unit) {
            let count = unit.min(out.len() - x);
            out[x..x + count].copy_from_slice(&source[last..last + count]);
        }
    }
}

/// An NV12 picture in memory, row by row.
#[derive(Clone, Copy, Debug)]
pub struct Nv12<'a> {
    pub luma: &'a [u8],
    pub luma_pitch: usize,
    pub chroma: &'a [u8],
    pub chroma_pitch: usize,
}

impl Nv12<'_> {
    /// Whether it holds `geometry`'s displayed picture.
    pub fn fits(&self, geometry: &Geometry) -> bool {
        let (width, height) = (geometry.width as usize, geometry.height as usize);
        self.luma_pitch >= width
            && self.chroma_pitch >= width
            && self.luma.len() >= self.luma_pitch * (height - 1) + width
            && self.chroma.len() >= self.chroma_pitch * (height / 2 - 1) + width
    }
}

/// A plane of an NV12 surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plane {
    /// Y, one byte a pixel.
    Luma,
    /// Interleaved Cb and Cr, half as tall: two bytes for each two pixels.
    Chroma,
}

impl Plane {
    /// The picture's rows of the plane.
    pub fn rows(self, geometry: &Geometry) -> usize {
        match self {
            Self::Luma => geometry.height as usize,
            Self::Chroma => geometry.height as usize / 2,
        }
    }

    /// The surface's rows of the plane, the coded height's.
    fn coded_rows(self, geometry: &Geometry) -> usize {
        match self {
            Self::Luma => geometry.coded_height as usize,
            Self::Chroma => geometry.coded_height as usize / 2,
        }
    }

    /// Where the plane starts in the surface: the chroma follows the luma.
    fn offset(self, geometry: &Geometry) -> usize {
        match self {
            Self::Luma => 0,
            Self::Chroma => geometry.pitch as usize * geometry.coded_height as usize,
        }
    }

    /// The bytes a padding column repeats.
    fn unit(self) -> usize {
        match self {
            Self::Luma => 1,
            Self::Chroma => 2,
        }
    }
}

/// Stores the picture's rows `first..first + count` of `plane`, which
/// `rows` holds `row_pitch` bytes apart, into the block-linear NV12 surface
/// at `surface`, repeating each row's last pixel out to the pitch. The band
/// that ends with the plane's last row also repeats that row out to the
/// coded height, as NVENC pads nothing. A picture stored band by band, in
/// any order, is the surface [`store_nv12`] makes of it whole.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes. `count` is
/// at least one, `first + count` at most `plane.rows(geometry)`, and `rows`
/// holds `(count - 1) * row_pitch` bytes and then the picture's width.
pub unsafe fn store_rows(
    surface: *mut u8,
    geometry: &Geometry,
    plane: Plane,
    first: usize,
    count: usize,
    rows: &[u8],
    row_pitch: usize,
) {
    let pitch = geometry.pitch as usize;
    let width = geometry.width as usize;
    let last = plane.rows(geometry) - 1;
    debug_assert!(count >= 1 && first + count - 1 <= last);
    debug_assert!(rows.len() >= (count - 1) * row_pitch + width);
    let end = if first + count - 1 == last {
        plane.coded_rows(geometry)
    } else {
        first + count
    };
    // SAFETY: the plane's rows lie within the surface, which the caller
    // makes writable.
    unsafe {
        store_plane(
            surface.add(plane.offset(geometry)),
            pitch,
            first..end,
            |y, line| {
                let start = (y.min(last) - first) * row_pitch;
                padded(line, &rows[start..], width, plane.unit());
            },
        );
    }
}

/// Stores `picture` into the block-linear NV12 surface at `surface`,
/// repeating its last row and column out to the coded size.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes, and
/// `picture` [`Nv12::fits`] it.
pub unsafe fn store_nv12(surface: *mut u8, geometry: &Geometry, picture: &Nv12<'_>) {
    for (plane, rows, row_pitch) in [
        (Plane::Luma, picture.luma, picture.luma_pitch),
        (Plane::Chroma, picture.chroma, picture.chroma_pitch),
    ] {
        // SAFETY: the caller's bounds, and `fits` covers every row.
        unsafe {
            store_rows(
                surface,
                geometry,
                plane,
                0,
                plane.rows(geometry),
                rows,
                row_pitch,
            );
        }
    }
}

/// Reads the block-linear NV12 surface at `surface` back into a pitch-wide
/// NV12 picture of the coded size: luma, then chroma.
///
/// # Safety
/// `surface` is readable for `geometry.surface_bytes()` bytes.
pub unsafe fn load_nv12(surface: *const u8, geometry: &Geometry) -> Vec<u8> {
    let pitch = geometry.pitch as usize;
    let rows = geometry.coded_height as usize;
    let mut out = vec![0u8; pitch * rows * 3 / 2];
    for (plane, plane_rows) in [(0, rows), (pitch * rows, rows / 2)] {
        for y in 0..plane_rows {
            for x in (0..pitch).step_by(16) {
                // SAFETY: as in `store_plane`, reading.
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        surface.add(plane + block_linear_offset(x, y, pitch)),
                        out.as_mut_ptr().add(plane + y * pitch + x),
                        16,
                    );
                }
            }
        }
    }
    out
}

/// The byte order of a 32-bit pixel in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// B, G, R, A: `kCVPixelFormatType_32BGRA`, what ScreenCaptureKit gives.
    Bgra,
    /// R, G, B, A.
    Rgba,
}

/// A 32-bit RGB picture in memory, row by row.
#[derive(Clone, Copy, Debug)]
pub struct Rgb<'a> {
    pub pixels: &'a [u8],
    pub pitch: usize,
    pub order: Order,
}

impl Rgb<'_> {
    pub fn fits(&self, geometry: &Geometry) -> bool {
        let (width, height) = (geometry.width as usize, geometry.height as usize);
        self.pitch >= 4 * width && self.pixels.len() >= self.pitch * (height - 1) + 4 * width
    }

    fn rgb(&self, x: usize, y: usize) -> (i32, i32, i32) {
        let at = y * self.pitch + 4 * x;
        let p = &self.pixels[at..at + 4];
        let (r, g, b) = match self.order {
            Order::Bgra => (p[2], p[1], p[0]),
            Order::Rgba => (p[0], p[1], p[2]),
        };
        (i32::from(r), i32::from(g), i32::from(b))
    }
}

/// BT.709 limited range, 8-bit fixed point (x256).
fn luma(r: i32, g: i32, b: i32) -> u8 {
    (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)) as u8
}

fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let cb = 128 + ((-26 * r - 87 * g + 113 * b + 128) >> 8);
    let cr = 128 + ((112 * r - 102 * g - 10 * b + 128) >> 8);
    (cb.clamp(16, 240) as u8, cr.clamp(16, 240) as u8)
}

/// Converts `picture` to NV12 (BT.709, limited range, chroma the average of
/// each 2x2) and stores it into the block-linear surface at `surface`, the
/// last row and column repeated out to the coded size.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes, and
/// `picture` [`Rgb::fits`] it.
pub unsafe fn store_bgra(surface: *mut u8, geometry: &Geometry, picture: &Rgb<'_>) {
    // SAFETY: the caller's bounds; `fits` covers every row.
    unsafe {
        store_bgra_rows(surface, geometry, 0, geometry.height as usize, picture);
    }
}

/// Converts the picture's rows `first..first + count`, which `band` holds
/// from its first row on, as [`store_bgra`] does, and stores their luma and
/// the chroma rows they make whole into the block-linear NV12 surface at
/// `surface`. The band that ends the picture also repeats its last rows out
/// to the coded height. A picture stored band by band, in any order, is the
/// surface [`store_bgra`] makes of it whole.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes. `first` and
/// `count` are even, `count` at least two and `first + count` at most the
/// picture's height, and `band` holds `count` rows of the picture's width.
pub unsafe fn store_bgra_rows(
    surface: *mut u8,
    geometry: &Geometry,
    first: usize,
    count: usize,
    band: &Rgb<'_>,
) {
    let pitch = geometry.pitch as usize;
    let rows = geometry.coded_height as usize;
    let (width, height) = (geometry.width as usize, geometry.height as usize);
    debug_assert!(first.is_multiple_of(2) && count.is_multiple_of(2) && count >= 2);
    debug_assert!(first + count <= height);
    let ends = first + count == height;
    let mut source = vec![0u8; width];
    let luma_end = if ends { rows } else { first + count };
    // SAFETY: the luma plane, as in `store_nv12`.
    unsafe {
        store_plane(surface, pitch, first..luma_end, |y, line| {
            let y = y.min(height - 1) - first;
            for (x, out) in source.iter_mut().enumerate() {
                let (r, g, b) = band.rgb(x, y);
                *out = luma(r, g, b);
            }
            padded(line, &source, width, 1);
        });
    }
    let mut source = vec![0u8; width];
    let chroma_end = if ends { rows / 2 } else { (first + count) / 2 };
    // SAFETY: the chroma plane, as in `store_nv12`.
    unsafe {
        store_plane(
            surface.add(pitch * rows),
            pitch,
            first / 2..chroma_end,
            |y, line| {
                let y = y.min(height / 2 - 1);
                for x in (0..width).step_by(2) {
                    let (mut r, mut g, mut b) = (0, 0, 0);
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let (pr, pg, pb) = band.rgb(x + dx, 2 * y + dy - first);
                        (r, g, b) = (r + pr, g + pg, b + pb);
                    }
                    let (cb, cr) = chroma((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
                    source[x] = cb;
                    source[x + 1] = cr;
                }
                padded(line, &source, width, 2);
            },
        );
    }
}

/// The picture rows one band of the fast stores takes: two luma block rows,
/// which make one chroma block row. Their bands start at multiples of it.
pub const BAND_ROWS: usize = 2 * BLOCK_ROWS;

/// Copies 16 bytes with two 8-byte moves: the host is soft-float, and its
/// code keeps out of the vector registers, which hold the guest's state.
///
/// # Safety
/// `from` is readable and `to` writable for 16 bytes.
#[inline(always)]
unsafe fn copy16(from: *const u8, to: *mut u8) {
    // SAFETY: the caller's bounds; unaligned moves.
    unsafe {
        let low = from.cast::<u64>().read_unaligned();
        let high = from.add(8).cast::<u64>().read_unaligned();
        to.cast::<u64>().write_unaligned(low);
        to.add(8).cast::<u64>().write_unaligned(high);
    }
}

/// Stores block rows `blocks` of the block-linear plane at `plane`, `pitch`
/// bytes a row, writing each block row's bytes in address order: whole
/// 64-byte lines one after another, which is what a write-combined BAR1
/// mapping takes fastest. `row(y)` gives the plane's row `y`, `pitch`
/// readable bytes already padded.
///
/// A block row is 16 rows by the pitch, contiguous: its 64-byte-wide
/// blocks in order, each two GOBs of 8 rows, each GOB's 16-byte lines in
/// Turing's order ([`gob_line`]): column pair, row quad, column, row.
///
/// # Safety
/// `plane` is writable for `pitch * 16 * blocks.end` bytes, and each row
/// `row` gives is readable for `pitch` bytes.
unsafe fn tile_block_rows(
    plane: *mut u8,
    pitch: usize,
    blocks: core::ops::Range<usize>,
    row: impl Fn(usize) -> *const u8,
) {
    for block_row in blocks {
        let y0 = block_row * BLOCK_ROWS;
        let mut rows = [core::ptr::null::<u8>(); BLOCK_ROWS];
        for (y, at) in rows.iter_mut().enumerate() {
            *at = row(y0 + y);
        }
        // SAFETY: the block row lies inside the plane.
        let mut out = unsafe { plane.add(y0 * pitch) };
        for x0 in (0..pitch).step_by(64) {
            for gob in 0..2 {
                for pair in 0..2 {
                    for quad in 0..2 {
                        for column in 0..2 {
                            let x = x0 + (pair * 2 + column) * 16;
                            for y in 0..4 {
                                let source = rows[gob * 8 + quad * 4 + y];
                                // SAFETY: a 16-byte line of a padded row,
                                // and the next 16 bytes of the block row.
                                unsafe {
                                    copy16(source.add(x), out);
                                    out = out.add(16);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The block rows of `plane` a band from picture row `first` stores: its
/// own two (luma) or one (chroma), and for the band holding the picture's
/// last rows every block row after them, which repeat the last row.
fn band_blocks(geometry: &Geometry, plane: Plane, first: usize) -> core::ops::Range<usize> {
    let (start, own) = match plane {
        Plane::Luma => (first / BLOCK_ROWS, 2),
        Plane::Chroma => (first / 2 / BLOCK_ROWS, 1),
    };
    let blocks = plane.coded_rows(geometry).div_ceil(BLOCK_ROWS);
    if first + BAND_ROWS >= geometry.height as usize {
        start..blocks
    } else {
        start..(start + own).min(blocks)
    }
}

/// Room for one band's rows of both planes, padded to the pitch, which the
/// fast stores keep between bands so that none allocates.
#[derive(Default)]
pub struct Staging {
    rows: Vec<u8>,
}

impl Staging {
    /// The luma rows (`BAND_ROWS`) and chroma rows (half as many), each
    /// `pitch` bytes.
    fn planes(&mut self, pitch: usize) -> (&mut [u8], &mut [u8]) {
        let luma = BAND_ROWS * pitch;
        let bytes = luma + luma / 2;
        if self.rows.len() < bytes {
            self.rows.resize(bytes, 0);
        }
        let (luma_rows, chroma_rows) = self.rows[..bytes].split_at_mut(luma);
        (luma_rows, chroma_rows)
    }
}

/// Stores the NV12 picture's rows `first..` (at most [`BAND_ROWS`], to its
/// height) into the block-linear surface at `surface`, as whole block rows
/// in address order, and for the last band every block row to the coded
/// height, the last row and column repeated. `luma` holds the band's luma
/// rows from `first`, `luma_pitch` apart; `chroma` its chroma rows from
/// `first / 2`. Bands may come in any order; together they make the surface
/// [`store_nv12`] makes, and also fill a chroma plane's rows past the coded
/// height to its last block row.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes; `first` is a
/// multiple of [`BAND_ROWS`] below the height; each plane's slice holds its
/// rows, the last only as wide as the picture.
#[allow(clippy::too_many_arguments)]
pub unsafe fn store_nv12_band(
    surface: *mut u8,
    geometry: &Geometry,
    first: usize,
    luma: &[u8],
    luma_pitch: usize,
    chroma: &[u8],
    chroma_pitch: usize,
    staging: &mut Staging,
) {
    let pitch = geometry.pitch as usize;
    let width = geometry.width as usize;
    let height = geometry.height as usize;
    debug_assert!(first.is_multiple_of(BAND_ROWS) && first < height);
    let count = BAND_ROWS.min(height - first);
    let (luma_rows, chroma_rows) = staging.planes(pitch);
    for (plane, rows, row_pitch, staged, plane_first, plane_rows) in [
        (Plane::Luma, luma, luma_pitch, luma_rows, first, count),
        (
            Plane::Chroma,
            chroma,
            chroma_pitch,
            chroma_rows,
            first / 2,
            count / 2,
        ),
    ] {
        // A row as wide as the pitch is read in place; a narrower one is
        // padded into the staging rows first.
        let direct = width == pitch;
        if !direct {
            for y in 0..plane_rows {
                padded(
                    &mut staged[y * pitch..(y + 1) * pitch],
                    &rows[y * row_pitch..],
                    width,
                    plane.unit(),
                );
            }
        }
        let last = plane.rows(geometry) - 1;
        let staged = staged.as_ptr();
        let source = rows.as_ptr();
        // SAFETY: the plane's block rows lie inside the surface (the
        // chroma's whole blocks are in `surface_bytes`); each row given is
        // `pitch` readable bytes: a picture row at least that wide when
        // direct, a staging row otherwise.
        unsafe {
            tile_block_rows(
                surface.add(plane.offset(geometry)),
                pitch,
                band_blocks(geometry, plane, first),
                |y| {
                    let y = y.min(last) - plane_first;
                    if direct {
                        source.add(y * row_pitch)
                    } else {
                        staged.add(y * pitch)
                    }
                },
            );
        }
    }
}

/// BT.709 limited-range luma of two 32-bit pixels at once: `pixels` holds
/// them in its two 32-bit lanes, blue at bit `blue` and red at bit `red` of
/// each. Gives the two lumas in the lanes' low bytes, and each lane's red,
/// green and blue for the chroma's sums. Every lane's sum stays below 2^16,
/// so the lanes never carry into each other.
#[inline(always)]
fn luma_pair(pixels: u64, blue: u32, red: u32) -> (u64, u64, u64, u64) {
    const LANES: u64 = 0x0000_00ff_0000_00ff;
    let b = (pixels >> blue) & LANES;
    let g = (pixels >> 8) & LANES;
    let r = (pixels >> red) & LANES;
    let sum = 47 * r + 157 * g + 16 * b + 0x0000_0080_0000_0080;
    (((sum >> 8) & LANES) + 0x0000_0010_0000_0010, r, g, b)
}

/// Converts `count` rows of 32-bit RGB (even, at most [`BAND_ROWS`]) from
/// `band` into pitch-wide NV12 rows: `count` luma rows and `count / 2`
/// chroma rows, each row's last pixel repeated to the pitch. The same
/// arithmetic as [`store_bgra`]'s, two pixels a step.
fn convert_rows(
    band: &Rgb<'_>,
    count: usize,
    width: usize,
    pitch: usize,
    luma_rows: &mut [u8],
    chroma_rows: &mut [u8],
) {
    let (blue, red) = match band.order {
        Order::Bgra => (0, 16),
        Order::Rgba => (16, 0),
    };
    // The two lanes' sum of a 2x2 block's component, rounded to its mean.
    let mean = |sum: u64| (((sum & 0xffff_ffff) + (sum >> 32) + 2) >> 2) as i32;
    for pair in 0..count / 2 {
        let above = &band.pixels[2 * pair * band.pitch..][..4 * width];
        let below = &band.pixels[(2 * pair + 1) * band.pitch..][..4 * width];
        let (top, rest) = luma_rows[2 * pair * pitch..].split_at_mut(pitch);
        let bottom = &mut rest[..pitch];
        let uv = &mut chroma_rows[pair * pitch..][..pitch];
        let pixels = above
            .as_chunks::<8>()
            .0
            .iter()
            .zip(below.as_chunks::<8>().0);
        let outputs = top[..width]
            .as_chunks_mut::<2>()
            .0
            .iter_mut()
            .zip(bottom[..width].as_chunks_mut::<2>().0)
            .zip(uv[..width].as_chunks_mut::<2>().0);
        for ((a, b), ((top, bottom), uv)) in pixels.zip(outputs) {
            let a = u64::from_le_bytes(*a);
            let b = u64::from_le_bytes(*b);
            let (ya, ra, ga, ba) = luma_pair(a, blue, red);
            let (yb, rb, gb, bb) = luma_pair(b, blue, red);
            top[0] = ya as u8;
            top[1] = (ya >> 32) as u8;
            bottom[0] = yb as u8;
            bottom[1] = (yb >> 32) as u8;
            let (cb, cr) = chroma(mean(ra + rb), mean(ga + gb), mean(ba + bb));
            uv[0] = cb;
            uv[1] = cr;
        }
        for (row, unit) in [(top, 1), (bottom, 1), (uv, 2)] {
            let last = width - unit;
            for x in (width..pitch).step_by(unit) {
                let count = unit.min(pitch - x);
                row.copy_within(last..last + count, x);
            }
        }
    }
}

/// Converts the 32-bit RGB picture's rows `first..` (at most
/// [`BAND_ROWS`], to its height), which `band` holds from row `first` on,
/// to NV12 as [`store_bgra`] does, and stores them as whole block rows in
/// address order, the last band also every block row to the coded height.
/// Bands may come in any order; together they make the surface
/// [`store_bgra`] makes, and also fill a chroma plane's rows past the
/// coded height to its last block row.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes; `first` is a
/// multiple of [`BAND_ROWS`] below the height; `band` holds the band's
/// rows, the last only as wide as the picture.
pub unsafe fn store_bgra_band(
    surface: *mut u8,
    geometry: &Geometry,
    first: usize,
    band: &Rgb<'_>,
    staging: &mut Staging,
) {
    let pitch = geometry.pitch as usize;
    let width = geometry.width as usize;
    let height = geometry.height as usize;
    debug_assert!(first.is_multiple_of(BAND_ROWS) && first < height);
    let count = BAND_ROWS.min(height - first);
    let (luma_rows, chroma_rows) = staging.planes(pitch);
    convert_rows(band, count, width, pitch, luma_rows, chroma_rows);
    for (plane, staged, plane_first, plane_last) in [
        (Plane::Luma, &*luma_rows, first, height - 1),
        (Plane::Chroma, &*chroma_rows, first / 2, height / 2 - 1),
    ] {
        let staged = staged.as_ptr();
        // SAFETY: as in `store_nv12_band`; each staging row is `pitch`
        // bytes.
        unsafe {
            tile_block_rows(
                surface.add(plane.offset(geometry)),
                pitch,
                band_blocks(geometry, plane, first),
                |y| staged.add((y.min(plane_last) - plane_first) * pitch),
            );
        }
    }
}

/// nvenc-test's test pattern (`tiling.c`), pitch-wide NV12 of the coded
/// size: a diagonal luma ramp with 16-pixel stripes, a bright 256x256 square
/// moving 8 px right and 4 px down per frame, and four chroma quadrants. A
/// wrong GOB layout shows as torn 16-byte runs; the square gives P frames
/// real motion. Rows past the displayed height repeat its last.
pub fn pattern(geometry: &Geometry, frame: u32) -> Vec<u8> {
    let pitch = geometry.pitch as usize;
    let rows = geometry.coded_height as usize;
    let (width, height) = (geometry.width as i64, geometry.height as i64);
    let n = i64::from(frame);
    let square = 256.min(width / 2).min(height / 2);
    let sx = (100 + 8 * n) % (width - square).max(1);
    let sy = (100 + 4 * n) % (height - square).max(1);
    let mut out = vec![0u8; pitch * rows * 3 / 2];
    let (y_plane, uv_plane) = out.split_at_mut(pitch * rows);
    for j in 0..rows {
        let jj = (j as i64).min(height - 1);
        for i in 0..pitch {
            let ii = i as i64;
            let mut v = ((ii + jj) / 4 + ((ii / 16) & 1) * 24) & 0xff;
            if ii >= sx && ii < sx + square && jj >= sy && jj < sy + square {
                v = 235;
            }
            y_plane[j * pitch + i] = v.clamp(16, 235) as u8;
        }
    }
    const CB: [u8; 4] = [90, 160, 110, 200];
    const CR: [u8; 4] = [200, 90, 110, 160];
    for j in 0..rows / 2 {
        let jj = (j as i64).min(height / 2 - 1);
        for i in 0..pitch / 2 {
            let q = usize::from(i as i64 >= width / 4) + 2 * usize::from(jj >= height / 4);
            uv_plane[j * pitch + 2 * i] = CB[q];
            uv_plane[j * pitch + 2 * i + 1] = CR[q];
        }
    }
    out
}

/// The pattern's planes as an [`Nv12`] picture.
pub fn pattern_picture<'a>(geometry: &Geometry, nv12: &'a [u8]) -> Nv12<'a> {
    let pitch = geometry.pitch as usize;
    let (luma, chroma) = nv12.split_at(pitch * geometry.coded_height as usize);
    Nv12 {
        luma,
        luma_pitch: pitch,
        chroma,
        chroma_pitch: pitch,
    }
}

/// The luma PSNR of `b` against `a`, both pitch-wide NV12 of `geometry`'s
/// coded size, over the displayed picture, in hundredths of a dB; 9900 when
/// they are equal.
pub fn psnr_luma_centi_db(a: &[u8], b: &[u8], geometry: &Geometry) -> u32 {
    let pitch = geometry.pitch as usize;
    let mut squared: u64 = 0;
    for y in 0..geometry.height as usize {
        for x in 0..geometry.width as usize {
            let d = i64::from(a[y * pitch + x]) - i64::from(b[y * pitch + x]);
            squared += (d * d) as u64;
        }
    }
    if squared == 0 {
        return 9900;
    }
    let samples = f64::from(geometry.width) * f64::from(geometry.height);
    let ratio = 255.0 * 255.0 * samples / squared as f64;
    let db = 10.0 * log2(ratio) * core::f64::consts::LOG10_2;
    (db * 100.0 + 0.5).clamp(0.0, 9900.0) as u32
}

/// log2 of a positive finite `x` without `std`: its exponent, and
/// 2 atanh((m - 1) / (m + 1)) / ln 2 for its mantissa m in [1, 2).
fn log2(x: f64) -> f64 {
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mantissa = f64::from_bits((bits & ((1 << 52) - 1)) | (1023 << 52));
    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0;
    let mut k = 1.0;
    for _ in 0..12 {
        sum += term / k;
        term *= t2;
        k += 2.0;
    }
    exponent as f64 + 2.0 * sum * core::f64::consts::LOG2_E
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture stored in bands of any size, in any order, is the surface
    /// the whole store makes, NV12 and BGRA alike.
    #[test]
    fn bands_store_what_the_whole_store_does() {
        for (width, height) in [(1920, 1080), (640, 362), (48, 34)] {
            let geometry = Geometry::new(width, height).unwrap();
            let bytes = geometry.surface_bytes() as usize;
            let nv12 = pattern(&geometry, 3);
            let picture = pattern_picture(&geometry, &nv12);
            let mut whole = vec![0u8; bytes];
            // SAFETY: the buffer is the surface's size; the pattern fits.
            unsafe { store_nv12(whole.as_mut_ptr(), &geometry, &picture) };
            let mut banded = vec![0u8; bytes];
            for (plane, rows, pitch) in [
                (Plane::Chroma, picture.chroma, picture.chroma_pitch),
                (Plane::Luma, picture.luma, picture.luma_pitch),
            ] {
                let total = plane.rows(&geometry);
                let mut bands = Vec::new();
                let mut row = 0;
                for size in [7, 64, 1, 30].iter().cycle() {
                    if row == total {
                        break;
                    }
                    let count = (*size).min(total - row);
                    bands.push((row, count));
                    row += count;
                }
                for &(first, count) in bands.iter().rev() {
                    let rows =
                        &rows[first * pitch..first * pitch + (count - 1) * pitch + width as usize];
                    // SAFETY: as above.
                    unsafe {
                        store_rows(
                            banded.as_mut_ptr(),
                            &geometry,
                            plane,
                            first,
                            count,
                            rows,
                            pitch,
                        );
                    }
                }
            }
            assert!(whole == banded, "{width}x{height} NV12");

            let pitch = 4 * width as usize + 32;
            let pixels: Vec<u8> = (0..pitch * height as usize)
                .map(|i| (i * 7 + i / pitch * 13) as u8)
                .collect();
            let rgb = Rgb {
                pixels: &pixels,
                pitch,
                order: Order::Bgra,
            };
            let mut whole = vec![0u8; bytes];
            // SAFETY: as above.
            unsafe { store_bgra(whole.as_mut_ptr(), &geometry, &rgb) };
            let mut banded = vec![0u8; bytes];
            let mut row = 0;
            while row < height as usize {
                let count = 64.min(height as usize - row);
                let band = Rgb {
                    pixels: &pixels[row * pitch..],
                    pitch,
                    order: Order::Bgra,
                };
                // SAFETY: as above.
                unsafe { store_bgra_rows(banded.as_mut_ptr(), &geometry, row, count, &band) };
                row += count;
            }
            assert!(whole == banded, "{width}x{height} BGRA");
        }
    }

    /// Each plane's coded rows of the block-linear `surface`, as linear
    /// rows: a chroma plane's last block row may hold rows past them.
    fn coded(surface: &[u8], geometry: &Geometry) -> Vec<u8> {
        let pitch = geometry.pitch as usize;
        let mut out = Vec::new();
        for plane in [Plane::Luma, Plane::Chroma] {
            let base = plane.offset(geometry);
            for y in 0..plane.coded_rows(geometry) {
                for x in (0..pitch).step_by(16) {
                    let at = base + block_linear_offset(x, y, pitch);
                    out.extend_from_slice(&surface[at..at + 16]);
                }
            }
        }
        out
    }

    /// A BGRA picture of `geometry` with every value and order of bytes,
    /// `pitch` bytes a row.
    fn bgra(geometry: &Geometry, pitch: usize) -> Vec<u8> {
        (0..pitch * geometry.height as usize)
            .map(|i| (i * 7 + i / pitch * 13 + (i * i) % 251) as u8)
            .collect()
    }

    /// The fast bands, in any order, store the surface the whole stores
    /// store, NV12 at the pitch and wider, and BGRA in both byte orders;
    /// 1080p's surface is equal byte for byte.
    #[test]
    fn fast_bands_store_what_the_whole_store_does() {
        for (width, height) in [
            (1920, 1080),
            (640, 362),
            (48, 34),
            (1280, 720),
            (1366, 768),
            (2560, 1440),
        ] {
            let geometry = Geometry::new(width, height).unwrap();
            let bytes = geometry.surface_bytes() as usize;
            let (w, h) = (width as usize, height as usize);
            let firsts: Vec<usize> = (0..h).step_by(BAND_ROWS).rev().collect();
            let mut staging = Staging::default();
            for extra in [0, 64] {
                let nv12 = pattern(&geometry, 9);
                let pitch = geometry.pitch as usize;
                // The same picture at the guest's pitch, `extra` wider.
                let row_pitch = pitch + extra;
                let rows = geometry.coded_height as usize;
                let mut wide = vec![0xeeu8; row_pitch * rows * 3 / 2];
                for y in 0..rows * 3 / 2 {
                    wide[y * row_pitch..y * row_pitch + pitch]
                        .copy_from_slice(&nv12[y * pitch..(y + 1) * pitch]);
                }
                let picture = Nv12 {
                    luma: &wide[..row_pitch * rows],
                    luma_pitch: row_pitch,
                    chroma: &wide[row_pitch * rows..],
                    chroma_pitch: row_pitch,
                };
                let mut whole = vec![0u8; bytes];
                // SAFETY: the buffer is the surface's size; the picture fits.
                unsafe { store_nv12(whole.as_mut_ptr(), &geometry, &picture) };
                let mut banded = vec![0u8; bytes];
                for &first in &firsts {
                    // SAFETY: as above, a band of rows from `first`.
                    unsafe {
                        store_nv12_band(
                            banded.as_mut_ptr(),
                            &geometry,
                            first,
                            &picture.luma[first * row_pitch..],
                            row_pitch,
                            &picture.chroma[first / 2 * row_pitch..],
                            row_pitch,
                            &mut staging,
                        );
                    }
                }
                assert!(
                    coded(&whole, &geometry) == coded(&banded, &geometry),
                    "{width}x{height}+{extra} NV12"
                );
                if (width, height) == (1920, 1080) {
                    assert!(whole == banded, "1080p NV12 whole surface");
                }
            }
            for order in [Order::Bgra, Order::Rgba] {
                let pitch = 4 * w + 32;
                let pixels = bgra(&geometry, pitch);
                let rgb = Rgb {
                    pixels: &pixels,
                    pitch,
                    order,
                };
                let mut whole = vec![0u8; bytes];
                // SAFETY: as above.
                unsafe { store_bgra(whole.as_mut_ptr(), &geometry, &rgb) };
                let mut banded = vec![0u8; bytes];
                for &first in &firsts {
                    let band = Rgb {
                        pixels: &pixels[first * pitch..],
                        pitch,
                        order,
                    };
                    // SAFETY: as above.
                    unsafe {
                        store_bgra_band(banded.as_mut_ptr(), &geometry, first, &band, &mut staging)
                    };
                }
                assert!(
                    coded(&whole, &geometry) == coded(&banded, &geometry),
                    "{width}x{height} {order:?}"
                );
                if (width, height) == (1920, 1080) {
                    assert!(whole == banded, "1080p {order:?} whole surface");
                }
            }
        }
    }

    /// Microseconds per 1080p picture of each store, into host memory, when
    /// `NVENC_STORE_BENCH` is set: `NVENC_STORE_BENCH=1 cargo test --release
    /// -p stela-driver-gpu --lib stores_per_picture -- --nocapture`. The
    /// host build may vectorize what the soft-float UEFI build cannot; the
    /// numbers are a ceiling on speed. Without the variable it does nothing.
    #[test]
    fn stores_per_picture() {
        extern crate std;
        use std::time::Instant;
        if std::env::var_os("NVENC_STORE_BENCH").is_none() {
            return;
        }
        let geometry = Geometry::new(1920, 1080).unwrap();
        let bytes = geometry.surface_bytes() as usize;
        let nv12 = pattern(&geometry, 4);
        let picture = pattern_picture(&geometry, &nv12);
        let pitch = 4 * 1920;
        let pixels = bgra(&geometry, pitch);
        let rgb = Rgb {
            pixels: &pixels,
            pitch,
            order: Order::Bgra,
        };
        let mut surface = vec![0u8; bytes];
        let mut staging = Staging::default();
        let runs = 30;
        let mut time = |name: &str, store: &mut dyn FnMut(&mut [u8])| {
            store(&mut surface.clone());
            let started = Instant::now();
            for _ in 0..runs {
                store(&mut surface);
            }
            std::println!(
                "{name}: {} us per 1080p picture",
                started.elapsed().as_micros() / runs
            );
        };
        let luma_pitch = geometry.pitch as usize;
        time("store_nv12 (16-byte lines, scattered)", &mut |out| {
            // SAFETY: the surface's size; the pattern fits it.
            unsafe { store_nv12(out.as_mut_ptr(), &geometry, &picture) }
        });
        time("store_nv12_band (block rows in order)", &mut |out| {
            for first in (0..1080).step_by(BAND_ROWS) {
                // SAFETY: the surface's size; bands of the picture.
                unsafe {
                    store_nv12_band(
                        out.as_mut_ptr(),
                        &geometry,
                        first,
                        &picture.luma[first * luma_pitch..],
                        luma_pitch,
                        &picture.chroma[first / 2 * luma_pitch..],
                        luma_pitch,
                        &mut staging,
                    )
                }
            }
        });
        time("store_bgra (per pixel)", &mut |out| {
            // SAFETY: as above.
            unsafe { store_bgra(out.as_mut_ptr(), &geometry, &rgb) }
        });
        time("store_bgra_band (two pixels a step)", &mut |out| {
            for first in (0..1080).step_by(BAND_ROWS) {
                let band = Rgb {
                    pixels: &pixels[first * pitch..],
                    pitch,
                    order: Order::Bgra,
                };
                // SAFETY: as above.
                unsafe { store_bgra_band(out.as_mut_ptr(), &geometry, first, &band, &mut staging) }
            }
        });
    }

    #[test]
    fn log2_is_close() {
        for (x, want) in [
            (1.0, 0.0),
            (2.0, 1.0),
            (10.0, core::f64::consts::LOG2_10),
            (65025.0, 15.988_7),
        ] {
            assert!((log2(x) - want).abs() < 1e-4, "{x}: {}", log2(x));
        }
    }
}
