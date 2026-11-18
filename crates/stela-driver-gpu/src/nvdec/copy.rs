//! Decoded pictures out of video memory by the copy engine: GA10x's
//! `AMPERE_DMA_COPY_B` (NVC7B5, `clc7b5.h`) blits each plane of NVDEC's
//! block-linear surface (GOB_2, [`super::surface`]) to pitch-linear NV12 in
//! system memory, and the CPU reads that cached instead of reading video
//! memory over uncached BAR1 (about 215 ms a 1080p frame).
//!
//! **The channel.** nouveau's abi16 opens copy-engine channels already:
//! `CHANNEL_ALLOC` with the engine hack (`fb_ctxdma_handle` ~0) and
//! `NOUVEAU_FIFO_ENGINE_CE` (0x30) puts the channel on a copy engine's
//! runlist (`nvif_fifo_runlist_ce`, one without GR), and `GROBJ_ALLOC`
//! creates NVC7B5 on it unchanged; under GSP-RM nouveau's own buffer moves
//! use the same class. No Linux patch is needed. The channel is opened on
//! the decoding session's own render node file, after its `VM_INIT`, so it
//! shares the session's VM: the surfaces are read at the addresses NVDEC
//! wrote them, and [`Copier`]'s two buffers are bound beside them, past
//! every buffer of NVIDIA's captured layout:
//!
//! - **the ring** ([`RING_ADDRESS`], 64 KiB of system memory): the push
//!   ring, and the semaphore at [`SEMAPHORE_OFFSET`];
//! - **the staging buffer** ([`STAGING_ADDRESS`]): the pitch-linear output,
//!   in system memory (GART) the CPU maps write-back on x86, where the GPU's
//!   writes are snooped.
//!
//! **The methods** ([`words`]), as Mesa's NVK copies a block-linear image to
//! a buffer (`nvk_cmd_copy.c`, `nouveau_copy_rect`) and NVIDIA's UVM
//! releases a copy engine's semaphore (`uvm_hal_maxwell_ce_semaphore_release`
//! with Ampere's `DISABLE_PLC`). Per plane, on subchannel 4:
//!
//! - `OFFSET_IN/OUT_UPPER/LOWER`, `PITCH_IN/OUT`, `LINE_LENGTH_IN` (bytes,
//!   since no component remap), `LINE_COUNT`;
//! - `SET_SRC_BLOCK_SIZE` (one GOB wide, 2 GOBs high, depth one GOB,
//!   `GOB_HEIGHT_FERMI_8`: 0x1010), `SET_SRC_WIDTH` (the pitch in bytes),
//!   `SET_SRC_HEIGHT` (the plane's rows), `SET_SRC_DEPTH` 1, `SET_SRC_LAYER`
//!   0, and `SRC_ORIGIN_X/Y` (the crop, in bytes and rows);
//! - `LAUNCH_DMA` 0x0400_0306: `NON_PIPELINED`, `FLUSH_ENABLE`,
//!   source block-linear, destination pitch, `MULTI_LINE_ENABLE`,
//!   `DISABLE_PLC`, virtual addresses.
//!
//! then `SET_SEMAPHORE_A/B/PAYLOAD` and a `LAUNCH_DMA` 0x0400_000c of no
//! data (`DATA_TRANSFER_TYPE_NONE`, `FLUSH_ENABLE` of type `SYS`,
//! `RELEASE_ONE_WORD_SEMAPHORE`): the payload lands once the planes are in
//! system memory.
//!
//! nvdec-cap-1118 cannot check these words: its tracer watched the NVDEC
//! channel alone, and the copy channels CUDA made (16 NVC7B5 objects, which
//! is where NVIDIA's own pitch-linear output came from) were not dumped.
//!
//! **The wait** is bounded as the decoder's: the semaphore read, nouveau's
//! fence after the push asked between reads. A copy that never lands marks
//! the copier dead; one whose job is still pending keeps the channel and its
//! buffers ([`Copier::keeps_channel`]), and its caller keeps the file.
use super::surface::{Crop, Tiling};
use crate::nvenc::layout::Geometry;
use crate::nvenc::{
    CHANNEL_ALLOC, CHANNEL_FREE, ChannelAlloc, ChannelFree, DOMAIN_COHERENT, DOMAIN_GART, EXEC,
    Exec, ExecPush, GEM_CLOSE, GEM_NEW, GROBJ_ALLOC, GemClose, GemInfo, GemNew, GrobjAlloc, Node,
    SUBCHANNEL, SYNCOBJ_CREATE, SYNCOBJ_DESTROY, SYNCOBJ_WAIT, Sync, SyncobjCreate, SyncobjDestroy,
    SyncobjWait, VM_BIND, VM_BIND_OP_MAP, VmBind, VmBindOp, header,
};
use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{Ordering, fence};

