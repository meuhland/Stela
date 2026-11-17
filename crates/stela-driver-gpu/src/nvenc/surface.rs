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

/// Stores `rows` pitch-wide rows into a block-linear plane at `plane`,
/// each row given by `row(y, buffer)` into a pitch-wide buffer.
///
/// # Safety
/// `plane` is writable for `pitch * rows` bytes.
unsafe fn store_plane(
    plane: *mut u8,
    pitch: usize,
    rows: usize,
    mut row: impl FnMut(usize, &mut [u8]),
) {
    let mut line = vec![0u8; pitch];
    for y in 0..rows {
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

/// Stores `picture` into the block-linear NV12 surface at `surface`,
/// repeating its last row and column out to the coded size.
///
/// # Safety
/// `surface` is writable for `geometry.surface_bytes()` bytes, and
/// `picture` [`Nv12::fits`] it.
pub unsafe fn store_nv12(surface: *mut u8, geometry: &Geometry, picture: &Nv12<'_>) {
    let pitch = geometry.pitch as usize;
    let rows = geometry.coded_height as usize;
    let (width, height) = (geometry.width as usize, geometry.height as usize);
    // SAFETY: the luma plane is the surface's first pitch * rows bytes.
    unsafe {
        store_plane(surface, pitch, rows, |y, line| {
            let y = y.min(height - 1);
            let start = y * picture.luma_pitch;
            padded(line, &picture.luma[start..], width, 1);
        });
    }
    // SAFETY: the chroma plane follows, half as tall.
    unsafe {
        store_plane(surface.add(pitch * rows), pitch, rows / 2, |y, line| {
            let y = y.min(height / 2 - 1);
            let start = y * picture.chroma_pitch;
            padded(line, &picture.chroma[start..], width, 2);
        });
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
    let pitch = geometry.pitch as usize;
    let rows = geometry.coded_height as usize;
    let (width, height) = (geometry.width as usize, geometry.height as usize);
    let mut source = vec![0u8; width];
    // SAFETY: the luma plane, as in `store_nv12`.
    unsafe {
        store_plane(surface, pitch, rows, |y, line| {
            let y = y.min(height - 1);
            for (x, out) in source.iter_mut().enumerate() {
                let (r, g, b) = picture.rgb(x, y);
                *out = luma(r, g, b);
            }
            padded(line, &source, width, 1);
        });
    }
    let mut source = vec![0u8; width];
    // SAFETY: the chroma plane, as in `store_nv12`.
    unsafe {
        store_plane(surface.add(pitch * rows), pitch, rows / 2, |y, line| {
            let y = y.min(height / 2 - 1);
            for x in (0..width).step_by(2) {
                let (mut r, mut g, mut b) = (0, 0, 0);
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let (pr, pg, pb) = picture.rgb(x + dx, 2 * y + dy);
                    (r, g, b) = (r + pr, g + pg, b + pb);
                }
                let (cb, cr) = chroma((r + 2) / 4, (g + 2) / 4, (b + 2) / 4);
                source[x] = cb;
                source[x + 1] = cr;
            }
            padded(line, &source, width, 2);
        });
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
