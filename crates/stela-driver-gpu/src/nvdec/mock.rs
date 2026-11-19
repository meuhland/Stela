//! A GA106 render node of the patched nouveau with a decoder, for the
//! session's and the self-test's tests: buffers in host memory, `VM_BIND`
//! recorded, and an engine that reads each submission's methods the way
//! NVC7B0 would, checks them against the setup page, and writes a known
//! picture into the current surface, a status, and the release.
//!
//! A copy-engine channel on the same file (`NOUVEAU_FIFO_ENGINE_CE`, class
//! NVC7B5) reads its submissions' methods as `clc7b5.h` defines them and
//! performs each `LAUNCH_DMA`: a block-linear rectangle copied to
//! pitch-linear memory, and the one-word semaphore release.
use super::copy::{self, launch, method as ce};
use super::fields::{h264 as f4, hevc as f5};
use super::method as m;
use super::setup::{H264_STATUS, HEVC_STATUS, PAGE_BYTES};
use super::surface::Tiling;
use super::{APPLICATION_H264, APPLICATION_HEVC, DECODER_CLASS, FIFO_ENGINE_NVDEC};
use crate::nvenc::{
    CHANNEL_ALLOC, CHANNEL_FREE, ChannelAlloc, ChannelFree, DOMAIN_VRAM, EXEC, Exec, ExecPush,
    GEM_CLOSE, GEM_NEW, GROBJ_ALLOC, GemClose, GemNew, GrobjAlloc, Node, SUBCHANNEL,
    SYNCOBJ_CREATE, SYNCOBJ_DESTROY, SYNCOBJ_WAIT, Sync, SyncobjCreate, SyncobjWait, VM_BIND,
    VM_BIND_OP_MAP, VM_INIT, VmBind, VmBindOp, VmInit,
};
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::ptr::NonNull;

/// What the engine does with a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Decodes: [`picture`] of the picture index into the current surface
    /// in `Tiling`, a clean status, and the release.
    Decode(Tiling),
    /// Decodes the first `n` pictures, then never gets past the push.
    HangsAfter(u32),
    /// Takes the methods and signals the fence, but writes nothing.
    Silent,
}

/// What the copy engine does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ce {
    /// Copies as `LAUNCH_DMA` says, and releases.
    Copies,
    /// No copy-engine channel: `CHANNEL_ALLOC` answers this errno.
    NoChannel(i64),
    /// No NVC7B5 on the channel: `GROBJ_ALLOC` answers this errno.
    NoClass(i64),
    /// Never gets past the push: no release, the fence pending.
    Hangs,
    /// Takes the methods and signals the fence, but releases nothing.
    Silent,
    /// Releases, but reads the source as if pitch-linear.
    Garbles,
}

/// The copy engine's channel.
pub const CE_CHANNEL: i32 = 6;
/// The syncobjs the mock hands out: the decoder's, then the copier's.
const SYNCOBJS: [u32; 2] = [78, 79];

/// The picture the mock "decodes" for picture index `index`: pitch-linear
/// planes of `pitch` x `rows`, luma then chroma.
pub fn picture(index: u32, pitch: u32, rows: u32) -> (Vec<u8>, Vec<u8>) {
    let (pitch, rows) = (pitch as usize, rows as usize);
    let mut luma = vec![0u8; pitch * rows];
    let mut chroma = vec![0u8; pitch * rows / 2];
    for y in 0..rows {
        for x in 0..pitch {
            luma[y * pitch + x] = (x + 3 * y + 7 * index as usize) as u8;
        }
    }
    for y in 0..rows / 2 {
        for x in 0..pitch {
            chroma[y * pitch + x] = (5 * x + y + index as usize) as u8;
        }
    }
    (luma, chroma)
}

pub struct Buffer {
    words: Vec<u64>,
    pub bytes: u64,
    pub kind: u8,
    pub address: Option<u64>,
}

impl Buffer {
    fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.bytes as usize;
        // SAFETY: the words hold at least `bytes` bytes.
        unsafe { core::slice::from_raw_parts_mut(self.words.as_mut_ptr().cast(), len) }
    }
}

