//! The session's buffers: what each holds, where it lives, and the GPU
//! address `VM_BIND` puts it at.
//!
//! A picture that fits NVIDIA's captured 1920x1088 session gets that
//! session's buffers at the GPU addresses NVIDIA's driver used
//! (`nvenc-capture.md`'s table, nvenc-test's `nvenc.h`), so its method
//! stream is nvenc-test's word for word. A larger one gets the same buffers
//! scaled, one after another from [`SCALED_BASE`]; their engine-owned sizes
//! (history, ME predictors, co-located MVs) are this port's estimates, not
//! a capture's, and that layout has not run on the GPU.
use super::Codec;
use alloc::vec::Vec;

/// Where nouveau keeps its own mappings when the session's file has done
/// `VM_INIT`, as NVK does: `[2^39, 2^40)`.
pub const KERNEL_VA_START: u64 = 1 << 39;
pub const VA_TOP: u64 = 1 << 40;
/// The first address of a scaled layout.
pub const SCALED_BASE: u64 = 0x2_0000_0000;
/// Where buffers a caller binds into the session's VM go
/// ([`super::session::Session::bind_import`]).
pub const IMPORT_BASE: u64 = 0x4_0000_0000;
/// The pushbuffer ring.
pub const PUSH_BYTES: u64 = 0x4_0000;
/// The PTE kind of the picture surfaces (input and reconstructed):
/// generic memory, as nvenc-test's default.
pub const SURFACE_KIND: u8 = 0x06;
/// Setups in flight, used in turn (n % 16).
pub const SETUPS: usize = 16;
/// Slots of status, RC data, input and bitstream (n % 4).
pub const SLOTS: usize = 4;
/// Reconstructed pictures (n % 5).
pub const RECONS: usize = 5;

/// The largest coded size `nvenc_*_surface_cfg_s` takes.
pub const MAX_SIDE: u32 = 4096;

/// A picture's sizes: displayed, coded (16-aligned) and in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Bytes per row of a block-linear plane: the coded width rounded up to
    /// a GOB's 64 bytes.
    pub pitch: u32,
}

impl Geometry {
    /// The geometry of a `width` x `height` picture, when the encoder takes
    /// it: both even, at least 16 and at most [`MAX_SIDE`].
    pub fn new(width: u32, height: u32) -> Result<Self, &'static str> {
        if width < 16 || height < 16 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err("the picture must be at least 16x16, in even sizes (4:2:0)");
        }
        let coded_width = width.next_multiple_of(16);
        let coded_height = height.next_multiple_of(16);
        if coded_width > MAX_SIDE || coded_height > MAX_SIDE {
            return Err("the picture is larger than NVENC's 4096x4096");
        }
        Ok(Self {
            width,
            height,
            coded_width,
            coded_height,
            pitch: coded_width.next_multiple_of(64),
        })
    }

    pub fn luma_bytes(&self) -> u64 {
        u64::from(self.pitch) * u64::from(self.coded_height)
    }

    /// NV12: the luma, then half as much interleaved chroma.
    pub fn surface_bytes(&self) -> u64 {
        self.luma_bytes() * 3 / 2
    }

    pub fn mb_width(&self) -> u32 {
        self.coded_width / 16
    }

    pub fn mb_height(&self) -> u32 {
        self.coded_height / 16
    }

    pub fn macroblocks(&self) -> u32 {
        self.mb_width() * self.mb_height()
    }
}

/// Where a buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// System memory the GPU reads and writes, coherent with the CPU.
    Gart,
    /// Video memory; [`Plan::mappable`] ones the CPU reaches through BAR1.
    Vram,
}

/// One buffer of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub name: &'static str,
    pub domain: Domain,
    pub mappable: bool,
    pub kind: u8,
    /// The bytes asked for.
    pub bytes: u64,
    /// The GPU addresses kept for it, which the buffer nouveau makes, rounded
    /// to its pages, must fit.
    pub span: u64,
    pub address: u64,
}

impl Plan {
    /// Whether the CPU maps it: system memory always, video memory when
    /// asked.
    pub fn mapped(&self) -> bool {
        self.domain == Domain::Gart || self.mappable
    }
}

/// Every buffer of a session, and which is which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub codec: Codec,
    pub plans: Vec<Plan>,
    pub push: usize,
    pub fence: usize,
    pub setup: [usize; SETUPS],
    pub mepred: [usize; 2],
    pub history: usize,
    pub rc_process: usize,
    pub status: [usize; SLOTS],
    pub rc_data: [usize; SLOTS],
    pub recon: [usize; RECONS],
    /// H.264 only: the co-located motion vectors, one area per picture.
    pub coloc: Option<usize>,
    pub coloc_stride: u64,
    pub input: [usize; SLOTS],
    pub bitstream: [usize; SLOTS],
    /// Whether this is NVIDIA's captured layout.
    pub captured: bool,
}

