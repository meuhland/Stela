//! A decoding session's buffers: what each holds, how big it is, where it
//! lives, and the GPU address `VM_BIND` puts it at.
//!
//! Sizes and addresses follow NVIDIA's driver (nvdec-cap-1118,
//! `nvdec-conventions.md` section 3), derived from the picture size: every
//! one of the eight captured clips (1280x720, 1920x1080/1088, 3840x2160
//! Main 10) gets NVIDIA's buffers at NVIDIA's addresses, so each frame's
//! method stream is NVIDIA's word for word (`captured.rs`).
//!
//! **NVIDIA's address space** (`libnvcuvid` on CUDA's allocator), in order:
//!
//! - fixed low pages: the setup pages at `0x1_2002_b000` (4 of 4 KiB), the
//!   H.264 history at `0x1_2009_0000`;
//! - from `0x1_2040_0000`, the decoder's `pool` surfaces
//!   (`ulNumDecodeSurfaces`, the application's choice: FFmpeg asked for 15
//!   to 19), each as two allocations of one whole NV12 frame (pitch x
//!   (luma rows + chroma rows), both rounded up to 32), 2 MiB-aligned: the
//!   first holds the luma, the second the chroma, at the luma's size into
//!   it;
//! - then, 2 MiB-aligned, the co-located motion vectors, HEVC's filter
//!   buffer, and the 4 bitstream buffers back to back.
//!
//! Stela keeps that map but binds only the surfaces its stream needs
//! (`slots` of the `pool`): the rest of the surface range stays unbound.
//!
//! - **Setup pages** (system memory, 4 KiB, n % 4): the picture setup with,
//!   on the same page, H.264's slice offsets or HEVC's scaling list and tile
//!   sizes, and the frame's status ([`super::setup`]).
//! - **Bitstream** (system memory, 4 x width x 32-rounded rows x 2 bytes,
//!   n % 4): the slices, each behind a 3-byte start code.
//! - **Surfaces** (video memory, PTE kind 0x06, one buffer per slot over
//!   both allocations): block-linear (64x8 GOBs, blocks of two GOBs), NV12
//!   at 8 bits, 16-bit samples (P010's layout) at 10, pitch = width
//!   rounded up to 64, times the sample's bytes. Mapped for the CPU.
//! - **H.264**: the co-located motion vectors (4 MiB at both captured
//!   sizes: at least 4 MiB, and 514 bytes per macroblock beyond), and one
//!   buffer holding the macroblock history (`H264_SET_MBHIST_BUF_OFFSET`)
//!   and, after `mbhist_buffer_size` bytes, the history
//!   (`SET_HISTORY_OFFSET`), in 64 KiB pages.
//! - **HEVC**: the co-located motion vectors (17 pictures of
//!   `colMvBuffersize`, at least 4 MiB) and the filter buffer whose areas
//!   [`super::setup::hevc_filter`] places, with the intra-prediction top
//!   row (`SET_INTRA_TOP_BUF_OFFSET`) last.
use super::Codec;
use super::setup::{self, h264_history_units, h264_mbhist_bytes};
use crate::nvenc::layout::{Domain, Geometry, Plan};
use alloc::vec::Vec;

pub use crate::nvenc::layout::{KERNEL_VA_START, VA_TOP};

/// The PTE kind of the picture surfaces: generic memory, as NVENC's.
pub const SURFACE_KIND: u8 = 0x06;
/// Setup pages and bitstream buffers in use, by picture index.
pub const RING: usize = 4;
/// The most surface slots a session takes.
pub const MAX_SLOTS: usize = 16;
/// The most surfaces the address map leaves room for.
pub const MAX_POOL: usize = 32;
/// The pushbuffer ring.
pub const PUSH_BYTES: u64 = 0x4_0000;

const PUSH_ADDRESS: u64 = 0x1_1000_0000;
const FENCE_ADDRESS: u64 = 0x1_1004_0000;
const SETUP_ADDRESS: u64 = 0x1_2002_b000;
const HISTORY_ADDRESS: u64 = 0x1_2009_0000;
/// The history must end before NVIDIA's next fixed mapping.
const HISTORY_MOST: u64 = 0x1_2020_0000 - HISTORY_ADDRESS;
const SURFACE_ADDRESS: u64 = 0x1_2040_0000;
const HUGE: u64 = 0x20_0000;
const BIG: u64 = 0x1_0000;
const PAGE: u64 = 0x1000;
/// The smallest co-located buffer NVIDIA's driver made (720p and 1080p).
const COLOC_LEAST: u64 = 0x40_0000;
/// H.264's co-located bytes per macroblock: 4 MiB over 1920x1088's 8160.
const H264_COLOC_PER_MB: u64 = 514;
/// HEVC's co-located buffer holds this many pictures' `colMvBuffersize`.
const HEVC_COLOC_PICTURES: u64 = 17;