pub struct Gpu {
    pub engine: Engine,
    /// Errnos to answer each request with, instead of performing it.
    pub refuse: Vec<(u32, i64)>,
    pub requests: Vec<u32>,
    pub buffers: Vec<Buffer>,
    pub closed: Vec<u32>,
    pub mapped: usize,
    pub unmapped: usize,
    pub clock: u64,
    pub vm_init: bool,
    pub channel_freed: bool,
    signaled: bool,
    /// Each submission's (method, data) pairs.
    pub submissions: Vec<Vec<(u32, u32)>>,
    pub frames: u32,
    last_slot: Option<u32>,
    pub ce: Ce,
    pub syncobjs: usize,
    pub ce_freed: bool,
    ce_signaled: bool,
    /// Each copy-engine submission's words, and its (method, data) pairs.
    pub ce_words: Vec<Vec<u32>>,
    pub ce_submissions: Vec<Vec<(u32, u32)>>,
    /// The surfaces' pitch in bytes and luma rows the engine writes:
    /// 1920x1088 unless [`Gpu::with_size`] says otherwise.
    pub size: (u32, u32),
    /// Whether `ERROR_FRM_IDX` must be the previous submission's slot, as
    /// in the P-only clips (pictures with B-frames conceal from a
    /// reference, not the last picture).
    pub check_conceal: bool,
}

impl Gpu {
    pub fn new(engine: Engine) -> Self {
        Self {
            ce: Ce::Copies,
            syncobjs: 0,
            ce_freed: false,
            ce_signaled: false,
            ce_words: Vec::new(),
            ce_submissions: Vec::new(),
            engine,
            refuse: Vec::new(),
            requests: Vec::new(),
            buffers: Vec::new(),
            closed: Vec::new(),
            mapped: 0,
            unmapped: 0,
            clock: 1_000_000_000,
            vm_init: false,
            channel_freed: false,
            signaled: false,
            submissions: Vec::new(),
            frames: 0,
            last_slot: None,
            size: (1920, 1088),
            check_conceal: true,
        }
    }

    /// Writes pictures of `pitch` bytes by `rows` rows, for any clip, and
    /// takes any `ERROR_FRM_IDX`.
    /// The gate's tests use it, built with the `metal` feature.
    #[cfg_attr(not(feature = "metal"), allow(dead_code))]
    pub fn with_size(mut self, pitch: u32, rows: u32) -> Self {
        self.size = (pitch, rows);
        self.check_conceal = false;
        self
    }

    /// The buffer holding GPU address `address`, and the offset in it.
    pub fn at(&mut self, address: u64) -> (&mut Buffer, usize) {
        let buffer = self
            .buffers
            .iter_mut()
            .find(|buffer| {
                buffer
                    .address
                    .is_some_and(|start| address >= start && address < start + buffer.bytes)
            })
            .unwrap_or_else(|| panic!("no buffer bound at {address:#x}"));
        let offset = (address - buffer.address.unwrap()) as usize;
        (buffer, offset)
    }

