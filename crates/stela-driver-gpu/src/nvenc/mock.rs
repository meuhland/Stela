//! A GA106 render node of the patched nouveau with an encoder, for the
//! session's and the self-test's tests: buffers in host memory, `VM_BIND`
//! recorded, and an engine that reads each submission's methods the way
//! NVC7B7 would and writes a status, a slice and a reconstruction.
use super::method as m;
use super::*;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// What the engine does with a submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Encodes: a status, a slice of the setup's picture type, the input
    /// copied as the reconstruction, and the release.
    Encode,
    /// Encodes the first `n` frames, then never gets past the push.
    HangsAfter(u32),
    /// Takes the methods and signals the fence, but writes nothing.
    Silent,
    /// Encodes, but every slice is an IDR slice.
    WrongType,
    /// Encodes, but reports `error_status` 1.
    Error,
}

/// A timing model of the engine and of the host's Linux: a submission's job
/// reaches the channel only once Linux runs after its `EXEC` (a push the
/// platform counts in `pushes`, or a `SYNCOBJ_WAIT`), and its semaphore is
/// released `engine_ns` later, by the shared `clock`.
#[derive(Clone)]
pub struct Timing {
    pub clock: Arc<AtomicU64>,
    pub engine_ns: u64,
    pub pushes: Arc<AtomicU64>,
}

/// A release the timing model holds back: where, what, the pushes counted
/// at the `EXEC`, and when the job reached the channel.
#[derive(Clone, Copy)]
struct Held {
    address: u64,
    payload: u32,
    pushes: u64,
    started: Option<u64>,
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
    pub syncobjs: u32,
    pub syncobj_destroyed: bool,
    signaled: bool,
    /// Each submission's words.
    pub submissions: Vec<Vec<u32>>,
    pub frames: u32,
    pub timing: Option<Timing>,
    held: Option<Held>,
}

impl Gpu {
    pub fn new(engine: Engine) -> Self {
        Self {
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
            syncobjs: 0,
            syncobj_destroyed: false,
            signaled: false,
            submissions: Vec::new(),
            frames: 0,
            timing: None,
            held: None,
        }
    }