/// `AMPERE_DMA_COPY_B`, GA10x's copy engine (`clc7b5.h`).
pub const COPY_CLASS: u32 = 0xc7b5;
/// `NOUVEAU_FIFO_ENGINE_CE`, upstream's abi16 engine.
pub const FIFO_ENGINE_CE: u32 = 0x30;
/// The copy engine's object handle on its channel.
pub const OBJECT_HANDLE: u32 = 0xc7b5_0001;

/// The `clc7b5.h` methods the copier sends.
pub mod method {
    /// The host's `NV_UDMA_OBJECT`: binds the class to the subchannel.
    pub const SET_OBJECT: u32 = 0x0000;
    pub const SET_SEMAPHORE_A: u32 = 0x0240;
    pub const SET_SEMAPHORE_B: u32 = 0x0244;
    pub const SET_SEMAPHORE_PAYLOAD: u32 = 0x0248;
    pub const LAUNCH_DMA: u32 = 0x0300;
    pub const OFFSET_IN_UPPER: u32 = 0x0400;
    pub const OFFSET_IN_LOWER: u32 = 0x0404;
    pub const OFFSET_OUT_UPPER: u32 = 0x0408;
    pub const OFFSET_OUT_LOWER: u32 = 0x040c;
    pub const PITCH_IN: u32 = 0x0410;
    pub const PITCH_OUT: u32 = 0x0414;
    pub const LINE_LENGTH_IN: u32 = 0x0418;
    pub const LINE_COUNT: u32 = 0x041c;
    pub const SET_SRC_BLOCK_SIZE: u32 = 0x0728;
    pub const SET_SRC_WIDTH: u32 = 0x072c;
    pub const SET_SRC_HEIGHT: u32 = 0x0730;
    pub const SET_SRC_DEPTH: u32 = 0x0734;
    pub const SET_SRC_LAYER: u32 = 0x0738;
    pub const SRC_ORIGIN_X: u32 = 0x0744;
    pub const SRC_ORIGIN_Y: u32 = 0x0748;
}

/// `NVC7B5_LAUNCH_DMA`'s fields.
pub mod launch {
    /// `DATA_TRANSFER_TYPE_NON_PIPELINED` (1:0): waits for the copies
    /// before it.
    pub const NON_PIPELINED: u32 = 2;
    /// `FLUSH_ENABLE_TRUE` (2:2); `FLUSH_TYPE` (25:25) zero is `SYS`.
    pub const FLUSH: u32 = 1 << 2;
    /// `SEMAPHORE_TYPE_RELEASE_ONE_WORD_SEMAPHORE` (4:3).
    pub const RELEASE_ONE_WORD: u32 = 1 << 3;
    /// `SRC_MEMORY_LAYOUT_PITCH` (7:7); zero is block-linear.
    pub const SRC_PITCH: u32 = 1 << 7;
    /// `DST_MEMORY_LAYOUT_PITCH` (8:8).
    pub const DST_PITCH: u32 = 1 << 8;
    /// `MULTI_LINE_ENABLE_TRUE` (9:9).
    pub const MULTI_LINE: u32 = 1 << 9;
    /// `REMAP_ENABLE_TRUE` (10:10), which the copier never sets.
    pub const REMAP: u32 = 1 << 10;
    /// `DISABLE_PLC_TRUE` (26:26), as NVIDIA's UVM launches on C7B5.
    pub const DISABLE_PLC: u32 = 1 << 26;
    /// A plane's copy, block-linear to pitch.
    pub const COPY: u32 = NON_PIPELINED | FLUSH | DST_PITCH | MULTI_LINE | DISABLE_PLC;
    /// The release after the planes.
    pub const RELEASE: u32 = FLUSH | RELEASE_ONE_WORD | DISABLE_PLC;
}

