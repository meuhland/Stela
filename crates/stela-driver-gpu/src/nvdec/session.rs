//! A decoding session: one NVDEC channel and its buffers, on a render node
//! file of its own, decoding one picture at a time.
//!
//! [`Session::open`] is NVENC's bring-up (`nvenc::session`) on the decoder:
//! `VM_INIT`, a channel on the NVDEC runlist (Linux patch 0208), the
//! NVC7B0 object on it, a syncobj, and every buffer of the [`Layout`] made
//! by `GEM_NEW`, bound by `VM_BIND` at its address and, when the CPU needs
//! it, mapped and zeroed.
//!
//! [`Session::decode`] writes the picture's slices, its setup page and its
//! method stream, submits it with `EXEC` and waits for the stream's own
//! semaphore release, at most the session's bound. A picture that never
//! completes marks the session dead; one whose job is still pending keeps
//! the channel and its file open ([`Closed::keeps_file`]), since freeing
//! either makes nouveau idle the channel, a busy wait of up to 15 s.
//! [`Session::planes`] then copies the picture's surface out of BAR1, the
//! fallback of the copy engine's readback ([`super::copy`]), which opens its
//! channel on this file ([`Session::node`]) and, when its own job never
//! completes, makes the session keep everything ([`Session::hold`]).
use super::fields::status as st;
use super::layout::{CHROMA_OFFSET, KERNEL_VA_START, Layout, PUSH_BYTES, VA_TOP};
use super::push::{self, MOST_WORDS};
use super::setup::{
    self, H264_SLICE_OFFSETS, H264_STATUS, HEVC_STATUS, HEVC_TILE_SIZES, PAGE_BYTES,
};
use super::{
    Codec, DECODER_CLASS, FIFO_ENGINE_NVDEC, OBJECT_HANDLE, Parsed, Picture, decoder_class,
};
use crate::nvenc::layout::{Domain, Geometry};
use crate::nvenc::session::Closed;
use crate::nvenc::{
    CHANNEL_ALLOC, CHANNEL_FREE, ChannelAlloc, ChannelFree, DOMAIN_COHERENT, DOMAIN_GART,
    DOMAIN_MAPPABLE, DOMAIN_VRAM, EXEC, Exec, ExecPush, GEM_CLOSE, GEM_NEW, GROBJ_ALLOC, GemClose,
    GemInfo, GemNew, GrobjAlloc, Node, SYNCOBJ_CREATE, SYNCOBJ_DESTROY, SYNCOBJ_WAIT, Sync,
    SyncobjCreate, SyncobjDestroy, SyncobjWait, VM_BIND, VM_BIND_OP_MAP, VM_INIT, VmBind, VmBindOp,
    VmInit,
};
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{Ordering, fence};