    /// The timing model's step: a held job reaches the channel once Linux
    /// ran (`linux`, or a push counted since its `EXEC`), and its release
    /// lands once the engine's time passed.
    fn advance(&mut self, linux: bool) {
        let (Some(timing), Some(mut held)) = (self.timing.clone(), self.held) else {
            return;
        };
        let now = timing.clock.load(Ordering::Relaxed);
        if held.started.is_none() && (linux || timing.pushes.load(Ordering::Relaxed) > held.pushes)
        {
            held.started = Some(now);
        }
        self.held = Some(held);
        if let Some(started) = held.started
            && now >= started + timing.engine_ns
        {
            self.held = None;
            self.signaled = true;
            let (fence, offset) = self.at(held.address);
            fence.bytes_mut()[offset..offset + 4].copy_from_slice(&held.payload.to_le_bytes());
        }
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

    /// `bytes` bytes at GPU address `address`.
    pub fn read(&mut self, address: u64, bytes: usize) -> Vec<u8> {
        let (buffer, offset) = self.at(address);
        buffer.bytes_mut()[offset..offset + bytes].to_vec()
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
        self.submissions.push(words.clone());
        let frame = self.frames;
        self.frames += 1;
        // The syncobj takes this job's fence, not yet signalled.
        self.signaled = false;
        let engine = match self.engine {
            Engine::HangsAfter(n) if frame >= n => return,
            Engine::HangsAfter(_) => Engine::Encode,
            engine => engine,
        };
        self.signaled = true;
        if engine == Engine::Silent {
            return;
        }
        let mut methods = Vec::new();
        for pair in words.chunks(2) {
            assert_eq!(pair[0] >> 16, 0x2001, "one-word headers on subchannel 4");
            assert_eq!(pair[0] >> 13 & 7, SUBCHANNEL);
            methods.push(((pair[0] & 0xfff) << 2, pair[1]));
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
        let codec = match get(m::SET_APPLICATION_ID) {
            APPLICATION_H264 => Codec::H264,
            APPLICATION_H265 => Codec::Hevc,
            other => panic!("application {other}"),
        };
        let picture = get(m::SET_PICTURE_INDEX);
        let idr = {
            let (setup, offset) = self.at(shifted(m::SET_IN_DRV_PIC_SETUP));
            let bytes = &setup.bytes_mut()[offset..offset + codec.setup_bytes()];
            assert!(
                setup::layout_ok(codec, bytes),
                "a setup at the setup method"
            );
            setup::picture_type(codec, bytes) == 3
        };
        let slice_type = idr || engine == Engine::WrongType;
        let mut slice = vec![0, 0, 0, 1];
        match codec {
            Codec::H264 => slice.push(if slice_type { 0x65 } else { 0x41 }),
            Codec::Hevc => slice.extend_from_slice(&[if slice_type { 19 << 1 } else { 1 << 1 }, 1]),
        }
        slice.resize(if idr { 2000 } else { 100 }, 0x88);
        let start = match codec {
            Codec::H264 => 0,
            Codec::Hevc => 0xff,
        };
        {
            let (bitstream, offset) = self.at(shifted(m::SET_OUT_BITSTREAM));
            bitstream.bytes_mut()[offset + start..offset + start + slice.len()]
                .copy_from_slice(&slice);
        }
        {
            let (status, offset) = self.at(shifted(m::SET_OUT_ENC_STATUS));
            let page = &mut status.bytes_mut()[offset..offset + 0x1000];
            let put = |page: &mut [u8], at: usize, value: u32| {
                page[at..at + 4].copy_from_slice(&value.to_le_bytes());
            };
            put(page, 0, picture);
            // error_status 2 (bits 1:0), ucode_error_status 0, as on the GPU.
            put(page, 4, if engine == Engine::Error { 1 } else { 2 });
            put(page, 8, 8 * slice.len() as u32);
            put(page, 12, 8 * slice.len() as u32 - 32);
            page[16] = if idr { 3 } else { 0 };
            page[22] = 25;
            put(page, 32, start as u32);
            put(page, 36, (start + slice.len()) as u32);
        }
        // The reconstruction: the input, as an engine at QP 0 would make it.
        let input: Vec<u8> = {
            let (input, offset) = self.at(shifted(m::SET_IN_CUR_PIC));
            input.bytes_mut()[offset..].to_vec()
        };
        {
            let (recon, offset) = self.at(shifted(m::SET_OUT_REF_PIC_LUMA));
            let bytes = recon.bytes_mut();
            let length = (bytes.len() - offset).min(input.len());
            bytes[offset..offset + length].copy_from_slice(&input[..length]);
        }
        let address = u64::from(get(m::SEMAPHORE_A)) << 32 | u64::from(get(m::SEMAPHORE_B));
        let payload = get(m::SEMAPHORE_C);
        assert_eq!(get(m::SEMAPHORE_D), 0);
        if let Some(timing) = &self.timing {
            // Released later, once Linux pushed the job and the engine ran.
            self.signaled = false;
            self.held = Some(Held {
                address,
                payload,
                pushes: timing.pushes.load(Ordering::Relaxed),
                started: None,
            });
            return;
        }
        let (fence, offset) = self.at(address);
        fence.bytes_mut()[offset..offset + 4].copy_from_slice(&payload.to_le_bytes());
    }
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
                assert_eq!(alloc.tt_ctxdma_handle, FIFO_ENGINE_NVENC);
                alloc.channel = 3;
            }
            GROBJ_ALLOC => {
                // SAFETY: as above.
                let object = unsafe { &*argument.cast::<GrobjAlloc>() };
                assert_eq!((object.channel, object.class), (3, 0xc7b7));
            }
            SYNCOBJ_CREATE => {
                self.syncobjs += 1;
                // SAFETY: as above.
                unsafe { (*argument.cast::<SyncobjCreate>()).handle = 77 };
            }
            SYNCOBJ_DESTROY => self.syncobj_destroyed = true,
            SYNCOBJ_WAIT => {
                // SAFETY: as above.
                let wait = unsafe { &*argument.cast::<SyncobjWait>() };
                assert_eq!((wait.count_handles, wait.timeout_nsec), (1, 0));
                // SAFETY: the session's handle.
                assert_eq!(unsafe { *(wait.handles as *const u32) }, 77);
                // The call lets the host's Linux run.
                self.advance(true);
                if !self.signaled {
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
                assert_eq!(exec.channel, 3);
                self.exec(&exec);
            }
            GEM_CLOSE => {
                // SAFETY: as above.
                self.closed
                    .push(unsafe { (*argument.cast::<GemClose>()).handle });
            }
            CHANNEL_FREE => self.channel_freed = true,
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
        if let Some(timing) = &self.timing {
            let now = timing.clock.fetch_add(1000, Ordering::Relaxed) + 1000;
            self.advance(false);
            return now;
        }
        self.clock += 1000;
        self.clock
    }

    fn pause(&mut self) {
        self.clock += 1_000_000;
    }
}