/// `SET_SRC_BLOCK_SIZE` of blocks `gobs` GOBs high: `WIDTH` one GOB (3:0),
/// `HEIGHT` log2 (7:4), `DEPTH` one GOB (11:8), `GOB_HEIGHT_FERMI_8`
/// (15:12).
pub const fn block_size(gobs: u32) -> u32 {
    (gobs.trailing_zeros() & 0xf) << 4 | 1 << 12
}

/// Where the ring is in the session's VM: past every buffer of NVIDIA's
/// captured layout (`super::layout`, whose last, HEVC's bitstream ring,
/// ends at 0x1_2b99_0000).
pub const RING_ADDRESS: u64 = 0x1_3000_0000;
pub const RING_BYTES: u64 = 0x1_0000;
/// The push ring's bytes, before the semaphore.
const PUSH_BYTES: u32 = 0xf000;
/// The semaphore's place in the ring.
pub const SEMAPHORE_OFFSET: u64 = 0xf000;
/// Where the staging buffer is.
pub const STAGING_ADDRESS: u64 = 0x1_3010_0000;
/// The staging buffer's distance from the ring.
const STAGING_FROM_RING: u64 = STAGING_ADDRESS - RING_ADDRESS;
/// The most staging a copier takes: a 4096x4096 picture of 16-bit samples,
/// 4:2:0.
pub const MOST_STAGING: u64 = 0x300_0000;
// The ring, then the staging buffer, below the kernel's addresses.
const _: () = assert!(
    RING_ADDRESS + RING_BYTES <= STAGING_ADDRESS
        && STAGING_ADDRESS + MOST_STAGING <= crate::nvenc::layout::KERNEL_VA_START
);

/// Where a copier's ring goes in a session's VM whose buffers end at
/// `end`: [`RING_ADDRESS`] when they end below it (every captured layout),
/// else the next 2 MiB past them; none when the ring and the most staging
/// would reach the kernel's addresses.
pub fn ring_after(end: u64) -> Option<u64> {
    let ring = RING_ADDRESS.max(end.next_multiple_of(0x20_0000));
    (ring + STAGING_FROM_RING + MOST_STAGING <= crate::nvenc::layout::KERNEL_VA_START)
        .then_some(ring)
}
/// The most planes in one copy.
pub const MOST_BLITS: usize = 4;
/// The words of one plane's copy.
pub const BLIT_WORDS: usize = 20;
const ETIME: i64 = -62;
const ETIMEDOUT: i64 = -110;

/// One plane's copy: `rows` lines of `width` bytes at (`x` bytes, `y`
/// rows) of a block-linear plane, to a pitch-linear one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blit {
    /// The block-linear plane's GPU address.
    pub src: u64,
    /// Its bytes per row and rows.
    pub src_pitch: u32,
    pub src_rows: u32,
    /// Its blocks' height in 8-row GOBs.
    pub gobs: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub rows: u32,
    /// The pitch-linear plane's GPU address and bytes per row.
    pub dst: u64,
    pub dst_pitch: u32,
}

impl Blit {
    /// The bytes it writes from [`Blit::dst`] on.
    pub fn span(&self) -> u64 {
        u64::from(self.rows.saturating_sub(1)) * u64::from(self.dst_pitch) + u64::from(self.width)
    }