/// A frame's submission never crosses the ring's end.
const PUSH_ROOM: u32 = (MOST_WORDS * 4) as u32 + 256;
const ETIME: i64 = -62;
const ETIMEDOUT: i64 = -110;
/// How long [`Session::close`] lets a frame in flight complete.
pub const CLOSE_WAIT_NS: u64 = 50_000_000;
/// Zeros written after each picture's slices.
const TAIL_BYTES: usize = 64;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The chipset has no decoder this module drives.
    NoDecoder,
    /// The session or picture is not one the decoder takes.
    Config(&'static str),
    /// nouveau refused `step` with this errno.
    Refused { step: &'static str, errno: i64 },
    /// nouveau made a buffer larger than its addresses' span.
    TooLarge { name: &'static str, bytes: u64 },
    /// The CPU mapping of a buffer was refused.
    Map { name: &'static str },
    /// The picture's semaphore never held its payload within the bound;
    /// `value` is what it held. `job_done` says whether nouveau's fence
    /// after the push signalled: if not, the channel is still busy.
    Timeout { value: u32, job_done: bool },
    /// An earlier picture timed out: nothing more is submitted.
    Dead,
}

/// What the engine reported for a picture (`nvdec_status_s`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub mbs_correctly_decoded: u32,
    pub mbs_in_error: u32,
    pub cycle_count: u32,
    pub error_status: u32,
    pub slice_header_error_code: u32,
}

impl Status {
    fn read(page: &[u8]) -> Self {
        let get = |field: crate::nvenc::setup::Field| field.get(page) as u32;
        Self {
            mbs_correctly_decoded: get(st::MBS_CORRECTLY_DECODED),
            mbs_in_error: get(st::MBS_IN_ERROR),
            cycle_count: get(st::CYCLE_COUNT),
            error_status: get(st::ERROR_STATUS),
            slice_header_error_code: get(st::SLICE_HEADER_ERROR_CODE),
        }
    }

    /// No error reported: NVIDIA's driver saw `error_status` 0 and
    /// `mbs_in_error` 0 with every macroblock counted on good frames.
    pub fn clean(&self) -> bool {
        self.error_status == 0 && self.mbs_in_error == 0 && self.slice_header_error_code == 0
    }
}

/// Where a picture's time went, in nanoseconds of [`Node::now_ns`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// Writing the slices, setup and methods.
    pub write_ns: u64,
    /// From the submit to the semaphore.
    pub gpu_ns: u64,
}

/// A decoded picture, still in its surface slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// The session's decode count of it.
    pub index: u32,
    pub slot: u8,
    pub status: Status,
    pub timing: Timing,
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

#[derive(Clone, Copy, Debug)]
struct InFlight {
    index: u32,
    slot: u8,
    ring: usize,
    payload: u32,
    written: u64,
    submitted: u64,
    deadline: u64,
    job_done: bool,
}

pub struct Session<N: Node> {
    node: N,
    codec: Codec,
    geometry: Geometry,
    layout: Layout,
    channel: i32,
    syncobj: u32,
    buffers: Vec<Buffer>,
    push_at: u32,
    fence_seq: u32,
    decoded: u32,
    timeout_ns: u64,
    state: State,
    in_flight: Option<InFlight>,
    /// Another channel on the file (the copy engine's) has a job that never
    /// finished reading the surfaces: [`Session::close`] keeps everything.
    held: bool,
}

impl<N: Node> Session<N> {
    /// Opens a `codec` session of `slots` surfaces of `geometry` on `node`,
    /// a fresh render node file of `chipset`'s GPU; each picture waits at
    /// most `timeout_ns`. On a refusal the node comes back with everything
    /// made so far given back: nothing was submitted, so nothing waits.
    pub fn open(
        node: N,
        chipset: u16,
        codec: Codec,
        geometry: Geometry,
        slots: usize,
        timeout_ns: u64,
    ) -> Result<Self, (Error, N)> {
        if decoder_class(chipset).is_none() {
            return Err((Error::NoDecoder, node));
        }
        let layout = match Layout::new(codec, &geometry, slots) {
            Ok(layout) => layout,
            Err(why) => return Err((Error::Config(why), node)),
        };
        let mut session = Self {
            node,
            codec,
            geometry,
            layout,
            channel: -1,
            syncobj: 0,
            buffers: Vec::new(),
            push_at: 0,
            fence_seq: 0,
            decoded: 0,
            timeout_ns,
            state: State::Open,
            in_flight: None,
            held: false,
        };
        let made = session.start_channel().and_then(|()| {
            for index in 0..session.layout.plans.len() {
                let buffer = session.make(index)?;
                session.buffers.push(buffer);
            }
            Ok(())
        });
        match made {
            Ok(()) => Ok(session),
            Err(error) => {
                session.give_back();
                Err((error, session.node))
            }
        }
    }

    fn control<T>(
        &mut self,
        request: u32,
        argument: &mut T,
        step: &'static str,
    ) -> Result<(), Error> {
        match self.node.control(request, (argument as *mut T).cast()) {
            0 => Ok(()),
            errno => Err(Error::Refused { step, errno }),
        }
    }

    fn start_channel(&mut self) -> Result<(), Error> {
        let mut init = VmInit {
            kernel_managed_addr: KERNEL_VA_START,
            kernel_managed_size: VA_TOP - KERNEL_VA_START,
        };
        self.control(VM_INIT, &mut init, "VM_INIT")?;
        let mut alloc = ChannelAlloc {
            fb_ctxdma_handle: !0,
            tt_ctxdma_handle: FIFO_ENGINE_NVDEC,
            ..ChannelAlloc::default()
        };
        self.control(CHANNEL_ALLOC, &mut alloc, "channel")?;
        self.channel = alloc.channel;
        let mut object = GrobjAlloc {
            channel: self.channel,
            handle: OBJECT_HANDLE,
            class: DECODER_CLASS as i32,
        };
        self.control(GROBJ_ALLOC, &mut object, "class")?;
        let mut create = SyncobjCreate::default();
        self.control(SYNCOBJ_CREATE, &mut create, "syncobj")?;
        self.syncobj = create.handle;
        Ok(())
    }

    /// Makes, binds and maps the layout's buffer `index`.
    fn make(&mut self, index: usize) -> Result<Buffer, Error> {
        let plan = self.layout.plans[index];
        let domain = match plan.domain {
            Domain::Vram if plan.mappable => DOMAIN_VRAM | DOMAIN_MAPPABLE,
            Domain::Vram => DOMAIN_VRAM,
            Domain::Gart => DOMAIN_GART | DOMAIN_COHERENT,
        };
        let mut new = GemNew {
            info: GemInfo {
                size: plan.bytes,
                domain,
                tile_flags: u32::from(plan.kind) << 8,
                ..GemInfo::default()
            },
            channel_hint: 0,
            align: if plan.domain == Domain::Vram {
                0x1_0000
            } else {
                0x1000
            },
        };
        self.control(GEM_NEW, &mut new, plan.name)?;
        let handle = new.info.handle;
        let bytes = new.info.size;
        let mut buffer = Buffer {
            handle,
            bytes,
            cpu: None,
        };
        if bytes > plan.span {
            self.close_handle(handle);
            return Err(Error::TooLarge {
                name: plan.name,
                bytes,
            });
        }
        let mut op = VmBindOp {
            op: VM_BIND_OP_MAP,
            flags: u32::from(plan.kind),
            handle,
            addr: plan.address,
            range: bytes,
            ..VmBindOp::default()
        };
        let mut bind = VmBind {
            op_count: 1,
            op_ptr: (&raw mut op) as u64,
            ..VmBind::default()
        };
        if let Err(error) = self.control(VM_BIND, &mut bind, plan.name) {
            self.close_handle(handle);
            return Err(error);
        }
        if plan.mapped() {
            let Some(cpu) = self.node.map(new.info.map_handle, bytes as usize) else {
                self.close_handle(handle);
                return Err(Error::Map { name: plan.name });
            };
            // SAFETY: the mapping is `bytes` long and writable. Surfaces
            // are zeroed too, so a picture the engine never wrote reads 0.
            unsafe { cpu.as_ptr().write_bytes(0, plan.bytes as usize) };
            buffer.cpu = Some(cpu);
        }
        Ok(buffer)
    }

    fn close_handle(&mut self, handle: u32) {
        let mut close = GemClose { handle, pad: 0 };
        self.node.control(GEM_CLOSE, (&raw mut close).cast());
    }

    /// Unmaps every mapping and, unless a channel is busy, gives back the
    /// buffers, the syncobj and the channel.
    fn give_back(&mut self) {
        for index in 0..self.buffers.len() {
            if let Some(cpu) = self.buffers[index].cpu.take() {
                let bytes = self.buffers[index].bytes as usize;
                self.node.unmap(cpu, bytes);
            }
        }
        if self.keeps_file() {
            return;
        }
        for index in 0..self.buffers.len() {
            let handle = self.buffers[index].handle;
            self.close_handle(handle);
        }
        self.buffers.clear();
        if self.syncobj != 0 {
            let mut destroy = SyncobjDestroy {
                handle: self.syncobj,
                pad: 0,
            };
            self.node
                .control(SYNCOBJ_DESTROY, (&raw mut destroy).cast());
            self.syncobj = 0;
        }
        if self.channel >= 0 {
            let mut free = ChannelFree {
                channel: self.channel,
            };
            self.node.control(CHANNEL_FREE, (&raw mut free).cast());
            self.channel = -1;
        }
    }

    /// Ends the session: its node, and whether the file must stay open. A
    /// picture still in flight gets [`CLOSE_WAIT_NS`] to complete; one that
    /// does not, with its job pending, keeps the channel and its file.
    pub fn close(mut self) -> Closed<N> {
        if self.in_flight.is_some() {
            let deadline = self.node.now_ns().saturating_add(CLOSE_WAIT_NS);
            while let Some(flight) = self.in_flight {
                if self.semaphore() == flight.payload {
                    self.in_flight = None;
                    break;
                }
                let job_done = flight.job_done || self.ask_fence();
                if self.node.now_ns() >= deadline {
                    self.in_flight = None;
                    self.state = State::Dead {
                        keeps_channel: !job_done,
                    };
                    break;
                }
                self.node.pause();
            }
        }
        self.give_back();
        let keeps_file = self.keeps_file();
        Closed {
            node: self.node,
            keeps_file,
        }
    }

    /// Whether [`Session::close`] keeps the channel, the buffers and the
    /// file: a picture's job, or another channel's on the file, is pending.
    fn keeps_file(&self) -> bool {
        self.held
            || matches!(
                self.state,
                State::Dead {
                    keeps_channel: true
                }
            )
    }

    /// The session's render node file, for another channel on the same VM:
    /// the copy engine's ([`super::copy::Copier`]), which reads the surfaces
    /// at their addresses here.
    pub fn node(&mut self) -> &mut N {
        &mut self.node
    }

    /// Keeps the channel, the buffers and the file at [`Session::close`]:
    /// another channel on the file has a job that never finished reading
    /// the surfaces.
    pub fn hold(&mut self) {
        self.held = true;
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The node's clock.
    pub fn now_ns(&mut self) -> u64 {
        self.node.now_ns()
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// Whether a picture timed out, and whether its channel is still busy.
    pub fn dead(&self) -> Option<bool> {
        match self.state {
            State::Open => None,
            State::Dead { keeps_channel } => Some(keeps_channel),
        }
    }

    fn cpu(&self, index: usize) -> NonNull<u8> {
        self.buffers[index]
            .cpu
            .expect("the layout maps every buffer the CPU touches")
    }

    /// Decodes `picture`, waiting for the engine: it reads the semaphore and
    /// asks between reads whether nouveau's fence after the push signalled;
    /// each ask lets the host's Linux run.
    pub fn decode(&mut self, picture: &Picture) -> Result<Decoded, Error> {
        self.submit(picture)?;
        loop {
            if let Some(decoded) = self.poll(true)? {
                return Ok(decoded);
            }
            self.node.pause();
        }
    }

    /// Writes `picture`'s slices, setup and methods and submits them,
    /// without waiting: [`Session::poll`] then looks for the completion.
    pub fn submit(&mut self, picture: &Picture) -> Result<(), Error> {
        if self.state != State::Open {
            return Err(Error::Dead);
        }
        if self.in_flight.is_some() {
            return Err(Error::Config("a picture is in flight"));
        }
        if picture.codec != self.codec {
            return Err(Error::Config("the picture is another codec's"));
        }
        if usize::from(picture.slot) >= self.layout.surfaces.len() {
            return Err(Error::Config(
                "the picture's slot is past the session's surfaces",
            ));
        }
        let started = self.node.now_ns();
        let setup = picture
            .setup(&self.geometry)
            .map_err(|why| Error::Config(why.0))?;
        let data = picture.bitstream();
        let capacity = self.layout.bitstream_bytes() as usize;
        if data.len() + TAIL_BYTES > capacity {
            return Err(Error::Config(
                "the picture's slices exceed the bitstream buffer",
            ));
        }
        let index = self.decoded;
        let ring = index as usize % self.layout.setup.len();
        // SAFETY: the bitstream buffer's mapping holds `capacity` bytes, and
        // nothing else holds it while no picture is in flight.
        unsafe {
            let at = self.cpu(self.layout.bitstream[ring]).as_ptr();
            core::ptr::copy_nonoverlapping(data.as_ptr(), at, data.len());
            at.add(data.len()).write_bytes(0, TAIL_BYTES);
        }
        // SAFETY: the setup page's mapping, PAGE_BYTES long.
        let page = unsafe {
            core::slice::from_raw_parts_mut(self.cpu(self.layout.setup[ring]).as_ptr(), PAGE_BYTES)
        };
        page.fill(0);
        page[..setup.len()].copy_from_slice(&setup);
        match &picture.parsed {
            Parsed::H264(_) => {
                for (k, offset) in picture.slice_offsets().iter().enumerate() {
                    let at = H264_SLICE_OFFSETS + 4 * k;
                    page[at..at + 4].copy_from_slice(&offset.to_le_bytes());
                }
            }
            Parsed::Hevc(hevc) => {
                page[HEVC_TILE_SIZES..HEVC_TILE_SIZES + 4]
                    .copy_from_slice(&setup::hevc_tile_sizes(&hevc.sps));
            }
        }
        self.fence_seq = self.fence_seq.wrapping_add(1).max(1);
        let payload = self.fence_seq;
        let words = push::words(&self.layout, picture, index, payload, index == 0);
        if self.push_at + PUSH_ROOM > PUSH_BYTES as u32 {
            // Every picture is waited for, so the ring is free.
            self.push_at = 0;
        }
        let push_cpu = self.cpu(self.layout.push);
        // SAFETY: the ring's mapping, with room for the words at push_at.
        unsafe {
            let at = push_cpu.as_ptr().add(self.push_at as usize).cast::<u32>();
            for (k, word) in words.iter().enumerate() {
                at.add(k).write_volatile(*word);
            }
        }
        // The slices, setup and words land before the doorbell.
        fence(Ordering::SeqCst);
        let mut exec_push = ExecPush {
            va: self.layout.address(self.layout.push) + u64::from(self.push_at),
            va_len: (words.len() * 4) as u32,
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
        let written = self.node.now_ns();
        self.control(EXEC, &mut exec, "EXEC")?;
        self.push_at += (words.len() as u32 * 4).next_multiple_of(256);
        let deadline = self.node.now_ns().saturating_add(self.timeout_ns);
        self.in_flight = Some(InFlight {
            index,
            slot: picture.slot,
            ring,
            payload,
            written: written.saturating_sub(started),
            submitted: written,
            deadline,
            job_done: false,
        });
        Ok(())
    }

    /// The fence page's semaphore word.
    fn semaphore(&self) -> u32 {
        let word = self.cpu(self.layout.fence).cast::<u32>();
        // SAFETY: the fence page's mapping holds the word.
        unsafe { word.as_ptr().read_volatile() }
    }

    /// Asks nouveau whether the fence after the picture in flight
    /// signalled, which lets the host's Linux run; whether it did.
    pub fn ask_fence(&mut self) -> bool {
        let Some(flight) = self.in_flight else {
            return true;
        };
        if flight.job_done {
            return true;
        }
        let mut handle = self.syncobj;
        let mut wait = SyncobjWait {
            handles: (&raw mut handle) as u64,
            timeout_nsec: 0,
            count_handles: 1,
            ..SyncobjWait::default()
        };
        let done = match self.node.control(SYNCOBJ_WAIT, (&raw mut wait).cast()) {
            0 => true,
            ETIME | ETIMEDOUT => false,
            // Not a reason to stop reading: the semaphore decides.
            _ => false,
        };
        if let Some(flight) = self.in_flight.as_mut() {
            flight.job_done |= done;
        }
        done
    }

    /// One look at the picture in flight: its status once the semaphore
    /// holds its payload, `None` while it does not and the bound has not
    /// passed. Past the bound the picture timed out and the session is dead.
    pub fn poll(&mut self, ask: bool) -> Result<Option<Decoded>, Error> {
        let Some(flight) = self.in_flight else {
            return Err(Error::Config("no picture is in flight"));
        };
        let value = self.semaphore();
        if value == flight.payload {
            fence(Ordering::Acquire);
            self.in_flight = None;
            let done = self.node.now_ns();
            self.decoded += 1;
            let status_at = match self.codec {
                Codec::H264 => H264_STATUS,
                Codec::Hevc => HEVC_STATUS,
            };
            // SAFETY: the setup page's mapping, PAGE_BYTES long.
            let page = unsafe {
                core::slice::from_raw_parts(
                    self.cpu(self.layout.setup[flight.ring])
                        .as_ptr()
                        .cast_const(),
                    PAGE_BYTES,
                )
            };
            let status = Status::read(&page[status_at..]);
            return Ok(Some(Decoded {
                index: flight.index,
                slot: flight.slot,
                status,
                timing: Timing {
                    write_ns: flight.written,
                    gpu_ns: done.saturating_sub(flight.submitted),
                },
            }));
        }
        if self.node.now_ns() >= flight.deadline {
            let job_done = if ask {
                flight.job_done
            } else {
                self.ask_fence()
            };
            self.in_flight = None;
            self.state = State::Dead {
                keeps_channel: !job_done,
            };
            return Err(Error::Timeout { value, job_done });
        }
        if ask {
            self.ask_fence();
        }
        Ok(None)
    }

    /// The raw bytes of surface slot `slot`'s planes, `luma_bytes` of the
    /// luma and `chroma_bytes` of the chroma, copied out of BAR1 in address
    /// order with 8-byte reads (video memory read by the CPU is uncached).
    pub fn planes(
        &self,
        slot: u8,
        luma_bytes: usize,
        chroma_bytes: usize,
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        if self.in_flight.is_some() {
            return None;
        }
        let index = *self.layout.surfaces.get(usize::from(slot))?;
        let buffer = self.buffers.get(index)?;
        let cpu = buffer.cpu?;
        let chroma_at = CHROMA_OFFSET as usize;
        if luma_bytes > chroma_at || chroma_at + chroma_bytes > buffer.bytes as usize {
            return None;
        }
        let read = |from: usize, bytes: usize| {
            let mut out = alloc::vec![0u8; bytes.next_multiple_of(8)];
            let words = out.len() / 8;
            // SAFETY: the mapping holds the buffer's bytes; the words read
            // lie inside it (checked above), and `out` holds them.
            unsafe {
                let source = cpu.as_ptr().add(from).cast::<u64>();
                let target = out.as_mut_ptr().cast::<u64>();
                for k in 0..words {
                    target.add(k).write_unaligned(source.add(k).read_volatile());
                }
            }
            out.truncate(bytes);
            out
        };
        Some((read(0, luma_bytes), read(chroma_at, chroma_bytes)))
    }
}
