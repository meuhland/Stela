//! A decoding session's buffers: what each holds, where it lives, and the
//! GPU address `VM_BIND` puts it at.
//!
//! The picture size NVIDIA's driver was captured decoding (1920x1088 coded,
//! nvdec-cap-1118) gets that capture's buffers at the GPU addresses NVIDIA's
//! driver used, so a frame's method stream is NVIDIA's word for word
//! (`captured.rs` checks it). Other sizes are refused for now: the engine's
//! own buffers (co-located motion vectors, H.264 history, the HEVC filter
//! buffer's areas) are only known at the captured sizes.
//!
//! - **Setup pages** (system memory, 4 KiB, n % 4): the picture setup with,
//!   on the same page, H.264's slice offsets or HEVC's scaling list and tile
//!   sizes, and the frame's status ([`super::setup`]).
//! - **Bitstream** (system memory, 4 x 0x3fc000, n % 4): the slices, each
//!   behind a 3-byte start code.
//! - **Surfaces** (video memory, PTE kind 0x06, one per DPB slot): NV12,
//!   block-linear (64x8 GOBs, blocks of two GOBs), pitch = width; the luma
//!   at the slot's base and the chroma 0x5fe000 after it, as NVIDIA's
//!   driver placed them (its CUDA surfaces put each plane in an allocation
//!   of its own). Mapped for the CPU, which reads each decoded picture back.
//! - **H.264**: the co-located motion vectors (4 MiB of video memory, one
//!   area per slot the engine places by `col_idx`), and one 128 KiB buffer
//!   holding the macroblock history (`H264_SET_MBHIST_BUF_OFFSET`) and,
//!   after `mbhist_buffer_size` bytes, the history (`SET_HISTORY_OFFSET`).
//! - **HEVC**: the co-located motion vectors (4 MiB) and the filter buffer
//!   (10 MiB) whose areas the setup places, with the intra-prediction top
//!   row at +0x8d7200 (`SET_INTRA_TOP_BUF_OFFSET`).
use super::Codec;
use super::setup::{self, h264_mbhist_bytes};
use crate::nvenc::layout::{Domain, Geometry, Plan};
use alloc::vec::Vec;

pub use crate::nvenc::layout::{KERNEL_VA_START, VA_TOP};

/// The PTE kind of the picture surfaces: generic memory, as NVENC's.
pub const SURFACE_KIND: u8 = 0x06;
/// Setup pages and bitstream buffers in use, by picture index.
pub const RING: usize = 4;
/// The most surface slots a session takes.
pub const MAX_SLOTS: usize = 16;
/// The pushbuffer ring.
pub const PUSH_BYTES: u64 = 0x4_0000;

const PUSH_ADDRESS: u64 = 0x1_1000_0000;
const FENCE_ADDRESS: u64 = 0x1_1004_0000;
const SETUP_ADDRESS: u64 = 0x1_2002_b000;
const HISTORY_ADDRESS: u64 = 0x1_2009_0000;
const HISTORY_BYTES: u64 = 0x2_0000;
const SURFACE_ADDRESS: u64 = 0x1_2040_0000;
/// Each slot's span: its luma, and its chroma at [`CHROMA_OFFSET`].
const SURFACE_SPAN: u64 = 0x80_0000;
/// Where the chroma plane is in a slot's span, as NVIDIA's driver had it.
pub const CHROMA_OFFSET: u64 = 0x5f_e000;
const BITSTREAM_BYTES: u64 = 0x3f_c000;
const H264_COLOC: (u64, u64) = (0x1_2840_0000, 0x40_0000);
const H264_BITSTREAM: u64 = 0x1_2880_0000;
const HEVC_COLOC: u64 = 0x1_29c0_0000;
const HEVC_FILTER: u64 = 0x1_2a00_0000;
const HEVC_BITSTREAM: u64 = 0x1_2aa0_0000;

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
    /// The buffers of a `codec` session of `slots` surfaces whose pictures
    /// have `geometry`, when that is NVIDIA's captured size.
    pub fn new(codec: Codec, geometry: &Geometry, slots: usize) -> Result<Self, &'static str> {
        if geometry.coded_width != 1920 || geometry.coded_height != 1088 || geometry.pitch != 1920 {
            return Err("only NVIDIA's captured 1920x1088 layout so far");
        }
        if slots == 0 || slots > MAX_SLOTS || (codec == Codec::H264 && slots > 16) {
            return Err("1 to 16 surface slots");
        }
        // The surfaces must not reach the next buffer up (H.264's
        // co-located area, HEVC's): 5 slots end at 0x1_22c0_0000.
        if SURFACE_ADDRESS + SURFACE_SPAN * slots as u64 > H264_COLOC.0 {
            return Err("too many surface slots for the captured addresses");
        }
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
        let bitstream_base = match codec {
            Codec::H264 => H264_BITSTREAM,
            Codec::Hevc => HEVC_BITSTREAM,
        };
        let mut bitstream = [0; RING];
        for (k, index) in bitstream.iter_mut().enumerate() {
            let address = bitstream_base + BITSTREAM_BYTES * k as u64;
            *index = add(gart("bitstream", BITSTREAM_BYTES, address));
        }
        let surfaces = (0..slots)
            .map(|k| {
                let address = SURFACE_ADDRESS + SURFACE_SPAN * k as u64;
                add(vram("surface", SURFACE_SPAN, address, SURFACE_KIND, true))
            })
            .collect();
        let (coloc, scratch, mbhist_bytes, intra_top) = match codec {
            Codec::H264 => {
                let coloc = add(vram("coloc", H264_COLOC.1, H264_COLOC.0, 0, false));
                let history = add(vram("history", HISTORY_BYTES, HISTORY_ADDRESS, 0, false));
                let mbhist = u64::from(h264_mbhist_bytes(geometry.coded_width / 16));
                (coloc, history, mbhist, 0)
            }
            Codec::Hevc => {
                let filter = setup::hevc_filter(1920, 1088, 8).ok_or("no HEVC filter layout")?;
                let coloc = add(vram("coloc", filter.coloc_bytes, HEVC_COLOC, 0, false));
                let buffer = add(vram("filter", filter.filter_bytes, HEVC_FILTER, 0, false));
                (coloc, buffer, 0, filter.intra_top)
            }
        };
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
        })
    }

    pub fn address(&self, index: usize) -> u64 {
        self.plans[index].address
    }

    /// The luma and chroma GPU addresses of surface slot `slot`.
    pub fn surface(&self, slot: u8) -> (u64, u64) {
        let base = self.address(self.surfaces[usize::from(slot)]);
        (base, base + CHROMA_OFFSET)
    }

    /// The bytes of each bitstream buffer.
    pub fn bitstream_bytes(&self) -> u64 {
        BITSTREAM_BYTES
    }
}