/// Where the chroma plane is in a 1920x1088 8-bit slot's span, as NVIDIA's
/// driver had it ([`Surfaces::chroma_offset`] in general).
pub const CHROMA_OFFSET: u64 = 0x5f_e000;

/// The decoded pictures' memory: one NV12 (8-bit) or 16-bit-sample (10-bit)
/// block-linear frame per slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surfaces {
    /// Bytes per sample: 1, or 2 above 8 bits.
    pub sample_bytes: u32,
    /// Bytes per row of either plane.
    pub pitch: u32,
    /// Rows of the luma plane and of the interleaved chroma plane: the
    /// coded height and half of it, each rounded up to 32.
    pub luma_rows: u32,
    pub chroma_rows: u32,
}

impl Surfaces {
    pub fn new(geometry: &Geometry, bit_depth: u8) -> Self {
        let sample_bytes = if bit_depth > 8 { 2 } else { 1 };
        Self {
            sample_bytes,
            pitch: geometry.pitch * sample_bytes,
            luma_rows: geometry.coded_height.next_multiple_of(32),
            chroma_rows: geometry.coded_height.div_ceil(2).next_multiple_of(32),
        }
    }

    pub fn luma_bytes(&self) -> u64 {
        u64::from(self.pitch) * u64::from(self.luma_rows)
    }

    pub fn chroma_bytes(&self) -> u64 {
        u64::from(self.pitch) * u64::from(self.chroma_rows)
    }

    /// One allocation of NVIDIA's: a whole frame.
    pub fn frame_bytes(&self) -> u64 {
        self.luma_bytes() + self.chroma_bytes()
    }

    /// The distance between allocations: a frame, 2 MiB-aligned.
    pub fn step(&self) -> u64 {
        self.frame_bytes().next_multiple_of(HUGE)
    }

    /// Where the chroma plane is in a slot's span: the second allocation,
    /// a luma plane into it.
    pub fn chroma_offset(&self) -> u64 {
        self.step() + self.luma_bytes()
    }

    /// A slot's span: both allocations.
    pub fn span(&self) -> u64 {
        2 * self.step()
    }
}

/// The bytes of each bitstream buffer: 2 bytes per sample of the coded
/// width by the 32-rounded rows (0x3fc000 at 1080p, 0x1cc000 at 720p,
/// 0xff0000 at 2160p, 8 or 10 bits alike).
pub fn bitstream_bytes(geometry: &Geometry) -> u64 {
    (u64::from(geometry.coded_width) * u64::from(geometry.coded_height.next_multiple_of(32)) * 2)
        .next_multiple_of(PAGE)
}

/// H.264's co-located buffer.
pub fn h264_coloc_bytes(geometry: &Geometry) -> u64 {
    let mbs = u64::from(geometry.coded_width / 16) * u64::from(geometry.coded_height / 16);
    (mbs * H264_COLOC_PER_MB)
        .next_multiple_of(HUGE)
        .max(COLOC_LEAST)
}

/// HEVC's co-located buffer, for pictures of up to 64x64 CTBs.
pub fn hevc_coloc_bytes(geometry: &Geometry) -> u64 {
    let blocks = u64::from(geometry.coded_width.next_multiple_of(64) / 16)
        * u64::from(geometry.coded_height.next_multiple_of(64) / 16);
    (blocks * 16 * HEVC_COLOC_PICTURES)
        .next_multiple_of(HUGE)
        .max(COLOC_LEAST)
}

/// The H.264 surface entries NVIDIA's driver sends
/// (`SET_PICTURE_LUMA/CHROMA_OFFSET0..`): one more than the pictures of
/// 32768 macroblocks (level 4's `MaxDpbMbs`) that fit at the size with its
/// height in macroblock pairs, at most 16 (5 at 1920x1088, 9 at 1280x720).
pub fn h264_entries(geometry: &Geometry) -> usize {
    let width = geometry.coded_width / 16;
    let height = (geometry.coded_height / 16).next_multiple_of(2);
    1 + (32768 / (width * height).max(1)).min(16) as usize
}

