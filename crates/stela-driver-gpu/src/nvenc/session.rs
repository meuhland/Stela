//! An encoding session: one NVENC channel and its buffers, on a render node
//! file of its own, encoding one picture at a time.
//!
//! [`Session::open`] does nvenc-test's known-good bring-up: `VM_INIT` (the
//! file's VM is then the session's, with nouveau's own mappings above
//! 2^39), a channel on the NVENC runlist, the NVC7B7 object on it, a
//! syncobj, and every buffer of the [`Layout`] made by `GEM_NEW`, bound by
//! `VM_BIND` at its address and, when the CPU needs it, mapped and zeroed.
//! A file takes one session: `VM_INIT` happens once.
//!
//! [`Session::encode`] stores the input picture, writes the frame's setup
//! and method stream, submits it with `EXEC` and waits for the stream's own
//! semaphore release, at most the session's bound. A frame that never
//! completes marks the session dead; one whose job is still pending keeps
//! the channel and its file open ([`Closed::keeps_file`]), since freeing
//! either makes nouveau idle the channel, a busy wait of up to 15 s.
//!
//! Every frame reports `error_status` 2 with `ucode_error_status` 0 on
//! nouveau and GSP-RM (2026-11-15, VM 118), H.264 and HEVC alike, though its
//! output decodes to the engine's own reconstruction and matches the input.
//! It is undocumented, and accepted as nvenc-test accepts it by default;
//! [`Status::accepted_error`] says when a frame had it.
use super::bits::{h264_parameter_sets, hevc_parameter_sets, nal_types, start_code_at};
use super::fields::{h264_status as s4, hevc_status as s5};
use super::layout::{
    Domain, Geometry, IMPORT_BASE, KERNEL_VA_START, Layout, PUSH_BYTES, RECONS, SETUPS, SLOTS,
    VA_TOP,
};
use super::push::{self, MOST_WORDS};
use super::setup::{self, Picture, Templates};
use super::surface::{self, Nv12, Rgb};
use super::*;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};