    /// Why the engine must not be given it, if so: it reads past its
    /// plane, or writes outside `dst_start..dst_end`.
    pub fn refusal(&self, dst_start: u64, dst_end: u64) -> Option<&'static str> {
        if self.width == 0 || self.rows == 0 {
            return Some("an empty blit");
        }
        if !self.gobs.is_power_of_two() || self.gobs > 32 {
            return Some("a block height the engine has no field for");
        }
        if !self.src_pitch.is_multiple_of(64) || !self.src_rows.is_multiple_of(8 * self.gobs) {
            return Some("a block-linear plane of partial blocks");
        }
        if u64::from(self.x) + u64::from(self.width) > u64::from(self.src_pitch)
            || u64::from(self.y) + u64::from(self.rows) > u64::from(self.src_rows)
        {
            return Some("a blit past its plane");
        }
        if self.x > 0xffff || self.y > 0xffff {
            return Some("an origin past 16 bits");
        }
        if self.width > self.dst_pitch {
            return Some("a line longer than the output's pitch");
        }
        if self.dst < dst_start || self.dst.saturating_add(self.span()) > dst_end {
            return Some("a blit outside the staging buffer");
        }
        None
    }
}

/// The bytes the displayed NV12 picture `crop` takes.
pub fn nv12_bytes(crop: &Crop) -> u64 {
    u64::from(crop.width) * u64::from(crop.height) * 3 / 2
}

/// The blits of a decoded NV12 picture: the displayed rectangle `crop` of
/// the luma at `luma` and of the interleaved chroma at `chroma`, both GOB_2
/// planes of `geometry`'s pitch and coded rows, packed at `dst` as the
/// displayed NV12 picture: `crop.width` bytes a row, the luma rows, then
/// the chroma rows. That is the layout of the golden checksums
/// ([`super::surface::picture`]).
pub fn nv12(luma: u64, chroma: u64, geometry: &Geometry, crop: &Crop, dst: u64) -> [Blit; 2] {
    let gobs = match Tiling::NVDEC {
        Tiling::Blocks(gobs) => gobs,
        Tiling::Pitch => 1,
    };
    let luma = Blit {
        src: luma,
        src_pitch: geometry.pitch,
        src_rows: Tiling::NVDEC.padded_rows(geometry.coded_height),
        gobs,
        x: crop.left,
        y: crop.top,
        width: crop.width,
        rows: crop.height,
        dst,
        dst_pitch: crop.width,
    };
    let chroma = Blit {
        src: chroma,
        src_rows: Tiling::NVDEC.padded_rows(geometry.coded_height / 2),
        // The chroma's samples are byte pairs: its bytes are the luma's.
        y: crop.top / 2,
        rows: crop.height / 2,
        dst: dst + u64::from(crop.width) * u64::from(crop.height),
        ..luma
    };
    [luma, chroma]
}

/// A Fermi-and-later incrementing method of `data`, on subchannel 4.
fn method(words: &mut Vec<u32>, method: u32, data: &[u32]) {
    words.push(header(SUBCHANNEL, method, data.len() as u32));
    words.extend_from_slice(data);
}

/// `OFFSET_*_UPPER` and `SET_SEMAPHORE_A`: bits 16:0 of the address's top.
fn upper(address: u64) -> u32 {
    (address >> 32) as u32 & 0x1_ffff
}