/// Every buffer of a session, and which is which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub codec: Codec,
    pub plans: Vec<Plan>,
    pub push: usize,
    pub fence: usize,
    pub setup: [usize; RING],
    pub bitstream: [usize; RING],
    pub surfaces: Vec<usize>,
    pub coloc: usize,
    /// H.264's history buffer; HEVC's filter buffer.
    pub scratch: usize,
    /// H.264's `mbhist_buffer_size`: the history follows it.
    pub mbhist_bytes: u64,
    /// HEVC's intra top row, in bytes into the filter buffer.
    pub intra_top: u64,
    /// The surfaces' format.
    pub format: Surfaces,
    /// The surfaces NVIDIA's map has room for.
    pub pool: usize,
    /// H.264's surface entries ([`h264_entries`]); 16 for HEVC.
    pub entries: usize,
    bitstream_bytes: u64,
}

fn gart(name: &'static str, bytes: u64, address: u64) -> Plan {
    Plan {
        name,
        domain: Domain::Gart,
        mappable: false,
        kind: 0,
        bytes,
        span: bytes,
        address,
    }
}

fn vram(name: &'static str, bytes: u64, address: u64, kind: u8, mappable: bool) -> Plan {
    Plan {
        name,
        domain: Domain::Vram,
        mappable,
        kind,
        bytes,
        span: bytes,
        address,
    }
}