/// What a codec's captured session laid out (`nvenc.h`).
struct Captured {
    setup_stride: u64,
    mepred: (u64, u64, u64),
    history: u64,
    rc_process: (u64, u64),
    status: (u64, u64),
    rc_data: (u64, u64),
    input: (u64, u64),
    bitstream: (u64, u64, u64),
    coloc: bool,
}

const H264: Captured = Captured {
    setup_stride: 0x3000,
    mepred: (0x1_2012_0000, 0x2_0000, 0x1_0000),
    history: 0x1_2016_0000,
    rc_process: (0x1_2017_1000, 0x1000),
    status: (0x1_2017_2000, 0x6000),
    rc_data: (0x1_2017_3000, 0x5000),
    input: (0x1_21c0_0000, 0xc0_0000),
    bitstream: (0x1_2230_c000, 0xc0_0000, 0x30_c000),
    coloc: true,
};

const HEVC: Captured = Captured {
    setup_stride: 0xe000,
    mepred: (0x1_201d_0000, 0x2_0000, 0x2_0000),
    history: 0x1_2181_0000,
    rc_process: (0x1_2182_0000, 0x2000),
    status: (0x1_2182_2000, 0xe000),
    rc_data: (0x1_2182_3000, 0xd000),
    input: (0x1_21a0_0000, 0xa0_0000),
    bitstream: (0x1_21e0_0000, 0xa0_0000, 0x4f_c000),
    coloc: false,
};

const PUSH_ADDRESS: u64 = 0x1_1000_0000;
const FENCE_ADDRESS: u64 = 0x1_1004_0000;
const SETUP_ADDRESS: u64 = 0x1_2009_0000;
const RECON_ADDRESS: u64 = 0x1_2040_0000;
const SURFACE_SPAN: u64 = 0x40_0000;
const COLOC_ADDRESS: u64 = 0x1_2180_0000;
const COLOC_BYTES: u64 = 0x40_0000;
/// The captured session's co-located area per picture.
const COLOC_STRIDE: u64 = 0x8_3400;
/// The captured picture, which the captured layout holds.
const CAPTURED_SURFACE: u64 = 0x2f_d000;
const CAPTURED_MBS: u32 = 8160;
const CAPTURED_MB_WIDTH: u32 = 120;
/// nouveau places large surfaces on 2 MiB pages.
const LARGE_PAGE: u64 = 0x20_0000;

impl Layout {
    pub fn new(codec: Codec, geometry: &Geometry, map_recon: bool) -> Self {
        let captured = geometry.surface_bytes() <= CAPTURED_SURFACE
            && geometry.macroblocks() <= CAPTURED_MBS
            && geometry.mb_width() <= CAPTURED_MB_WIDTH;
        let mut layout = Self {
            codec,
            plans: Vec::new(),
            push: 0,
            fence: 0,
            setup: [0; SETUPS],
            mepred: [0; 2],
            history: 0,
            rc_process: 0,
            status: [0; SLOTS],
            rc_data: [0; SLOTS],
            recon: [0; RECONS],
            coloc: None,
            coloc_stride: COLOC_STRIDE,
            input: [0; SLOTS],
            bitstream: [0; SLOTS],
            captured,
        };
        if captured {
            layout.captured(geometry, map_recon);
        } else {
            layout.scaled(geometry, map_recon);
        }
        layout
    }

    fn add(&mut self, plan: Plan) -> usize {
        self.plans.push(plan);
        self.plans.len() - 1
    }

    /// NVIDIA's own addresses, with the picture's surfaces in their spans.
    fn captured(&mut self, geometry: &Geometry, map_recon: bool) {
        let c = match self.codec {
            Codec::H264 => &H264,
            Codec::Hevc => &HEVC,
        };
        let gart = |name, bytes, span, address| Plan {
            name,
            domain: Domain::Gart,
            mappable: false,
            kind: 0,
            bytes,
            span,
            address,
        };
        let vram = |name, bytes, span, address, kind, mappable| Plan {
            name,
            domain: Domain::Vram,
            mappable,
            kind,
            bytes,
            span,
            address,
        };
        let surface = geometry.surface_bytes();
        let setup = self.codec.setup_bytes() as u64;
        self.push = self.add(gart("push", PUSH_BYTES, PUSH_BYTES, PUSH_ADDRESS));
        self.fence = self.add(gart("fence", 0x1000, 0x1000, FENCE_ADDRESS));
        for k in 0..SETUPS {
            let address = SETUP_ADDRESS + c.setup_stride * k as u64;
            self.setup[k] = self.add(gart("setup", setup, setup, address));
        }
        for k in 0..2 {
            let (base, stride, bytes) = c.mepred;
            self.mepred[k] = self.add(vram(
                "mepred",
                bytes,
                bytes,
                base + stride * k as u64,
                0,
                false,
            ));
        }
        self.history = self.add(vram("history", 0x1_0000, 0x1_0000, c.history, 0, false));
        let (address, span) = c.rc_process;
        self.rc_process = self.add(gart("rc_process", 0x1000, span, address));
        for k in 0..SLOTS {
            let (base, stride) = c.status;
            self.status[k] = self.add(gart("status", 0x1000, 0x1000, base + stride * k as u64));
            let (base, bytes) = c.rc_data;
            let address = base + c.status.1 * k as u64;
            self.rc_data[k] = self.add(gart("rc_data", bytes, bytes, address));
        }
        for k in 0..RECONS {
            let address = RECON_ADDRESS + SURFACE_SPAN * k as u64;
            let plan = vram(
                "recon",
                surface,
                SURFACE_SPAN,
                address,
                SURFACE_KIND,
                map_recon,
            );
            self.recon[k] = self.add(plan);
        }
        if c.coloc {
            let plan = vram("coloc", COLOC_BYTES, COLOC_BYTES, COLOC_ADDRESS, 0, false);
            self.coloc = Some(self.add(plan));
        }
        for k in 0..SLOTS {
            let (base, stride) = c.input;
            let address = base + stride * k as u64;
            let plan = vram("input", surface, SURFACE_SPAN, address, SURFACE_KIND, true);
            self.input[k] = self.add(plan);
            let (base, stride, bytes) = c.bitstream;
            let address = base + stride * k as u64;
            // HEVC's bitstream buffers have room to the next input.
            let span = match self.codec {
                Codec::H264 => bytes,
                Codec::Hevc => 0x60_0000,
            };
            self.bitstream[k] = self.add(gart("bitstream", bytes, span, address));
        }
    }