/// A copy's method stream: bind the class when `bind`, copy each of
/// `blits`, then release `payload` to the GPU address `semaphore`.
pub fn words(blits: &[Blit], semaphore: u64, payload: u32, bind: bool) -> Vec<u32> {
    let mut words = Vec::with_capacity(2 + BLIT_WORDS * blits.len() + 6);
    if bind {
        method(&mut words, method::SET_OBJECT, &[COPY_CLASS]);
    }
    for blit in blits {
        method(
            &mut words,
            method::OFFSET_IN_UPPER,
            &[
                upper(blit.src),
                blit.src as u32,
                upper(blit.dst),
                blit.dst as u32,
                blit.src_pitch,
                blit.dst_pitch,
                blit.width,
                blit.rows,
            ],
        );
        method(
            &mut words,
            method::SET_SRC_BLOCK_SIZE,
            &[block_size(blit.gobs), blit.src_pitch, blit.src_rows, 1, 0],
        );
        method(&mut words, method::SRC_ORIGIN_X, &[blit.x, blit.y]);
        method(&mut words, method::LAUNCH_DMA, &[launch::COPY]);
    }
    method(
        &mut words,
        method::SET_SEMAPHORE_A,
        &[upper(semaphore), semaphore as u32, payload],
    );
    method(&mut words, method::LAUNCH_DMA, &[launch::RELEASE]);
    words
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// nouveau refused `step` with this errno.
    Refused { step: &'static str, errno: i64 },
    /// nouveau made a buffer larger than its addresses' span.
    TooLarge { name: &'static str, bytes: u64 },
    /// The CPU mapping of a buffer was refused.
    Map { name: &'static str },
    /// The copy is not one the copier gives the engine.
    Config(&'static str),
    /// The semaphore never held the payload within the bound; `value` is
    /// what it held. `job_done` says whether nouveau's fence after the push
    /// signalled: if not, the channel is still busy.
    Timeout { value: u32, job_done: bool },
    /// An earlier copy failed: nothing more is submitted.
    Dead,
}

impl Error {
    /// Whether the copy engine's channel was left busy.
    pub fn pending(&self) -> bool {
        matches!(
            self,
            Self::Timeout {
                job_done: false,
                ..
            }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { step, errno } => write!(
                f,
                "{step} refused: {} ({errno}){}",
                crate::nvenc::errno_name(*errno),
                match (*step, *errno) {
                    ("copy channel", -22) => ": nouveau found no copy-engine runlist",
                    ("copy class", _) => ": GSP-RM or nouveau has no NVC7B5 on the channel",
                    _ => "",
                }
            ),
            Self::TooLarge { name, bytes } => {
                write!(
                    f,
                    "nouveau made {name} {bytes:#x} bytes, past its addresses"
                )
            }
            Self::Map { name } => write!(f, "the CPU mapping of {name} was refused"),
            Self::Config(why) => write!(f, "refused: {why}"),
            Self::Timeout { value, job_done } => write!(
                f,
                "TIMEOUT: the copy's semaphore held {value:#010x}; its channel fence is {}",
                if *job_done {
                    "done: the engine took the methods but released nothing"
                } else {
                    "pending: the copy channel and its file are left open"
                }
            ),
            Self::Dead => f.write_str("the copier is dead"),
        }
    }
}

/// A copy that landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copied {
    /// From writing the methods to the semaphore, in nanoseconds of
    /// [`Node::now_ns`].
    pub ce_ns: u64,
}

struct Buffer {
    handle: u32,
    bytes: u64,
    cpu: Option<NonNull<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    Dead { keeps_channel: bool },
}

/// A copy-engine channel on a decoding session's file, with its ring and
/// staging buffer.
pub struct Copier {
    /// Where its ring is bound; the staging buffer is 1 MiB past it.
    ring: u64,
    channel: i32,
    syncobj: u32,
    buffers: Vec<Buffer>,
    staging: u64,
    push_at: u32,
    sequence: u32,
    copies: u32,
    timeout_ns: u64,
    state: State,
}

const RING: usize = 0;
const STAGING: usize = 1;

impl Copier {
    /// Opens a copier on `node`, a decoding session's file after its
    /// `VM_INIT`, with `staging` bytes of output; each copy waits at most
    /// `timeout_ns`. On a refusal everything made so far is given back:
    /// nothing was submitted, so nothing waits. One copier a file: as the
    /// session's, its buffers' addresses stay bound until the file closes.
    pub fn open<N: Node>(node: &mut N, staging: u64, timeout_ns: u64) -> Result<Self, Error> {
        Self::open_at(node, RING_ADDRESS, staging, timeout_ns)
    }

    /// [`Copier::open`] with the ring at `ring` ([`ring_after`]) and the
    /// staging buffer 1 MiB past it.
    pub fn open_at<N: Node>(
        node: &mut N,
        ring: u64,
        staging: u64,
        timeout_ns: u64,
    ) -> Result<Self, Error> {
        if !ring.is_multiple_of(0x1_0000)
            || ring + STAGING_FROM_RING + MOST_STAGING > crate::nvenc::layout::KERNEL_VA_START
        {
            return Err(Error::Config("a copy ring outside the session's addresses"));
        }
        if staging == 0 || staging > MOST_STAGING {
            return Err(Error::Config("a staging buffer of 1 byte to 48 MiB"));
        }
        let mut copier = Self {
            ring,
            channel: -1,
            syncobj: 0,
            buffers: Vec::new(),
            staging,
            push_at: 0,
            sequence: 0,
            copies: 0,
            timeout_ns,
            state: State::Open,
        };
        match copier.start(node) {
            Ok(()) => Ok(copier),
            Err(error) => {
                copier.give_back(node);
                Err(error)
            }
        }
    }

    fn start<N: Node>(&mut self, node: &mut N) -> Result<(), Error> {
        let mut alloc = ChannelAlloc {
            fb_ctxdma_handle: !0,
            tt_ctxdma_handle: FIFO_ENGINE_CE,
            ..ChannelAlloc::default()
        };
        control(node, CHANNEL_ALLOC, &mut alloc, "copy channel")?;
        self.channel = alloc.channel;
        let mut object = GrobjAlloc {
            channel: self.channel,
            handle: OBJECT_HANDLE,
            class: COPY_CLASS as i32,
        };
        control(node, GROBJ_ALLOC, &mut object, "copy class")?;
        let mut create = SyncobjCreate::default();
        control(node, SYNCOBJ_CREATE, &mut create, "copy syncobj")?;
        self.syncobj = create.handle;
        make(
            node,
            "copy ring",
            RING_BYTES,
            RING_BYTES,
            self.ring,
            &mut self.buffers,
        )?;
        let bytes = self.staging.next_multiple_of(0x1000);
        make(
            node,
            "copy staging",
            bytes,
            MOST_STAGING,
            self.staging_address(),
            &mut self.buffers,
        )?;
        Ok(())
    }

    /// The channel nouveau gave.
    pub fn channel(&self) -> i32 {
        self.channel
    }

    /// The staging buffer's GPU address: where blits write.
    pub fn staging_address(&self) -> u64 {
        self.ring + STAGING_FROM_RING
    }

    /// The ring's GPU address.
    pub fn ring_address(&self) -> u64 {
        self.ring
    }

    /// The staging bytes asked for.
    pub fn staging_bytes(&self) -> u64 {
        self.staging
    }

    /// Whether a copy failed, and whether its channel is still busy.
    pub fn dead(&self) -> Option<bool> {
        match self.state {
            State::Open => None,
            State::Dead { keeps_channel } => Some(keeps_channel),
        }
    }

    /// Whether the channel is left busy: its file must stay open.
    pub fn keeps_channel(&self) -> bool {
        self.dead() == Some(true)
    }

    fn cpu(&self, index: usize) -> NonNull<u8> {
        self.buffers[index]
            .cpu
            .expect("the copier maps both its buffers")
    }

    fn semaphore(&self) -> u32 {
        // SAFETY: the ring's mapping holds the semaphore word.
        unsafe {
            self.cpu(RING)
                .as_ptr()
                .add(SEMAPHORE_OFFSET as usize)
                .cast::<u32>()
                .read_volatile()
        }
    }

    /// Asks nouveau whether the fence after the last push signalled, which
    /// lets the host's Linux run.
    fn ask_fence<N: Node>(&mut self, node: &mut N) -> bool {
        let mut handle = self.syncobj;
        let mut wait = SyncobjWait {
            handles: (&raw mut handle) as u64,
            timeout_nsec: 0,
            count_handles: 1,
            ..SyncobjWait::default()
        };
        match node.control(SYNCOBJ_WAIT, (&raw mut wait).cast()) {
            0 => true,
            ETIME | ETIMEDOUT => false,
            // Not a reason to stop reading: the semaphore decides.
            _ => false,
        }
    }

    /// Copies `blits` (each writing inside the staging buffer) and waits
    /// for the engine, at most the copier's bound.
    pub fn copy<N: Node>(&mut self, node: &mut N, blits: &[Blit]) -> Result<Copied, Error> {
        if self.state != State::Open {
            return Err(Error::Dead);
        }
        if blits.is_empty() || blits.len() > MOST_BLITS {
            return Err(Error::Config("1 to 4 planes a copy"));
        }
        let start = self.staging_address();
        let end = start + self.staging;
        if let Some(why) = blits.iter().find_map(|blit| blit.refusal(start, end))
        {
            return Err(Error::Config(why));
        }
        let started = node.now_ns();
        self.sequence = self.sequence.wrapping_add(1).max(1);
        let payload = self.sequence;
        let words = words(
            blits,
            self.ring + SEMAPHORE_OFFSET,
            payload,
            self.copies == 0,
        );
        let bytes = (words.len() * 4) as u32;
        if self.push_at + bytes > PUSH_BYTES {
            // Every copy is waited for, so the ring is free.
            self.push_at = 0;
        }
        // SAFETY: the ring's mapping, with room for the words at push_at.
        unsafe {
            let at = self
                .cpu(RING)
                .as_ptr()
                .add(self.push_at as usize)
                .cast::<u32>();
            for (k, word) in words.iter().enumerate() {
                at.add(k).write_volatile(*word);
            }
        }
        // The words land before the doorbell.
        fence(Ordering::SeqCst);
        let mut exec_push = ExecPush {
            va: self.ring + u64::from(self.push_at),
            va_len: bytes,
            flags: 0,
        };
        let mut signal = Sync {
            flags: 0,
            handle: self.syncobj,
            timeline_value: 0,
        };
        let mut exec = Exec {
            channel: self.channel as u32,
            push_count: 1,
            sig_count: 1,
            sig_ptr: (&raw mut signal) as u64,
            push_ptr: (&raw mut exec_push) as u64,
            ..Exec::default()
        };
        if let Err(error) = control(node, EXEC, &mut exec, "copy EXEC") {
            // Nothing was submitted: the channel is idle.
            self.state = State::Dead {
                keeps_channel: false,
            };
            return Err(error);
        }
        self.push_at += bytes.next_multiple_of(256);
        let deadline = node.now_ns().saturating_add(self.timeout_ns);
        let mut job_done = false;
        loop {
            let value = self.semaphore();
            if value == payload {
                fence(Ordering::Acquire);
                self.copies += 1;
                return Ok(Copied {
                    ce_ns: node.now_ns().saturating_sub(started),
                });
            }
            if node.now_ns() >= deadline {
                let job_done = job_done || self.ask_fence(node);
                self.state = State::Dead {
                    keeps_channel: !job_done,
                };
                return Err(Error::Timeout { value, job_done });
            }
            if !job_done {
                job_done = self.ask_fence(node);
            }
            node.pause();
        }
    }

    /// The first `bytes` of the staging buffer, as the last copy left them.
    pub fn output(&self, bytes: usize) -> Option<&[u8]> {
        if bytes as u64 > self.staging || self.state != State::Open {
            return None;
        }
        let cpu = self.buffers.get(STAGING)?.cpu?;
        // SAFETY: the staging mapping holds at least `staging` bytes, and no
        // copy is in flight while the copier is open between copies.
        Some(unsafe { core::slice::from_raw_parts(cpu.as_ptr().cast_const(), bytes) })
    }

    /// Ends the copier: everything is given back unless its channel is
    /// still busy, which keeps the channel and its buffers (unmapped);
    /// whether it did, so the file must stay open.
    pub fn close<N: Node>(mut self, node: &mut N) -> bool {
        self.give_back(node);
        self.keeps_channel()
    }

    /// Ends the copier on a file that stays open anyway (its decoder's
    /// channel is busy): the mappings go, nothing else is given back.
    pub fn leave<N: Node>(mut self, node: &mut N) {
        self.unmap(node);
    }

    fn unmap<N: Node>(&mut self, node: &mut N) {
        for buffer in &mut self.buffers {
            if let Some(cpu) = buffer.cpu.take() {
                node.unmap(cpu, buffer.bytes as usize);
            }
        }
    }

    fn give_back<N: Node>(&mut self, node: &mut N) {
        self.unmap(node);
        if self.keeps_channel() {
            return;
        }
        for buffer in self.buffers.drain(..) {
            close_handle(node, buffer.handle);
        }
        if self.syncobj != 0 {
            let mut destroy = SyncobjDestroy {
                handle: self.syncobj,
                pad: 0,
            };
            node.control(SYNCOBJ_DESTROY, (&raw mut destroy).cast());
            self.syncobj = 0;
        }
        if self.channel >= 0 {
            let mut free = ChannelFree {
                channel: self.channel,
            };
            node.control(CHANNEL_FREE, (&raw mut free).cast());
            self.channel = -1;
        }
    }
}

fn control<N: Node, T>(
    node: &mut N,
    request: u32,
    argument: &mut T,
    step: &'static str,
) -> Result<(), Error> {
    match node.control(request, (argument as *mut T).cast()) {
        0 => Ok(()),
        errno => Err(Error::Refused { step, errno }),
    }
}

fn close_handle<N: Node>(node: &mut N, handle: u32) {
    let mut close = GemClose { handle, pad: 0 };
    node.control(GEM_CLOSE, (&raw mut close).cast());
}

/// Makes `bytes` of system memory, binds it at `address` (where `span`
/// bytes are kept for it), maps it and zeroes it, as `buffers`' next.
fn make<N: Node>(
    node: &mut N,
    name: &'static str,
    bytes: u64,
    span: u64,
    address: u64,
    buffers: &mut Vec<Buffer>,
) -> Result<usize, Error> {
    let mut new = GemNew {
        info: GemInfo {
            size: bytes,
            domain: DOMAIN_GART | DOMAIN_COHERENT,
            ..GemInfo::default()
        },
        channel_hint: 0,
        align: 0x1000,
    };
    control(node, GEM_NEW, &mut new, name)?;
    let handle = new.info.handle;
    let made = new.info.size;
    if made > span {
        close_handle(node, handle);
        return Err(Error::TooLarge { name, bytes: made });
    }
    let mut op = VmBindOp {
        op: VM_BIND_OP_MAP,
        flags: 0,
        handle,
        addr: address,
        range: made,
        ..VmBindOp::default()
    };
    let mut bind = VmBind {
        op_count: 1,
        op_ptr: (&raw mut op) as u64,
        ..VmBind::default()
    };
    if let Err(error) = control(node, VM_BIND, &mut bind, name) {
        close_handle(node, handle);
        return Err(error);
    }
    let Some(cpu) = node.map(new.info.map_handle, made as usize) else {
        close_handle(node, handle);
        return Err(Error::Map { name });
    };
    // SAFETY: the mapping is `made` bytes long and writable.
    unsafe { cpu.as_ptr().write_bytes(0, made as usize) };
    buffers.push(Buffer {
        handle,
        bytes: made,
        cpu: Some(cpu),
    });
    Ok(buffers.len() - 1)
}

#[cfg(test)]
mod tests;