impl Layout {
    /// The buffers of an 8-bit `codec` session of `slots` surfaces whose
    /// pictures have `geometry`, the address map made for as many.
    pub fn new(codec: Codec, geometry: &Geometry, slots: usize) -> Result<Self, &'static str> {
        Self::with(codec, geometry, 8, slots, slots)
    }

    /// The buffers of a `codec` session of `slots` surfaces (bound, the
    /// first ones of the map) of `bit_depth` whose pictures have
    /// `geometry`, on NVIDIA's address map for `pool` surfaces (at least
    /// `slots`).
    pub fn with(
        codec: Codec,
        geometry: &Geometry,
        bit_depth: u8,
        slots: usize,
        pool: usize,
    ) -> Result<Self, &'static str> {
        if slots == 0 || slots > MAX_POOL {
            return Err("1 to 32 surfaces");
        }
        if pool < slots || pool > MAX_POOL {
            return Err("the address map's surfaces must hold the slots, at most 32");
        }
        if !matches!(bit_depth, 8 | 10) {
            return Err("8 or 10 bits");
        }
        if codec == Codec::H264 && bit_depth != 8 {
            return Err("H.264 is 8-bit");
        }
        let format = Surfaces::new(geometry, bit_depth);
        let mut plans = Vec::new();
        let mut add = |plan: Plan| {
            plans.push(plan);
            plans.len() - 1
        };
        let push = add(gart("push", PUSH_BYTES, PUSH_ADDRESS));
        let fence = add(gart("fence", 0x1000, FENCE_ADDRESS));
        let mut setup = [0; RING];
        for (k, index) in setup.iter_mut().enumerate() {
            *index = add(gart(
                "setup",
                setup::PAGE_BYTES as u64,
                SETUP_ADDRESS + 0x1000 * k as u64,
            ));
        }
        let step = format.step();
        let surfaces = (0..slots)
            .map(|k| {
                let address = SURFACE_ADDRESS + 2 * step * k as u64;
                add(vram("surface", format.span(), address, SURFACE_KIND, true))
            })
            .collect();
        // Past the pool's last allocation, 2 MiB-aligned.
        let mut next = (SURFACE_ADDRESS + (2 * pool as u64 - 1) * step + format.frame_bytes())
            .next_multiple_of(HUGE);
        let (coloc, scratch, mbhist_bytes, intra_top, entries) = match codec {
            Codec::H264 => {
                let bytes = h264_coloc_bytes(geometry);
                let coloc = add(vram("coloc", bytes, next, 0, false));
                next += bytes;
                let width_mbs = geometry.coded_width / 16;
                let mbhist = u64::from(h264_mbhist_bytes(width_mbs));
                let history_bytes =
                    (mbhist + 256 * u64::from(h264_history_units(width_mbs))).next_multiple_of(BIG);
                if history_bytes > HISTORY_MOST {
                    return Err("the H.264 history outgrows NVIDIA's low pages");
                }
                let history = add(vram("history", history_bytes, HISTORY_ADDRESS, 0, false));
                (coloc, history, mbhist, 0, h264_entries(geometry))
            }
            Codec::Hevc => {
                let filter =
                    setup::hevc_filter(geometry.coded_width, geometry.coded_height, bit_depth);
                let bytes = hevc_coloc_bytes(geometry);
                let coloc = add(vram("coloc", bytes, next, 0, false));
                next += bytes;
                let buffer = add(vram("filter", filter.filter_bytes, next, 0, false));
                next += filter.filter_bytes;
                (coloc, buffer, 0, filter.intra_top, 16)
            }
        };
        let bitstream_bytes = bitstream_bytes(geometry);
        let mut bitstream = [0; RING];
        for index in bitstream.iter_mut() {
            *index = add(gart("bitstream", bitstream_bytes, next));
            next += bitstream_bytes;
        }
        if next > KERNEL_VA_START {
            return Err("the buffers outgrow the session's address space");
        }
        Ok(Self {
            codec,
            plans,
            push,
            fence,
            setup,
            bitstream,
            surfaces,
            coloc,
            scratch,
            mbhist_bytes,
            intra_top,
            format,
            pool,
            entries,
            bitstream_bytes,
        })
    }

    pub fn address(&self, index: usize) -> u64 {
        self.plans[index].address
    }

    /// The luma and chroma GPU addresses of surface slot `slot`, bound or
    /// not.
    pub fn surface(&self, slot: u8) -> (u64, u64) {
        let base = SURFACE_ADDRESS + self.format.span() * u64::from(slot);
        (base, base + self.format.chroma_offset())
    }

    /// Where the chroma plane is in a slot's buffer.
    pub fn chroma_offset(&self) -> u64 {
        self.format.chroma_offset()
    }

    /// The bytes of each bitstream buffer.
    pub fn bitstream_bytes(&self) -> u64 {
        self.bitstream_bytes
    }

    /// Where the session's addresses end: past every buffer and every
    /// surface of the pool, bound or not.
    pub fn end(&self) -> u64 {
        let pool_end = SURFACE_ADDRESS + self.format.span() * self.pool as u64;
        self.plans
            .iter()
            .map(|plan| plan.address + plan.span)
            .fold(pool_end, u64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(width: u32, height: u32) -> Geometry {
        Geometry::new(width, height).unwrap()
    }

    #[test]
    fn the_captured_sizes_get_nvidias_buffers() {
        // h264-qp25: 16 surfaces.
        let l = Layout::with(Codec::H264, &geometry(1920, 1088), 8, 4, 16).unwrap();
        assert_eq!(l.format.frame_bytes(), 0x2f_d000);
        assert_eq!(l.surface(1), (0x1_20c0_0000, 0x1_211f_e000));
        assert_eq!(l.address(l.coloc), 0x1_2840_0000);
        assert_eq!(l.address(l.bitstream[1]), 0x1_28bf_c000);
        assert_eq!(l.address(l.scratch), 0x1_2009_0000);
        assert_eq!(l.mbhist_bytes, 0x3100);
        assert_eq!(l.entries, 5);
        // h264-cavlc-slices-720p: 15 surfaces.
        let l = Layout::with(Codec::H264, &geometry(1280, 720), 8, 3, 15).unwrap();
        assert_eq!(l.format.frame_bytes(), 0x15_e000);
        assert_eq!(l.surface(1), (0x1_2080_0000, 0x1_20ae_6000));
        assert_eq!(l.address(l.coloc), 0x1_2400_0000);
        assert_eq!(l.address(l.bitstream[0]), 0x1_2440_0000);
        assert_eq!(l.bitstream_bytes(), 0x1c_c000);
        assert_eq!(l.entries, 9);
        // hevc-qp25: 19 surfaces.
        let l = Layout::with(Codec::Hevc, &geometry(1920, 1088), 8, 5, 19).unwrap();
        assert_eq!(l.address(l.coloc), 0x1_29c0_0000);
        assert_eq!(l.address(l.scratch), 0x1_2a00_0000);
        assert_eq!(l.intra_top, 0x8d_7200);
        assert_eq!(l.address(l.bitstream[0]), 0x1_2aa0_0000);
        // hevc-main10-4k: 19 surfaces of 16-bit samples.
        let l = Layout::with(Codec::Hevc, &geometry(3840, 2160), 10, 5, 19).unwrap();
        assert_eq!(l.format.pitch, 7680);
        assert_eq!(l.format.frame_bytes(), 0x17e_8000);
        assert_eq!(l.surface(1), (0x1_2340_0000, 0x1_25bf_0000));
        assert_eq!(l.address(l.coloc), 0x1_5940_0000);
        assert_eq!(l.plans[l.coloc].bytes, 0xa0_0000);
        assert_eq!(l.address(l.scratch), 0x1_59e0_0000);
        assert_eq!(l.plans[l.scratch].bytes, 0x240_0000);
        assert_eq!(l.intra_top, 0x239_c400);
        assert_eq!(l.address(l.bitstream[3]), 0x1_5f1d_0000);
        assert_eq!(l.bitstream_bytes(), 0xff_0000);
    }

    #[test]
    fn the_1080p_chroma_is_where_phase_one_had_it() {
        let l = Layout::new(Codec::H264, &geometry(1920, 1088), 4).unwrap();
        assert_eq!(l.chroma_offset(), CHROMA_OFFSET);
    }
}