    /// The same buffers, scaled to the picture, one after another.
    fn scaled(&mut self, geometry: &Geometry, map_recon: bool) {
        let mut next = SCALED_BASE;
        let mut place = |layout: &mut Self, name, domain, mappable, kind: u8, bytes: u64| {
            let page = if domain == Domain::Vram && kind != 0 {
                LARGE_PAGE
            } else if domain == Domain::Vram {
                0x1_0000
            } else {
                0x1000
            };
            next = next.next_multiple_of(page.max(0x1_0000));
            let span = bytes.next_multiple_of(page);
            let address = next;
            next += span;
            layout.add(Plan {
                name,
                domain,
                mappable,
                kind,
                bytes,
                span,
                address,
            })
        };
        let mbs = u64::from(geometry.macroblocks());
        let surface = geometry.surface_bytes();
        let setup = self.codec.setup_bytes() as u64;
        // Per-picture engine data, scaled from the captured 8160 macroblocks.
        let (mepred, rc_data, bitstream) = match self.codec {
            Codec::H264 => (
                (mbs * 8).next_multiple_of(0x1_0000).max(0x1_0000),
                0x5000,
                (surface + 0xf000).next_multiple_of(0x1000),
            ),
            Codec::Hevc => (
                (mbs * 16).next_multiple_of(0x1_0000).max(0x2_0000),
                0xd000,
                (surface * 5 / 3).next_multiple_of(0x1000),
            ),
        };
        let history = (u64::from(geometry.mb_width()) * 384)
            .next_multiple_of(0x1_0000)
            .max(0x1_0000);
        self.coloc_stride =
            (u64::from(geometry.mb_width()) * u64::from(geometry.mb_height() + 2) * 64)
                .next_multiple_of(0x100);
        self.push = place(self, "push", Domain::Gart, false, 0, PUSH_BYTES);
        self.fence = place(self, "fence", Domain::Gart, false, 0, 0x1000);
        for k in 0..SETUPS {
            self.setup[k] = place(self, "setup", Domain::Gart, false, 0, setup);
        }
        for k in 0..2 {
            self.mepred[k] = place(self, "mepred", Domain::Vram, false, 0, mepred);
        }
        self.history = place(self, "history", Domain::Vram, false, 0, history);
        self.rc_process = place(self, "rc_process", Domain::Gart, false, 0, 0x1000);
        for k in 0..SLOTS {
            self.status[k] = place(self, "status", Domain::Gart, false, 0, 0x1000);
            self.rc_data[k] = place(self, "rc_data", Domain::Gart, false, 0, rc_data);
        }
        for k in 0..RECONS {
            self.recon[k] = place(
                self,
                "recon",
                Domain::Vram,
                map_recon,
                SURFACE_KIND,
                surface,
            );
        }
        if self.codec == Codec::H264 {
            let bytes = (self.coloc_stride * RECONS as u64).next_multiple_of(0x1_0000);
            self.coloc = Some(place(self, "coloc", Domain::Vram, false, 0, bytes));
        }
        for k in 0..SLOTS {
            self.input[k] = place(self, "input", Domain::Vram, true, SURFACE_KIND, surface);
            self.bitstream[k] = place(self, "bitstream", Domain::Gart, false, 0, bitstream);
        }
    }

    pub fn address(&self, index: usize) -> u64 {
        self.plans[index].address
    }

    /// The bytes of each bitstream buffer, which the setups name.
    pub fn bitstream_bytes(&self) -> u64 {
        self.plans[self.bitstream[0]].bytes
    }
}