/// The bytes of a status page; the engine writes `nvenc_pic_stat_s` at its
/// start.
const STATUS_BYTES: usize = 0x1000;
/// A frame's submission never crosses the ring's end.
const PUSH_ROOM: u32 = (MOST_WORDS * 4) as u32 + 256;
/// `-ETIME`, a syncobj wait that ran out.
const ETIME: i64 = -62;
const ETIMEDOUT: i64 = -110;
/// How long [`Session::close`] lets a frame in flight complete.
pub const CLOSE_WAIT_NS: u64 = 50_000_000;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The chipset has no encoder this module drives.
    NoEncoder,
    /// The configuration is not one the encoder takes.
    Config(&'static str),
    /// nouveau refused `step` with this errno.
    Refused { step: &'static str, errno: i64 },
    /// nouveau made a buffer larger than its addresses' span.
    TooLarge { name: &'static str, bytes: u64 },
    /// The CPU mapping of a buffer was refused.
    Map { name: &'static str },
    /// The input does not hold the session's picture.
    Input(&'static str),
    /// The frame's semaphore never held its payload within the bound;
    /// `value` is what it held. `job_done` says whether nouveau's fence
    /// after the push signalled: if not, the channel is still busy.
    Timeout { value: u32, job_done: bool },
    /// The engine completed the frame but its status or output is wrong.
    Engine { why: &'static str, status: Status },
    /// An earlier frame timed out: nothing more is submitted.
    Dead,
}

/// What the engine reported for a frame (`nvenc_pic_stat_s`,
/// `nvenc_h265_pic_stat_s`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub picture_index: u32,
    pub error_status: u32,
    pub ucode_error_status: u32,
    pub total_bits: u32,
    pub picture_type: u32,
    pub average_qp: u32,
    pub bitstream_start: u32,
    pub last_valid_byte: u32,
    /// Intra and inter macroblocks (H.264) or 32x32 CUs (HEVC).
    pub intra: u32,
    pub inter: u32,
}

impl Status {
    fn read(codec: Codec, page: &[u8]) -> Self {
        let get = |field: setup::Field| field.get(page) as u32;
        match codec {
            Codec::H264 => Self {
                picture_index: get(s4::PICTURE_INDEX),
                error_status: get(s4::ERROR_STATUS),
                ucode_error_status: get(s4::UCODE_ERROR_STATUS),
                total_bits: get(s4::TOTAL_BIT_COUNT),
                picture_type: get(s4::PIC_TYPE),
                average_qp: get(s4::AVGQP),
                bitstream_start: get(s4::BITSTREAM_START_POS),
                last_valid_byte: get(s4::LAST_VALID_BYTE_OFFSET),
                intra: get(s4::INTRA_MB_COUNT),
                inter: get(s4::INTER_MB_COUNT),
            },
            Codec::Hevc => Self {
                picture_index: get(s5::PICTURE_INDEX),
                error_status: get(s5::ERROR_STATUS),
                ucode_error_status: get(s5::UCODE_ERROR_STATUS),
                total_bits: get(s5::TOTAL_BIT_COUNT),
                picture_type: get(s5::PIC_TYPE),
                average_qp: get(s5::AVGQP),
                bitstream_start: get(s5::BITSTREAM_START_POS),
                last_valid_byte: get(s5::LAST_VALID_BYTE_OFFSET),
                intra: get(s5::INTRA_CU32X32_COUNT),
                inter: get(s5::INTER_CU32X32_COUNT),
            },
        }
    }

    /// `error_status` 2 with `ucode_error_status` 0: undocumented, seen on
    /// every frame under nouveau with known-good output, and accepted.
    pub fn accepted_error(&self) -> bool {
        self.error_status == 2 && self.ucode_error_status == 0
    }

    fn error_ok(&self) -> bool {
        (self.error_status == 0 && self.ucode_error_status == 0) || self.accepted_error()
    }
}

/// A frame to encode.
#[derive(Clone, Copy, Debug)]
pub enum Frame<'a> {
    /// NV12 in memory; the session stores it into its own input surface.
    Nv12(Nv12<'a>),
    /// 32-bit RGB in memory; converted to NV12 on the CPU as it is stored.
    Rgb(Rgb<'a>),
    /// A block-linear NV12 surface of the session's geometry already at this
    /// GPU address of the session's VM ([`Session::bind_import`]).
    Gpu { address: u64 },
    /// The next frame's own input surface, which the caller stored itself
    /// through [`Session::input_surface`], in bands perhaps
    /// ([`surface::store_rows`]).
    Stored,
}

/// Per-frame choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Make this frame an IDR.
    pub force_idr: bool,
}

/// Where a frame's time went, in nanoseconds of [`Node::now_ns`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// Storing or converting the input.
    pub store_ns: u64,
    /// From the submit to the semaphore.
    pub wait_ns: u64,
    pub total_ns: u64,
}

/// An encoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Encoded {
    /// Annex B, each access unit starting `00 00 00 01`: the parameter sets
    /// first on an IDR, then the engine's slice NALs.
    pub bytes: Vec<u8>,
    pub idr: bool,
    pub picture: u32,
    pub status: Status,
    pub timing: Timing,
}

/// A session given back: its node, and whether its file must stay open
/// because a channel is still busy.
pub struct Closed<N> {
    pub node: N,
    pub keeps_file: bool,
}

/// One buffer, made.
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

/// A frame submitted and not yet seen complete.
#[derive(Clone, Copy, Debug)]
struct InFlight {
    n: u32,
    idr: bool,
    since_idr: u32,
    slot: usize,
    setup_index: usize,
    payload: u32,
    started: u64,
    stored: u64,
    submitted: u64,
    /// When the bound runs out, from the first look after the submission.
    deadline: u64,
    /// nouveau's fence after the push signalled: the channel is idle.
    job_done: bool,
}

pub struct Session<N: Node> {
    node: N,
    config: Config,
    geometry: Geometry,
    layout: Layout,
    templates: Templates,
    /// The parameter sets every IDR starts with.
    headers: Vec<u8>,
    channel: i32,
    syncobj: u32,
    buffers: Vec<Buffer>,
    imports: u64,
    push_at: u32,
    fence_seq: u32,
    picture: u32,
    since_idr: u32,
    idrs: u32,
    /// The next frame must be an IDR: an engine error may have left its
    /// references wrong.
    idr_next: bool,
    timeout_ns: u64,
    state: State,
    /// The channel, its object and syncobj are made: the buffers follow.
    started: bool,
    in_flight: Option<InFlight>,
}

/// Whether `rate` is one the encoder takes.
fn rate_ok(rate: Rate) -> Result<(), Error> {
    match rate {
        Rate::ConstQp(qp) if qp > 51 => Err(Error::Config("QP is 0 to 51")),
        Rate::Cbr { bitrate } if bitrate < 100_000 => {
            Err(Error::Config("CBR needs at least 100 kb/s"))
        }
        _ => Ok(()),
    }
}

impl<N: Node> Session<N> {
    /// Opens a session for `config` on `node`, a fresh render node file of
    /// `chipset`'s GPU; each frame waits at most `timeout_ns`. On a refusal
    /// the node comes back with everything made so far given back: nothing
    /// was submitted, so nothing waits.
    pub fn open(
        node: N,
        chipset: u16,
        config: Config,
        timeout_ns: u64,
    ) -> Result<Self, (Error, N)> {
        let mut session = Self::begin(node, chipset, config, timeout_ns)?;
        loop {
            match session.bring_up_step() {
                Ok(true) => return Ok(session),
                Ok(false) => {}
                Err(error) => {
                    session.give_back();
                    return Err((error, session.node));
                }
            }
        }
    }

    /// A session for `config` on `node` as [`Session::open`] makes it, but
    /// with nothing asked of nouveau yet: each [`Session::bring_up_step`]
    /// makes a piece of it, so a caller with a short budget can spread the
    /// bring-up over several calls. A refusal here gives the node back.
    pub fn begin(
        node: N,
        chipset: u16,
        config: Config,
        timeout_ns: u64,
    ) -> Result<Self, (Error, N)> {
        if encoder_class(chipset).is_none() {
            return Err((Error::NoEncoder, node));
        }
        let geometry = match Geometry::new(config.width, config.height) {
            Ok(geometry) => geometry,
            Err(why) => return Err((Error::Config(why), node)),
        };
        if !(1..=240).contains(&config.fps) {
            return Err((Error::Config("the frame rate must be 1 to 240"), node));
        }
        if let Err(error) = rate_ok(config.rate) {
            return Err((error, node));
        }
        let layout = Layout::new(config.codec, &geometry, config.map_recon);
        let templates = match Templates::new(&config, &geometry, &layout) {
            Ok(templates) => templates,
            Err(why) => return Err((Error::Config(why), node)),
        };
        let headers = match config.codec {
            Codec::H264 => h264_parameter_sets(&templates.idr, geometry.width, geometry.height),
            Codec::Hevc => {
                match hevc_parameter_sets(
                    &templates.idr,
                    geometry.width,
                    geometry.height,
                    RECONS as u64 - 1,
                ) {
                    Some(headers) => headers,
                    None => return Err((Error::Config("the HEVC setup's parameter sets"), node)),
                }
            }
        };
        Ok(Self {
            node,
            config,
            geometry,
            layout,
            templates,
            headers,
            channel: -1,
            syncobj: 0,
            buffers: Vec::new(),
            imports: IMPORT_BASE,
            push_at: 0,
            fence_seq: 0,
            picture: 0,
            since_idr: 0,
            idrs: 0,
            idr_next: false,
            timeout_ns,
            state: State::Open,
            started: false,
            in_flight: None,
        })
    }

    /// Makes the next piece of a session [`Session::begin`] gave: first
    /// `VM_INIT`, the channel, its object and the syncobj, then one buffer
    /// each call. Whether the session is now whole. After a refusal the
    /// session goes to [`Session::close`], which gives back what was made.
    pub fn bring_up_step(&mut self) -> Result<bool, Error> {
        if !self.started {
            self.start_channel()?;
            self.started = true;
        } else if self.buffers.len() < self.layout.plans.len() {
            let buffer = self.make(self.buffers.len())?;
            self.buffers.push(buffer);
        }
        Ok(self.ready())
    }

    /// Whether every piece is made, so frames may be encoded.
    pub fn ready(&self) -> bool {
        self.started && self.buffers.len() == self.layout.plans.len()
    }

    /// The buffers made so far and the layout's count.
    pub fn made(&self) -> (usize, usize) {
        (self.buffers.len(), self.layout.plans.len())
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
            tt_ctxdma_handle: FIFO_ENGINE_NVENC,
            ..ChannelAlloc::default()
        };
        self.control(CHANNEL_ALLOC, &mut alloc, "channel")?;
        self.channel = alloc.channel;
        let mut object = GrobjAlloc {
            channel: self.channel,
            handle: OBJECT_HANDLE,
            class: ENCODER_CLASS as i32,
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
            // SAFETY: the mapping is `bytes` long and writable.
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
        if matches!(
            self.state,
            State::Dead {
                keeps_channel: true
            }
        ) {
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
    /// frame still in flight gets [`CLOSE_WAIT_NS`] to complete; one that
    /// does not, with its job pending, keeps the channel and its file, as a
    /// timeout does.
    pub fn close(mut self) -> Closed<N> {
        if self.in_flight.is_some() {
            let deadline = self.node.now_ns().saturating_add(CLOSE_WAIT_NS);
            while let Some(flight) = self.in_flight {
                let value = self.semaphore();
                if value == flight.payload {
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
        let keeps_file = matches!(
            self.state,
            State::Dead {
                keeps_channel: true
            }
        );
        Closed {
            node: self.node,
            keeps_file,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// The parameter sets each IDR starts with.
    pub fn headers(&self) -> &[u8] {
        &self.headers
    }

    /// Whether a frame timed out, and whether its channel is still busy.
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

    /// Binds `handle`, a buffer of this session's file (one imported from
    /// another file by PRIME, say), into the session's VM with PTE `kind`,
    /// and gives its GPU address for [`Frame::Gpu`].
    pub fn bind_import(&mut self, handle: u32, bytes: u64, kind: u8) -> Result<u64, Error> {
        let address = self.imports;
        let mut op = VmBindOp {
            op: VM_BIND_OP_MAP,
            flags: u32::from(kind),
            handle,
            addr: address,
            range: bytes,
            ..VmBindOp::default()
        };
        let mut bind = VmBind {
            op_count: 1,
            op_ptr: (&raw mut op) as u64,
            ..VmBind::default()
        };
        self.control(VM_BIND, &mut bind, "import")?;
        self.imports = (address + bytes).next_multiple_of(0x20_0000);
        Ok(address)
    }

    /// The reconstructed picture of `picture`, read back as pitch-wide NV12
    /// of the coded size, while its surface still holds it (the five newest)
    /// and when the session maps them ([`Config::map_recon`]).
    pub fn recon(&self, picture: u32) -> Option<Vec<u8>> {
        if picture >= self.picture || self.picture - picture > RECONS as u32 {
            return None;
        }
        let index = self.layout.recon[(picture % RECONS as u32) as usize];
        let cpu = self.buffers.get(index)?.cpu?;
        // SAFETY: the mapping holds the whole surface.
        Some(unsafe { surface::load_nv12(cpu.as_ptr(), &self.geometry) })
    }

    /// The input surface the next frame reads, for a caller that stores it
    /// itself ([`surface::store_rows`]) and then submits [`Frame::Stored`].
    /// It is the session's own until the frame after it is submitted: the
    /// mapping is `geometry().surface_bytes()` long.
    pub fn input_surface(&self) -> Result<NonNull<u8>, Error> {
        if !self.ready() {
            return Err(Error::Config("the session is still being made"));
        }
        if self.in_flight.is_some() {
            return Err(Error::Input("a frame is in flight"));
        }
        let slot = (self.picture % SLOTS as u32) as usize;
        Ok(self.cpu(self.layout.input[slot]))
    }

    /// Changes the rate control from the next frame on: the templates are
    /// made again from NVIDIA's captured setups at the new rate. The
    /// parameter sets carry no rate, so the stream goes on unchanged.
    pub fn set_rate(&mut self, rate: Rate) -> Result<(), Error> {
        rate_ok(rate)?;
        if self.in_flight.is_some() {
            return Err(Error::Input("a frame is in flight"));
        }
        let config = Config {
            rate,
            ..self.config
        };
        self.templates =
            Templates::new(&config, &self.geometry, &self.layout).map_err(Error::Config)?;
        self.config = config;
        Ok(())
    }

    /// Whether a frame was submitted and not yet seen complete.
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Encodes `frame`: an IDR when it is the session's first, when asked,
    /// or after an engine error; a P frame referencing the newest pictures
    /// otherwise. It waits for the engine, reading the semaphore and asking
    /// between reads whether nouveau's fence after the push signalled; each
    /// ask lets the host's Linux run.
    pub fn encode(&mut self, frame: Frame<'_>, options: Options) -> Result<Encoded, Error> {
        self.submit(frame, options)?;
        loop {
            if let Some(encoded) = self.poll(true)? {
                return Ok(encoded);
            }
            self.node.pause();
        }
    }

    /// Stores `frame`, writes its setup and method stream and submits it,
    /// without waiting: [`Session::poll`] then looks for its completion.
    pub fn submit(&mut self, frame: Frame<'_>, options: Options) -> Result<(), Error> {
        if self.state != State::Open {
            return Err(Error::Dead);
        }
        if !self.ready() {
            return Err(Error::Config("the session is still being made"));
        }
        if self.in_flight.is_some() {
            return Err(Error::Input("a frame is in flight"));
        }
        let started = self.node.now_ns();
        let n = self.picture;
        let idr = n == 0 || options.force_idr || self.idr_next;
        let since_idr = if idr { 0 } else { self.since_idr };
        let slot = (n % SLOTS as u32) as usize;
        let input = match frame {
            Frame::Gpu { address } => address,
            Frame::Stored => self.layout.address(self.layout.input[slot]),
            Frame::Nv12(picture) => {
                if !picture.fits(&self.geometry) {
                    return Err(Error::Input(
                        "the NV12 picture is smaller than the session's",
                    ));
                }
                let index = self.layout.input[slot];
                // SAFETY: the input surface's mapping holds the surface.
                unsafe { surface::store_nv12(self.cpu(index).as_ptr(), &self.geometry, &picture) };
                self.layout.address(index)
            }
            Frame::Rgb(picture) => {
                if !picture.fits(&self.geometry) {
                    return Err(Error::Input(
                        "the RGB picture is smaller than the session's",
                    ));
                }
                let index = self.layout.input[slot];
                // SAFETY: as above.
                unsafe { surface::store_bgra(self.cpu(index).as_ptr(), &self.geometry, &picture) };
                self.layout.address(index)
            }
        };
        let stored = self.node.now_ns();
        let picture = Picture {
            index: n,
            since_idr,
            idrs_before: self.idrs,
            refs: since_idr.min(self.templates.max_refs),
        };
        let setup_index = self.layout.setup[(n % SETUPS as u32) as usize];
        let setup_bytes = self.config.codec.setup_bytes();
        // SAFETY: the setup slot's mapping is `setup_bytes` long; nothing
        // else holds it while no frame is in flight.
        let setup =
            unsafe { core::slice::from_raw_parts_mut(self.cpu(setup_index).as_ptr(), setup_bytes) };
        setup::build(&self.templates, &picture, setup);
        let status_index = self.layout.status[slot];
        // SAFETY: the status page's mapping.
        unsafe { self.cpu(status_index).as_ptr().write_bytes(0, STATUS_BYTES) };
        self.fence_seq = self.fence_seq.wrapping_add(1);
        let payload = self.fence_seq;
        let words = push::words(
            &self.layout,
            &push::Frame {
                index: n,
                idr,
                refs: picture.refs,
                control: self.config.codec.control(self.config.rate),
                payload,
                input,
                preamble: n == 0,
            },
        );
        if self.push_at + PUSH_ROOM > PUSH_BYTES as u32 {
            // Every frame is waited for, so the ring is free.
            self.push_at = 0;
        }
        let push_cpu = self.cpu(self.layout.push);
        // SAFETY: the ring's mapping, with room for the words at push_at.
        unsafe {
            let at = push_cpu.as_ptr().add(self.push_at as usize).cast::<u32>();
            for (index, word) in words.iter().enumerate() {
                at.add(index).write_volatile(*word);
            }
        }
        // The setup, input and push words land before the doorbell.
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
        let submitted = self.node.now_ns();
        self.control(EXEC, &mut exec, "EXEC")?;
        self.push_at += (words.len() as u32 * 4).next_multiple_of(256);
        let deadline = self.node.now_ns().saturating_add(self.timeout_ns);
        self.in_flight = Some(InFlight {
            n,
            idr,
            since_idr,
            slot,
            setup_index,
            payload,
            started,
            stored,
            submitted,
            deadline,
            job_done: false,
        });
        Ok(())
    }

    /// The fence page's semaphore word.
    fn semaphore(&self) -> u32 {
        let fence_word = self.cpu(self.layout.fence).cast::<u32>();
        // SAFETY: the fence page's mapping holds the word.
        unsafe { fence_word.as_ptr().read_volatile() }
    }

    /// Asks nouveau whether the fence after the frame in flight signalled,
    /// which lets the host's Linux run; whether it did, now or before.
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
            // Not a reason to stop reading: the semaphore decides, and the
            // channel is kept as if busy.
            _ => false,
        };
        if let Some(flight) = self.in_flight.as_mut() {
            flight.job_done |= done;
        }
        done
    }

    /// One look at the frame in flight: its output once the semaphore holds
    /// its payload, `None` while it does not and the bound has not passed.
    /// With `ask`, a look that finds it pending asks nouveau's fence too
    /// ([`Session::ask_fence`]); without, only a look past the bound does,
    /// to tell a busy channel from an idle one. Past the bound the frame
    /// timed out and the session is dead.
    pub fn poll(&mut self, ask: bool) -> Result<Option<Encoded>, Error> {
        let Some(flight) = self.in_flight else {
            return Err(Error::Input("no frame is in flight"));
        };
        let value = self.semaphore();
        if value == flight.payload {
            fence(Ordering::Acquire);
            self.in_flight = None;
            return self.complete(flight).map(Some);
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

    /// The completed frame's output; the rotation and references move on.
    fn complete(&mut self, flight: InFlight) -> Result<Encoded, Error> {
        let done = self.node.now_ns();
        // The frame is the engine's whichever way its output reads: the
        // rotation and references move on.
        self.picture += 1;
        self.since_idr = flight.since_idr + 1;
        if flight.idr {
            self.idrs += 1;
            self.idr_next = false;
        }
        let timing = Timing {
            store_ns: flight.stored.saturating_sub(flight.started),
            wait_ns: done.saturating_sub(flight.submitted),
            total_ns: done.saturating_sub(flight.started),
        };
        match self.output(flight.n, flight.idr, flight.slot, flight.setup_index) {
            Ok((bytes, status)) => Ok(Encoded {
                bytes,
                idr: flight.idr,
                picture: flight.n,
                status,
                timing,
            }),
            Err(error) => {
                self.idr_next = true;
                Err(error)
            }
        }
    }

    /// Frame `n`'s status and slices, checked, with the parameter sets
    /// first on an IDR.
    fn output(
        &mut self,
        n: u32,
        idr: bool,
        slot: usize,
        setup_index: usize,
    ) -> Result<(Vec<u8>, Status), Error> {
        let codec = self.config.codec;
        // SAFETY: the status page's mapping.
        let page = unsafe {
            core::slice::from_raw_parts(
                self.cpu(self.layout.status[slot]).as_ptr().cast_const(),
                STATUS_BYTES,
            )
        };
        let status = Status::read(codec, page);
        let engine = |why| Err(Error::Engine { why, status });
        if !status.error_ok() {
            return engine("the engine reported an error");
        }
        if status.picture_index != n {
            return engine("the status is another picture's");
        }
        if status.total_bits == 0 {
            return engine("the engine wrote no bits");
        }
        let bitstream_index = self.layout.bitstream[slot];
        let capacity = self.layout.plans[bitstream_index].bytes as usize;
        // SAFETY: the bitstream buffer's mapping.
        let buffer = unsafe {
            core::slice::from_raw_parts(self.cpu(bitstream_index).as_ptr().cast_const(), capacity)
        };
        let bytes = (status.total_bits / 8) as usize;
        // H.264 put its slice at 0. NVIDIA's HEVC setups ask for 0xff, which
        // the status repeats; take the first of the status's, the setup's,
        // the last valid byte's and 0 that holds a start code.
        // SAFETY: the setup slot's mapping, written by this frame.
        let setup = unsafe {
            core::slice::from_raw_parts(
                self.cpu(setup_index).as_ptr().cast_const(),
                codec.setup_bytes(),
            )
        };
        let candidates = [
            status.bitstream_start as usize,
            setup::bitstream_start(codec, setup) as usize,
            (status.last_valid_byte as usize).saturating_sub(bytes),
            0,
        ];
        let Some(start) = candidates
            .into_iter()
            .find(|&at| at + bytes <= capacity && start_code_at(buffer, at))
        else {
            return engine("no start code where the status or setup says the slices are");
        };
        let slices = &buffer[start..start + bytes];
        let hevc = codec == Codec::Hevc;
        let types = nal_types(slices, hevc);
        let expected = match (codec, idr) {
            (Codec::H264, true) => types.first() == Some(&5),
            (Codec::H264, false) => types.first() == Some(&1),
            (Codec::Hevc, true) => matches!(types.first(), Some(19 | 20)),
            (Codec::Hevc, false) => matches!(types.first(), Some(0 | 1)),
        };
        if !expected {
            return engine("the slice is not the picture type asked for");
        }
        let mut out = Vec::with_capacity(self.headers.len() + bytes + 1);
        // The engine's own parameter sets, if it ever writes them, replace
        // this module's.
        let engine_sets = types.iter().any(|t| match codec {
            Codec::H264 => matches!(t, 7 | 8),
            Codec::Hevc => matches!(t, 32..=34),
        });
        if idr && !engine_sets {
            out.extend_from_slice(&self.headers);
        }
        if slices.starts_with(&[0, 0, 1]) {
            out.push(0); // zero_byte: every access unit starts 00 00 00 01
        }
        out.extend_from_slice(slices);
        Ok((out, status))
    }
}

#[cfg(test)]
mod tests;