    fn exec(&mut self, exec: &Exec) {
        assert_eq!(
            (exec.push_count, exec.sig_count, exec.wait_count),
            (1, 1, 0)
        );
        // SAFETY: the session's records, live through its ioctl.
        let (push, signal) = unsafe {
            (
                *(exec.push_ptr as *const ExecPush),
                *(exec.sig_ptr as *const Sync),
            )
        };
        assert_eq!(signal.flags, 0);
        let words: Vec<u32> = {
            let (buffer, offset) = self.at(push.va);
            buffer.bytes_mut()[offset..offset + push.va_len as usize]
                .chunks(4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect()
        };
        let mut methods = Vec::new();
        for pair in words.chunks(2) {
            assert_eq!(pair[0] >> 16, 0x2001, "one-word headers");
            assert_eq!(pair[0] >> 13 & 7, SUBCHANNEL);
            methods.push(((pair[0] & 0xfff) << 2, pair[1]));
        }
        self.submissions.push(methods.clone());
        let frame = self.frames;
        self.frames += 1;
        self.signaled = false;
        let tiling = match self.engine {
            Engine::HangsAfter(n) if frame >= n => return,
            Engine::HangsAfter(_) => Tiling::NVDEC,
            Engine::Silent => {
                self.signaled = true;
                return;
            }
            Engine::Decode(tiling) => tiling,
        };
        self.signaled = true;
        if frame == 0 {
            assert_eq!(
                methods[0],
                (m::SET_OBJECT, DECODER_CLASS),
                "the class first"
            );
        }
        let get = |method: u32| {
            methods
                .iter()
                .rev()
                .find(|(m, _)| *m == method)
                .map(|(_, data)| *data)
                .unwrap_or_else(|| panic!("no method {method:#x}"))
        };
        let shifted = |method: u32| u64::from(get(method)) << 8;
        let hevc = match get(m::SET_APPLICATION_ID) {
            APPLICATION_H264 => false,
            APPLICATION_HEVC => true,
            other => panic!("application {other}"),
        };
        let index = get(m::SET_PICTURE_INDEX);
        let page = shifted(m::SET_DRV_PIC_SETUP_OFFSET);
        let setup = {
            let (buffer, offset) = self.at(page);
            buffer.bytes_mut()[offset..offset + PAGE_BYTES].to_vec()
        };
        // The setup and the methods agree on the picture and its stream.
        let (slot, stream_len, status_at) = if hevc {
            (
                f5::CURR_PIC_IDX.get(&setup),
                f5::STREAM_LEN.get(&setup),
                HEVC_STATUS,
            )
        } else {
            (
                f4::CURR_PIC_IDX.get(&setup),
                f4::STREAM_LEN.get(&setup),
                H264_STATUS,
            )
        };
        // ERROR_FRM_IDX: the previous picture's slot.
        let conceal = get(m::SET_CONTROL_PARAMS) >> 7 & 0x3f;
        if !self.check_conceal {
            assert!(conceal < 32, "ERROR_FRM_IDX");
        } else if frame == 0 {
            assert_eq!(conceal, 0);
        } else {
            assert_eq!(Some(conceal), self.last_slot, "ERROR_FRM_IDX");
        }
        self.last_slot = Some(slot as u32);
        assert_eq!(shifted(m::SET_NVDEC_STATUS_OFFSET), page + status_at as u64);
        let stream = shifted(m::SET_IN_BUF_BASE_OFFSET);
        {
            let (buffer, offset) = self.at(stream);
            let bytes = buffer.bytes_mut();
            assert_eq!(&bytes[offset..offset + 3], &[0, 0, 1], "a start code first");
            assert!(stream_len > 3 && bytes[offset + stream_len as usize - 1] != 0);
        }
        let luma_method = if hevc {
            m::SET_PICTURE_LUMA_OFFSET0 + 4 * 16
        } else {
            m::SET_PICTURE_LUMA_OFFSET0 + 4 * slot as u32
        };
        let chroma_method =
            luma_method - m::SET_PICTURE_LUMA_OFFSET0 + m::SET_PICTURE_CHROMA_OFFSET0;
        let (luma_at, chroma_at) = (shifted(luma_method), shifted(chroma_method));
        let (pitch, rows) = self.size;
        let (luma, chroma) = picture(index, pitch, rows);
        for (address, plane, height) in [(luma_at, &luma, rows), (chroma_at, &chroma, rows / 2)] {
            let (buffer, offset) = self.at(address);
            let bytes = buffer.bytes_mut();
            for y in 0..height as usize {
                for x in (0..pitch as usize).step_by(16) {
                    let at = offset + tiling.offset(x, y, pitch as usize);
                    let from = y * pitch as usize + x;
                    bytes[at..at + 16].copy_from_slice(&plane[from..from + 16]);
                }
            }
        }
        {
            let (buffer, offset) = self.at(page + status_at as u64);
            let status = &mut buffer.bytes_mut()[offset..offset + 16];
            status[0..4].copy_from_slice(&8160u32.to_le_bytes());
            status[8..12].copy_from_slice(&123_456u32.to_le_bytes());
        }
        let address = u64::from(get(m::SEMAPHORE_A)) << 32 | u64::from(get(m::SEMAPHORE_B));
        let payload = get(m::SEMAPHORE_C);
        assert_eq!(get(m::SEMAPHORE_D), 0);
        let (fence, offset) = self.at(address);
        fence.bytes_mut()[offset..offset + 4].copy_from_slice(&payload.to_le_bytes());
    }

    /// Sets the copier to `ce`.
    pub fn with_ce(mut self, ce: Ce) -> Self {
        self.ce = ce;
        self
    }

    fn ce_exec(&mut self, exec: &Exec) {
        assert_eq!(
            (exec.push_count, exec.sig_count, exec.wait_count),
            (1, 1, 0)
        );
        // SAFETY: the copier's records, live through its ioctl.
        let (push, signal) = unsafe {
            (
                *(exec.push_ptr as *const ExecPush),
                *(exec.sig_ptr as *const Sync),
            )
        };
        assert_eq!(signal.handle, SYNCOBJS[1], "the copier's own syncobj");
        let words: Vec<u32> = {
            let (buffer, offset) = self.at(push.va);
            buffer.bytes_mut()[offset..offset + push.va_len as usize]
                .chunks(4)
                .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
                .collect()
        };
        let methods = decode(&words);
        let first = self.ce_submissions.is_empty();
        self.ce_words.push(words);
        self.ce_submissions.push(methods.clone());
        self.ce_signaled = false;
        match self.ce {
            Ce::Hangs => return,
            Ce::Silent => {
                self.ce_signaled = true;
                return;
            }
            _ => {}
        }
        self.ce_signaled = true;
        if first {
            assert_eq!(
                methods[0],
                (ce::SET_OBJECT, copy::COPY_CLASS),
                "the class first"
            );
        } else {
            assert!(methods.iter().all(|(m, _)| *m != ce::SET_OBJECT));
        }
        let mut state = [0u32; 0x800 / 4];
        for (method, data) in methods {
            assert!(method < 0x800, "method {method:#x}");
            state[method as usize / 4] = data;
            if method == ce::LAUNCH_DMA {
                self.launch(&state, data);
            }
        }
    }

    /// One `LAUNCH_DMA` of `data`, with the methods so far in `state`.
    fn launch(&mut self, state: &[u32], data: u32) {
        let get = |method: u32| state[method as usize / 4];
        let address =
            |upper: u32, lower: u32| u64::from(get(upper) & 0x1_ffff) << 32 | u64::from(get(lower));
        assert_eq!(data >> 28, 0, "no error code");
        assert_eq!(data & launch::REMAP, 0, "no component remap");
        assert_eq!(data >> 12 & 3, 0, "virtual addresses");
        assert_eq!(data >> 25 & 1, 0, "a SYS flush");
        assert_ne!(data & launch::FLUSH, 0, "a flush");
        if data & 3 != 0 {
            assert_eq!(data & 3, 2, "NON_PIPELINED");
            assert_eq!(data & launch::SRC_PITCH, 0, "a block-linear source");
            assert_ne!(data & launch::DST_PITCH, 0, "a pitch destination");
            assert_ne!(data & launch::MULTI_LINE, 0, "multi-line");
            assert_eq!(data >> 3 & 3, 0, "the release comes after the copies");
            let block = get(ce::SET_SRC_BLOCK_SIZE);
            assert_eq!(block & !0xf0, 0x1000, "one GOB wide and deep, FERMI_8 GOBs");
            let gobs = 1u32 << (block >> 4 & 0xf);
            let (pitch, rows) = (get(ce::SET_SRC_WIDTH), get(ce::SET_SRC_HEIGHT));
            assert_eq!(get(ce::PITCH_IN), pitch);
            assert_eq!((get(ce::SET_SRC_DEPTH), get(ce::SET_SRC_LAYER)), (1, 0));
            let (x, y) = (get(ce::SRC_ORIGIN_X), get(ce::SRC_ORIGIN_Y));
            let (width, lines) = (get(ce::LINE_LENGTH_IN), get(ce::LINE_COUNT));
            let dst_pitch = get(ce::PITCH_OUT);
            assert!(x + width <= pitch && y + lines <= rows && width <= dst_pitch);
            let src = address(ce::OFFSET_IN_UPPER, ce::OFFSET_IN_LOWER);
            let dst = address(ce::OFFSET_OUT_UPPER, ce::OFFSET_OUT_LOWER);
            let tiling = if self.ce == Ce::Garbles {
                Tiling::Pitch
            } else {
                Tiling::Blocks(gobs)
            };
            let plane = {
                let (buffer, offset) = self.at(src);
                let bytes = Tiling::Blocks(gobs).plane_bytes(pitch, rows);
                buffer.bytes_mut()[offset..offset + bytes].to_vec()
            };
            let (buffer, offset) = self.at(dst);
            let out = buffer.bytes_mut();
            let (pitch, dst_pitch) = (pitch as usize, dst_pitch as usize);
            for line in 0..lines as usize {
                let row = y as usize + line;
                let to = offset + line * dst_pitch;
                let mut k = 0;
                while k < width as usize {
                    let column = x as usize + k;
                    let within = column % 16;
                    let take = (16 - within).min(width as usize - k);
                    let at = tiling.offset(column - within, row, pitch) + within;
                    out[to + k..to + k + take].copy_from_slice(&plane[at..at + take]);
                    k += take;
                }
            }
        }
        match data >> 3 & 3 {
            0 => {}
            1 => {
                assert_eq!(data & 3, 0, "a release of its own");
                let semaphore = address(ce::SET_SEMAPHORE_A, ce::SET_SEMAPHORE_B);
                let payload = get(ce::SET_SEMAPHORE_PAYLOAD);
                let (buffer, offset) = self.at(semaphore);
                buffer.bytes_mut()[offset..offset + 4].copy_from_slice(&payload.to_le_bytes());
            }
            other => panic!("semaphore type {other}"),
        }
    }
}

/// The (method, data) pairs of a push of incrementing methods on
/// subchannel 4.
pub fn decode(words: &[u32]) -> Vec<(u32, u32)> {
    let mut methods = Vec::new();
    let mut at = 0;
    while at < words.len() {
        let header = words[at];
        assert_eq!(header >> 29, 1, "incrementing methods: {header:#010x}");
        assert_eq!(header >> 13 & 7, SUBCHANNEL);
        let count = (header >> 16 & 0x1fff) as usize;
        let method = (header & 0xfff) << 2;
        for k in 0..count {
            methods.push((method + 4 * k as u32, words[at + 1 + k]));
        }
        at += 1 + count;
    }
    methods
}

// SAFETY: the mock's mappings are its buffers, kept for the test.
unsafe impl Node for Gpu {
    fn control(&mut self, request: u32, argument: *mut c_void) -> i64 {
        self.requests.push(request);
        if let Some(&(_, errno)) = self.refuse.iter().find(|(r, _)| *r == request) {
            return errno;
        }
        match request {
            VM_INIT => {
                // SAFETY: the request's own argument.
                let init = unsafe { &*argument.cast::<VmInit>() };
                assert_eq!(
                    (init.kernel_managed_addr, init.kernel_managed_size),
                    (1 << 39, 1 << 39)
                );
                assert!(!self.vm_init, "VM_INIT once per file");
                self.vm_init = true;
            }
            CHANNEL_ALLOC => {
                // SAFETY: as above.
                let alloc = unsafe { &mut *argument.cast::<ChannelAlloc>() };
                assert_eq!(alloc.fb_ctxdma_handle, !0);
                if alloc.tt_ctxdma_handle == copy::FIFO_ENGINE_CE {
                    assert!(self.vm_init, "the copier shares the session's VM");
                    if let Ce::NoChannel(errno) = self.ce {
                        return errno;
                    }
                    alloc.channel = CE_CHANNEL;
                } else {
                    assert_eq!(alloc.tt_ctxdma_handle, FIFO_ENGINE_NVDEC);
                    alloc.channel = 5;
                }
            }
            GROBJ_ALLOC => {
                // SAFETY: as above.
                let object = unsafe { &*argument.cast::<GrobjAlloc>() };
                if object.channel == CE_CHANNEL {
                    assert_eq!(object.class, 0xc7b5);
                    if let Ce::NoClass(errno) = self.ce {
                        return errno;
                    }
                } else {
                    assert_eq!((object.channel, object.class), (5, 0xc7b0));
                }
            }
            SYNCOBJ_CREATE => {
                // SAFETY: as above.
                unsafe {
                    (*argument.cast::<SyncobjCreate>()).handle = SYNCOBJS[self.syncobjs.min(1)]
                };
                self.syncobjs += 1;
            }
            SYNCOBJ_DESTROY => {}
            SYNCOBJ_WAIT => {
                // SAFETY: as above.
                let wait = unsafe { &*argument.cast::<SyncobjWait>() };
                assert_eq!((wait.count_handles, wait.timeout_nsec), (1, 0));
                // SAFETY: the caller's handle, live through its ioctl.
                let handle = unsafe { *(wait.handles as *const u32) };
                let signaled = if handle == SYNCOBJS[1] {
                    self.ce_signaled
                } else {
                    assert_eq!(handle, SYNCOBJS[0]);
                    self.signaled
                };
                if !signaled {
                    return -62;
                }
            }
            GEM_NEW => {
                // SAFETY: as above.
                let new = unsafe { &mut *argument.cast::<GemNew>() };
                let kind = (new.info.tile_flags >> 8) as u8;
                let vram = new.info.domain & DOMAIN_VRAM != 0;
                let page = match (vram, kind) {
                    (true, 0) => 0x1_0000,
                    (true, _) => 0x20_0000,
                    (false, _) => 0x1000,
                };
                let bytes = new.info.size.next_multiple_of(page);
                self.buffers.push(Buffer {
                    words: vec![0; bytes as usize / 8],
                    bytes,
                    kind,
                    address: None,
                });
                new.info.handle = self.buffers.len() as u32;
                new.info.size = bytes;
                new.info.map_handle = (self.buffers.len() as u64) << 32;
            }
            VM_BIND => {
                // SAFETY: as above.
                let bind = unsafe { &*argument.cast::<VmBind>() };
                assert_eq!((bind.op_count, bind.flags), (1, 0));
                // SAFETY: the session's op.
                let op = unsafe { *(bind.op_ptr as *const VmBindOp) };
                assert_eq!(op.op, VM_BIND_OP_MAP);
                let index = op.handle as usize - 1;
                let buffer = &self.buffers[index];
                assert_eq!(u32::from(buffer.kind), op.flags, "the PTE kind");
                assert_eq!(op.range, buffer.bytes);
                assert!(op.addr % 0x1000 == 0 && op.addr + op.range <= 1 << 39);
                let overlaps = self.buffers.iter().any(|other| {
                    other.address.is_some_and(|start| {
                        start < op.addr + op.range && op.addr < start + other.bytes
                    })
                });
                assert!(!overlaps, "{:#x} overlaps another binding", op.addr);
                self.buffers[index].address = Some(op.addr);
            }
            EXEC => {
                // SAFETY: as above.
                let exec = unsafe { *argument.cast::<Exec>() };
                if exec.channel == CE_CHANNEL as u32 {
                    self.ce_exec(&exec);
                } else {
                    assert_eq!(exec.channel, 5);
                    self.exec(&exec);
                }
            }
            GEM_CLOSE => {
                // SAFETY: as above.
                self.closed
                    .push(unsafe { (*argument.cast::<GemClose>()).handle });
            }
            CHANNEL_FREE => {
                // SAFETY: as above.
                match unsafe { (*argument.cast::<ChannelFree>()).channel } {
                    CE_CHANNEL => self.ce_freed = true,
                    5 => self.channel_freed = true,
                    other => panic!("channel {other} freed"),
                }
            }
            _ => panic!("unexpected request {request:#x}"),
        }
        0
    }

    fn map(&mut self, offset: u64, bytes: usize) -> Option<NonNull<u8>> {
        let buffer = &mut self.buffers[(offset >> 32) as usize - 1];
        assert_eq!(bytes as u64, buffer.bytes);
        self.mapped += 1;
        NonNull::new(buffer.words.as_mut_ptr().cast())
    }

    fn unmap(&mut self, _: NonNull<u8>, _: usize) {
        self.unmapped += 1;
    }

    fn now_ns(&mut self) -> u64 {
        self.clock += 1000;
        self.clock
    }

    fn pause(&mut self) {
        self.clock += 1_000_000;
    }
}
