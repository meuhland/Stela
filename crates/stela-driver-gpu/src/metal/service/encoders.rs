//! The GPU's video encoder through the Metal service's gate: NVENC sessions
//! ([`crate::nvenc`]) that a frontend opens, encodes with and closes, as
//! `nvenc-phase2.md` section 5 designs them.
//!
//! Nothing of a session is the Metal device's, so its calls never take the
//! device's turn: [`SharedService`](super::SharedService) hands every
//! encoder operation to the GPU's [`GateEncoders`] before it looks for one.
//! The encoders have a lock of their own, which a call only tries: a call
//! that finds another holding it is answered `Pending` and made again a
//! moment later, so no processor waits for another's encoder work. Each
//! call runs on the platform's stack ([`EncoderPlatform::run`]).
//!
//! **No processor stalls.** A call does at most [`STORE_BUDGET_NS`] of work
//! and [`POLL_BUDGET_NS`] of looking, then answers `Pending`, and the
//! frontend calls again; what the call did stays done.
//!
//! - `EncoderOpen` makes one piece of the session at a time
//!   ([`Session::bring_up_step`]): the channel, then each buffer, every one
//!   a host kernel call or a few.
//! - `EncoderEncode` reads the picture's rows from the caller's memory, a
//!   band at a time, and stores each into the session's block-linear input
//!   surface ([`surface::store_rows`]): pitch-linear NV12 cannot reach the
//!   engine any other way. Once every row is stored it submits the frame,
//!   then looks at its semaphore until it completes. A call made again for
//!   the same frame goes on where the last stopped; one for a later frame
//!   starts it.
//! - A frame the engine has not finished within [`FRAME_BOUND_NS`] makes
//!   the session `Lost`: it takes no more frames. A channel whose job is
//!   still pending then keeps its file open, and no session opens on the
//!   GPU again this boot ([`State::refused`]).
//!
//! **Limits and cleanup.** A GPU serves [`MAX_SESSIONS`] sessions at once;
//! Parsec uses one. A connection's close, which a process's death makes,
//! closes its sessions ([`GateEncoders::disconnect`]), as the PC's stop
//! closes every one ([`GateEncoders::close_all`]).
//!
//! **Report.** Each session logs `NVENC SESSION` lines: when it opens, at
//! each platform report while it encodes ([`GateEncoders::report`]), when it
//! is lost and when it closes.
use super::gate::fault;
use super::wire::{
    self, ENCODE_FORCE_IDR, ENCODER_OPEN_FULL_RANGE, EncoderCapabilitiesResponse, EncoderCodec,
    EncoderEncodeRequest, EncoderEncodeResponse, EncoderInput, EncoderOpenRequest,
    EncoderOpenResponse, EncoderRate, Fault, GateCall, Handle, Operation, Span, Status,
};
use crate::nvenc::{
    Codec, Config, Node, Rate,
    layout::MAX_SIDE,
    session::{self, Encoded, Frame, Options, Session},
    surface::{self, Order, Plane, Rgb},
};
use alloc::vec::Vec;
use core::{
    cell::UnsafeCell,
    fmt,
    num::NonZeroU64,
    sync::atomic::{AtomicBool, Ordering},
};
use stela_contracts::gate::{CallerFault, CallerMemory};

#[cfg(test)]
mod tests;

/// The answer to an encoder operation where the host serves none: no
/// NVIDIA GPU with an encoder this port drives, `--no-nvenc`, or a GPU
/// whose encoder failed.
pub const NONE: Fault = Fault::new(Status::Unsupported, "the host serves no video encoder");

/// Sessions one GPU encodes at once: GA106 has one NVENC, which GSP-RM
/// time-slices between channels.
pub const MAX_SESSIONS: usize = 2;
/// About how long one call stores a picture or makes a session, at least
/// one piece, before it answers `Pending`: the service's own budgets are
/// 1 to 2 ms (`WAIT_BOUND_MILLIS`, `LIBRARY_MICROS`).
pub const STORE_BUDGET_NS: u64 = 1_500_000;
/// About how long one call looks for a submitted frame's completion before
/// it answers `Pending`.
pub const POLL_BUDGET_NS: u64 = 1_000_000;
/// How long a frame may take from its submission: past this the session is
/// `Lost`.
pub const FRAME_BOUND_NS: u64 = 1_000_000_000;
/// The rows of a plane one read from the caller's memory brings: an even
/// number, as BGRA's chroma takes rows in pairs.
const BAND_ROWS: usize = 64;

/// What the platform gives the encoders of one GPU.
pub trait EncoderPlatform: Send + Sync + 'static {
    type Node: Node + Send;
    /// The GPU's index, which the log lines name.
    fn index(&self) -> usize;
    /// nouveau's chipset of the GPU.
    fn chipset(&self) -> u16;
    /// A fresh render node file of the GPU, or why there is none.
    fn open_node(&self) -> Result<Self::Node, &'static str>;
    /// Gives back a closed session's file; with `keeps_file` it stays open,
    /// as its channel is still busy.
    fn close_node(&self, node: Self::Node, keeps_file: bool);
    /// A monotonic clock, in nanoseconds.
    fn now_ns(&self) -> u64;
    /// Waits a little between two looks at a semaphore.
    fn pause(&self);
    /// Waits a moment for another holder of the encoders, in a wait with
    /// no bound: it may do the platform's work the holder waits for.
    fn relax(&self);
    /// Runs `body` on a stack an encoder call fits on.
    fn run(&self, body: &mut dyn FnMut());
    /// A line of the platform's log.
    fn log(&self, line: fmt::Arguments<'_>);
}

/// A GPU's encoders as the shared service reaches them.
pub trait GateEncoders: Send + Sync {
    /// Runs `operation`, one of [`is_encoder_operation`]'s, for
    /// `connection`, on its request and response records.
    fn serve(
        &self,
        connection: NonZeroU64,
        operation: Operation,
        request: &[u8],
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault>;
    /// Closes `connection`'s sessions.
    fn disconnect(&self, connection: NonZeroU64);
    /// Logs each session that encoded since the last report, unless a call
    /// holds the encoders now.
    fn report(&self);
    /// Closes every session, for the PC's stop.
    fn close_all(&self);
}

/// Whether `operation` is an encoder's, which never reaches the device.
pub fn is_encoder_operation(operation: u32) -> bool {
    operation & 0xff00 == 0x0a00
}

/// Serves the encoder call `call` at `record` as [`super::Service::serve`]
/// serves the device's: its records through the caller's memory, its status
/// and reason in the record. Answers the status, or why the record itself
/// is out of reach.
pub fn serve_gate(
    encoders: &dyn GateEncoders,
    connection: NonZeroU64,
    record: u64,
    mut call: GateCall,
    caller: &mut dyn CallerMemory,
) -> Result<Status, CallerFault> {
    let outcome = serve_records(encoders, connection, &call, caller);
    let (status, reason) = match outcome {
        Ok(()) => (Status::Ok, alloc::string::String::new()),
        Err(fault) => (fault.status, alloc::format!("{fault}")),
    };
    call.status = status.raw();
    call.reason_length = reason.len() as u64;
    if !reason.is_empty() && call.reason.length >= call.reason_length {
        // The status still reports a reason the caller cannot take.
        let _ = caller.write(call.reason.address, reason.as_bytes());
    }
    let mut raw = [0; size_of::<GateCall>()];
    wire::write(&call, &mut raw).expect("a record of its own size");
    caller.write(record, &raw)?;
    Ok(status)
}

/// The largest request or response record of an encoder operation.
const RECORD_BYTES: usize = 128;

fn serve_records(
    encoders: &dyn GateEncoders,
    connection: NonZeroU64,
    call: &GateCall,
    caller: &mut dyn CallerMemory,
) -> Result<(), Fault> {
    let operation = Operation::from_raw(call.operation)
        .filter(|operation| is_encoder_operation(operation.raw()))
        .ok_or(Fault::new(Status::UnknownOperation, "unknown operation").value(call.operation))?;
    let (request_bytes, response_bytes) = operation.sizes();
    if call.request.length != request_bytes as u64 {
        return Err(
            Fault::malformed("request record").length(call.request.length, request_bytes as u64)
        );
    }
    if call.response.length != response_bytes as u64 {
        return Err(
            Fault::malformed("response record").length(call.response.length, response_bytes as u64)
        );
    }
    let mut request = [0; RECORD_BYTES];
    let request = &mut request[..request_bytes];
    caller.read(call.request.address, request).map_err(fault)?;
    let mut response = [0; RECORD_BYTES];
    let response = &mut response[..response_bytes];
    encoders.serve(connection, operation, request, response, caller)?;
    caller.write(call.response.address, response).map_err(fault)
}

/// Where a session's current frame is.
enum Stage {
    /// Its pieces are being made, for the open of `token`.
    Opening,
    /// No frame, or the last one answered.
    Idle,
    /// Frame `frame`'s rows are being stored: `plane`'s from `row` on.
    Storing {
        frame: u64,
        plane: Plane,
        row: usize,
    },
    /// Frame `frame` was submitted.
    InFlight { frame: u64 },
    /// Frame `frame` is coded and kept until a later frame's call, so a
    /// call made again gets it.
    Done { frame: u64, encoded: Encoded },
    /// Frame `frame` was refused, which a call made again is told again.
    Refused { frame: u64, fault: Fault },
}

/// One frame's request, as its first call gave it.
#[derive(Clone, Copy)]
struct Picture {
    input: EncoderInput,
    luma: Span,
    chroma: Span,
    luma_pitch: usize,
    chroma_pitch: usize,
    force_idr: bool,
    /// When its first call came, and the time its calls spent storing it.
    began: u64,
    convert_ns: u64,
}

/// What a session did since a report, or since it opened.
#[derive(Clone, Copy, Default)]
struct Counts {
    frames: u64,
    bytes: u64,
    idrs: u64,
    errors: u64,
    gpu_ns: u64,
    gpu_max: u64,
    convert_ns: u64,
    convert_max: u64,
    total_ns: u64,
    total_max: u64,
    calls: u64,
    pending: u64,
}

impl Counts {
    fn frame(&mut self, encoded: &Encoded, convert_ns: u64, total_ns: u64) {
        self.frames += 1;
        self.bytes += encoded.bytes.len() as u64;
        self.idrs += u64::from(encoded.idr);
        self.gpu_ns += encoded.timing.wait_ns;
        self.gpu_max = self.gpu_max.max(encoded.timing.wait_ns);
        self.convert_ns += convert_ns;
        self.convert_max = self.convert_max.max(convert_ns);
        self.total_ns += total_ns;
        self.total_max = self.total_max.max(total_ns);
    }

    fn add(&mut self, other: &Self) {
        self.frames += other.frames;
        self.bytes += other.bytes;
        self.idrs += other.idrs;
        self.errors += other.errors;
        self.gpu_ns += other.gpu_ns;
        self.gpu_max = self.gpu_max.max(other.gpu_max);
        self.convert_ns += other.convert_ns;
        self.convert_max = self.convert_max.max(other.convert_max);
        self.total_ns += other.total_ns;
        self.total_max = self.total_max.max(other.total_max);
        self.calls += other.calls;
        self.pending += other.pending;
    }
}

/// `frames=… bytes=… idr=… errors=… gpu_us=avg/max convert_us=avg/max
/// total_us=avg/max calls=… pending=…`.
impl fmt::Display for Counts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let average = |total: u64| total.checked_div(self.frames).unwrap_or(0) / 1000;
        write!(
            formatter,
            "frames={} bytes={} idr={} errors={} gpu_us={}/{} convert_us={}/{} total_us={}/{} \
             calls={} pending={}",
            self.frames,
            self.bytes,
            self.idrs,
            self.errors,
            average(self.gpu_ns),
            self.gpu_max / 1000,
            average(self.convert_ns),
            self.convert_max / 1000,
            average(self.total_ns),
            self.total_max / 1000,
            self.calls,
            self.pending,
        )
    }
}

struct Slot<N: Node> {
    id: u64,
    connection: NonZeroU64,
    token: u64,
    session: Session<N>,
    stage: Stage,
    picture: Option<Picture>,
    /// The rows a band brings, kept between bands.
    band: Vec<u8>,
    lost: bool,
    opened_at: u64,
    period: Counts,
    total: Counts,
}

impl<N: Node> Slot<N> {
    /// `h264 1920x1080 60 fps cbr 20000 kb/s`.
    fn describe(&self) -> alloc::string::String {
        let config = self.session.config();
        let rate = match config.rate {
            Rate::Cbr { bitrate } => alloc::format!("cbr {} kb/s", bitrate / 1000),
            Rate::ConstQp(qp) => alloc::format!("qp {qp}"),
        };
        alloc::format!(
            "{} {}x{} {} fps {rate}",
            config.codec.name(),
            config.width,
            config.height,
            config.fps
        )
    }
}

/// The encoders' state, which their lock gives one call at a time.
struct State<N: Node> {
    sessions: Vec<Slot<N>>,
    next_id: u64,
    /// Why no session opens on this GPU any more: one hung with its channel
    /// busy, or nouveau refused the encoder's channel.
    refused: Option<&'static str>,
}

/// A GPU's encoders, over its platform.
pub struct Encoders<P: EncoderPlatform> {
    platform: P,
    held: AtomicBool,
    state: UnsafeCell<State<P::Node>>,
}

// SAFETY: the lock gives one processor at a time the state. A session's
// mappings are the host's, which every processor reaches at the same
// addresses, and its node is `Send`.
unsafe impl<P: EncoderPlatform> Send for Encoders<P> {}
// SAFETY: as above.
unsafe impl<P: EncoderPlatform> Sync for Encoders<P> {}

/// The state while the lock is held.
struct Held<'a, P: EncoderPlatform>(&'a Encoders<P>);

impl<P: EncoderPlatform> Drop for Held<'_, P> {
    fn drop(&mut self) {
        self.0.held.store(false, Ordering::Release);
    }
}

impl<P: EncoderPlatform> Held<'_, P> {
    fn state(&mut self) -> &mut State<P::Node> {
        // SAFETY: the flag gives this holder the state.
        unsafe { &mut *self.0.state.get() }
    }
}

/// The fault of an encoder call that found another holding the encoders.
const BUSY: Fault = Fault::new(Status::Pending, "the encoders serve another call");
/// The fault of a call whose work for now is done, and which goes on when
/// made again.
const GOES_ON: Fault = Fault::new(Status::Pending, "the encoder goes on with the call");

impl<P: EncoderPlatform> Encoders<P> {
    pub fn new(platform: P) -> Self {
        Self {
            platform,
            held: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                sessions: Vec::new(),
                next_id: 1,
                refused: None,
            }),
        }
    }

    pub fn platform(&self) -> &P {
        &self.platform
    }

    fn try_hold(&self) -> Option<Held<'_, P>> {
        self.held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Held(self))
    }

    fn hold(&self) -> Held<'_, P> {
        loop {
            if let Some(held) = self.try_hold() {
                return held;
            }
            self.platform.relax();
        }
    }

    /// The sessions open now.
    pub fn open_sessions(&self) -> usize {
        let mut held = self.hold();
        held.state().sessions.len()
    }

    fn log(&self, line: fmt::Arguments<'_>) {
        self.platform.log(format_args!(
            "NVENC SESSION GPU {} {line}",
            self.platform.index()
        ));
    }

    /// Closes the session of slot `index`, which leaves the table.
    fn close_slot(&self, state: &mut State<P::Node>, index: usize, why: &str) {
        let slot = state.sessions.remove(index);
        let mut total = slot.total;
        total.add(&slot.period);
        let description = slot.describe();
        let closed = slot.session.close();
        if closed.keeps_file {
            state.refused = Some("an encoder session's channel stayed busy this boot");
        }
        let lived = self.platform.now_ns().saturating_sub(slot.opened_at) / 1_000_000;
        self.log(format_args!(
            "{} closed ({why}) after {lived} ms: {description}, {total}{}",
            slot.id,
            if closed.keeps_file {
                ", its channel still busy: its file stays open, and no session opens again"
            } else {
                ""
            }
        ));
        self.platform.close_node(closed.node, closed.keeps_file);
    }

    fn capabilities(state: &State<P::Node>, response: &mut [u8]) -> Result<(), Fault> {
        if let Some(why) = state.refused {
            return Err(Fault::new(Status::Unsupported, why));
        }
        let reply = EncoderCapabilitiesResponse {
            codecs: 1 << EncoderCodec::H264.raw() | 1 << EncoderCodec::Hevc.raw(),
            inputs: 1 << EncoderInput::Nv12.raw() | 1 << EncoderInput::Bgra.raw(),
            max_sessions: MAX_SESSIONS as u32,
            open_sessions: state.sessions.len() as u32,
            max_width: MAX_SIDE,
            max_height: MAX_SIDE,
        };
        wire::write(&reply, response)
    }

    fn open(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &EncoderOpenRequest,
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        if request.token == 0 {
            return Err(Fault::malformed("encoder open token").value(0u64));
        }
        let index = match state
            .sessions
            .iter()
            .position(|slot| slot.connection == connection && slot.token == request.token)
        {
            Some(index) => index,
            None => self.begin(state, connection, request)?,
        };
        let started = self.platform.now_ns();
        let slot = &mut state.sessions[index];
        while matches!(slot.stage, Stage::Opening) {
            match slot.session.bring_up_step() {
                Ok(true) => {
                    slot.stage = Stage::Idle;
                    let (buffers, _) = slot.session.made();
                    let took = self.platform.now_ns().saturating_sub(slot.opened_at);
                    let line = alloc::format!(
                        "{} open for connection {connection}: {}, coded {}x{}, {buffers} \
                         buffers in {} ms, {} bytes of parameter sets",
                        slot.id,
                        slot.describe(),
                        slot.session.geometry().coded_width,
                        slot.session.geometry().coded_height,
                        took / 1_000_000,
                        slot.session.headers().len()
                    );
                    self.log(format_args!("{line}"));
                }
                Ok(false) => {
                    if self.platform.now_ns().saturating_sub(started) >= STORE_BUDGET_NS {
                        return Err(GOES_ON);
                    }
                }
                Err(error) => {
                    let (made, planned) = slot.session.made();
                    let id = slot.id;
                    let refusal = refusal(error);
                    // A channel nouveau refuses is refused for good.
                    if matches!(
                        error,
                        session::Error::Refused {
                            step: "VM_INIT" | "channel" | "class",
                            ..
                        }
                    ) {
                        state.refused = Some("nouveau refused the encoder's channel this boot");
                    }
                    self.log(format_args!(
                        "{id} refused at open after {made} of {planned} buffers: {error:?}"
                    ));
                    self.close_slot(state, index, "its open failed");
                    return Err(refusal);
                }
            }
        }
        let slot = &state.sessions[index];
        let headers = slot.session.headers();
        if headers.len() as u64 <= request.parameter_sets.length {
            caller
                .write(request.parameter_sets.address, headers)
                .map_err(fault)?;
        }
        let geometry = slot.session.geometry();
        wire::write(
            &EncoderOpenResponse {
                encoder: slot.id,
                parameter_sets_length: headers.len() as u64,
                max_frame_bytes: slot.session.layout().bitstream_bytes() + headers.len() as u64 + 1,
                coded_width: geometry.coded_width,
                coded_height: geometry.coded_height,
            },
            response,
        )
    }

    /// A new slot for `request`, its session begun but nothing made yet.
    fn begin(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &EncoderOpenRequest,
    ) -> Result<usize, Fault> {
        if let Some(why) = state.refused {
            return Err(Fault::new(Status::Unsupported, why));
        }
        let codec = match EncoderCodec::from_raw(request.codec) {
            Some(EncoderCodec::H264) => Codec::H264,
            Some(EncoderCodec::Hevc) => Codec::Hevc,
            None => return Err(Fault::malformed("encoder codec").value(request.codec)),
        };
        let rate = match EncoderRate::from_raw(request.rate) {
            Some(EncoderRate::Cbr) => Rate::Cbr {
                bitrate: if request.bitrate == 0 {
                    Rate::CAPTURED_BITRATE
                } else {
                    request.bitrate
                },
            },
            Some(EncoderRate::ConstQp) => Rate::ConstQp(
                u8::try_from(request.qp)
                    .map_err(|_| Fault::new(Status::Invalid, "QP is 0 to 51").value(request.qp))?,
            ),
            None => return Err(Fault::malformed("encoder rate").value(request.rate)),
        };
        if request.flags & !ENCODER_OPEN_FULL_RANGE != 0 {
            return Err(Fault::malformed("encoder open flags").bits(request.flags));
        }
        if request.flags & ENCODER_OPEN_FULL_RANGE != 0 {
            return Err(Fault::new(
                Status::Unsupported,
                "full-range pictures: the parameter sets signal video range only",
            ));
        }
        if state.sessions.len() >= MAX_SESSIONS {
            return Err(Fault::new(
                Status::Unsupported,
                "every video encoder session of the GPU is in use",
            )
            .value(MAX_SESSIONS as u64));
        }
        let fps = match request.fps_milli {
            0 => 60,
            milli => milli.saturating_add(500) / 1000,
        }
        .clamp(1, 240);
        let config = Config {
            codec,
            width: request.width,
            height: request.height,
            fps,
            rate,
            map_recon: false,
        };
        let node = self
            .platform
            .open_node()
            .map_err(|why| Fault::new(Status::Unsupported, why))?;
        let session = match Session::begin(node, self.platform.chipset(), config, FRAME_BOUND_NS) {
            Ok(session) => session,
            Err((error, node)) => {
                self.platform.close_node(node, false);
                return Err(refusal(error));
            }
        };
        let id = state.next_id;
        state.next_id += 1;
        state.sessions.push(Slot {
            id,
            connection,
            token: request.token,
            session,
            stage: Stage::Opening,
            picture: None,
            band: Vec::new(),
            lost: false,
            opened_at: self.platform.now_ns(),
            period: Counts::default(),
            total: Counts::default(),
        });
        Ok(state.sessions.len() - 1)
    }

    fn encode(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &EncoderEncodeRequest,
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        let index = slot_of(state, connection, request.encoder)?;
        let started = self.platform.now_ns();
        let slot = &mut state.sessions[index];
        slot.period.calls += 1;
        if slot.lost {
            return Err(lost());
        }
        // A call made again for the frame it already answered, or refused.
        match &slot.stage {
            Stage::Opening => {
                return Err(Fault::new(
                    Status::Invalid,
                    "the session is still being made",
                ));
            }
            Stage::Done { frame, encoded } if *frame == request.frame => {
                return answer(encoded, *frame, slot.picture, request, response, caller);
            }
            Stage::Refused { frame, fault } if *frame == request.frame => return Err(*fault),
            _ => {}
        }
        // A frame still in flight completes before another starts; its
        // output is dropped when a later frame asks.
        if matches!(slot.stage, Stage::InFlight { frame } if frame != request.frame) {
            if let Err(fault) = self.poll(slot, started) {
                if fault.status == Status::Lost {
                    return Err(self.lost_slot(state, index));
                }
                return Err(fault);
            }
            if !matches!(slot.stage, Stage::Done { .. } | Stage::Refused { .. }) {
                slot.period.pending += 1;
                return Err(GOES_ON);
            }
        }
        if !matches!(
            slot.stage,
            Stage::Storing { frame, .. } | Stage::InFlight { frame } if frame == request.frame
        ) {
            slot.picture = Some(self.start_frame(slot, request, started)?);
            slot.stage = Stage::Storing {
                frame: request.frame,
                plane: Plane::Luma,
                row: 0,
            };
        }
        if let Stage::Storing { .. } = slot.stage {
            let stored = self.store(slot, request.frame, started, caller);
            let picture = slot
                .picture
                .as_mut()
                .expect("a frame being stored has its picture");
            picture.convert_ns += self.platform.now_ns().saturating_sub(started);
            stored?;
            let options = Options {
                force_idr: picture.force_idr,
            };
            if let Err(error) = slot.session.submit(Frame::Stored, options) {
                return Err(self.failed(state, index, request.frame, error));
            }
            slot.stage = Stage::InFlight {
                frame: request.frame,
            };
        }
        let slot = &mut state.sessions[index];
        if let Err(fault) = self.poll(slot, self.platform.now_ns()) {
            if fault.status == Status::Lost {
                return Err(self.lost_slot(state, index));
            }
            return Err(fault);
        }
        match &slot.stage {
            Stage::Done { frame, encoded } => {
                answer(encoded, *frame, slot.picture, request, response, caller)
            }
            Stage::Refused { fault, .. } => Err(*fault),
            _ => {
                slot.period.pending += 1;
                Err(GOES_ON)
            }
        }
    }

    /// The picture of a new frame, checked, with the session's rate set to
    /// the frame's bitrate.
    fn start_frame(
        &self,
        slot: &mut Slot<P::Node>,
        request: &EncoderEncodeRequest,
        began: u64,
    ) -> Result<Picture, Fault> {
        let input = EncoderInput::from_raw(request.input)
            .ok_or(Fault::malformed("encoder input").value(request.input))?;
        if request.flags & !ENCODE_FORCE_IDR != 0 {
            return Err(Fault::malformed("encode flags").bits(request.flags));
        }
        let geometry = *slot.session.geometry();
        let (width, height) = (geometry.width as u64, geometry.height as u64);
        let (luma_pitch, chroma_pitch) = (
            u64::from(request.luma_pitch),
            u64::from(request.chroma_pitch),
        );
        // Each span holds its rows, the last only as wide as the picture.
        let covers = |span: Span, pitch: u64, rows: u64, row_bytes: u64| {
            pitch >= row_bytes && span.length >= (rows - 1) * pitch + row_bytes
        };
        let fits = match input {
            EncoderInput::Nv12 => {
                covers(request.luma, luma_pitch, height, width)
                    && covers(request.chroma, chroma_pitch, height / 2, width)
            }
            EncoderInput::Bgra => covers(request.luma, luma_pitch, height, 4 * width),
        };
        if !fits {
            return Err(Fault::new(
                Status::Invalid,
                "the picture's planes are smaller than the session's",
            ));
        }
        if request.bitrate != 0
            && let Rate::Cbr { bitrate } = slot.session.config().rate
            && bitrate != request.bitrate
        {
            slot.session
                .set_rate(Rate::Cbr {
                    bitrate: request.bitrate,
                })
                .map_err(refusal)?;
            self.log(format_args!(
                "{} bitrate {} kb/s from frame {}",
                slot.id,
                request.bitrate / 1000,
                request.frame
            ));
        }
        Ok(Picture {
            input,
            luma: request.luma,
            chroma: request.chroma,
            luma_pitch: luma_pitch as usize,
            chroma_pitch: chroma_pitch as usize,
            force_idr: request.flags & ENCODE_FORCE_IDR != 0,
            began,
            convert_ns: 0,
        })
    }

    /// Stores bands of the frame's rows until every row is stored, or the
    /// call's budget is spent (`Pending`), or a band is out of the caller's
    /// reach (`Unmapped`); what was stored stays.
    fn store(
        &self,
        slot: &mut Slot<P::Node>,
        frame: u64,
        started: u64,
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        let picture = slot.picture.expect("a frame being stored has its picture");
        let geometry = *slot.session.geometry();
        let surface = slot.session.input_surface().map_err(refusal)?;
        let width = geometry.width as usize;
        let mut first_band = true;
        loop {
            let Stage::Storing { plane, row, .. } = slot.stage else {
                return Ok(());
            };
            let (rows, span, pitch, row_bytes) = match (picture.input, plane) {
                (EncoderInput::Nv12, Plane::Luma) => (
                    geometry.height as usize,
                    picture.luma,
                    picture.luma_pitch,
                    width,
                ),
                (EncoderInput::Nv12, Plane::Chroma) => (
                    geometry.height as usize / 2,
                    picture.chroma,
                    picture.chroma_pitch,
                    width,
                ),
                (EncoderInput::Bgra, _) => (
                    geometry.height as usize,
                    picture.luma,
                    picture.luma_pitch,
                    4 * width,
                ),
            };
            if !first_band && self.platform.now_ns().saturating_sub(started) >= STORE_BUDGET_NS {
                return Err(GOES_ON);
            }
            first_band = false;
            let count = BAND_ROWS.min(rows - row);
            let bytes = (count - 1) * pitch + row_bytes;
            slot.band.resize(bytes, 0);
            caller
                .read(span.address + (row * pitch) as u64, &mut slot.band)
                .map_err(fault)?;
            // SAFETY: the input surface is the session's until it submits
            // this frame; the band holds `count` rows `pitch` apart, the
            // last as wide as the picture, as `start_frame` checked.
            unsafe {
                match picture.input {
                    EncoderInput::Nv12 => surface::store_rows(
                        surface.as_ptr(),
                        &geometry,
                        plane,
                        row,
                        count,
                        &slot.band,
                        pitch,
                    ),
                    EncoderInput::Bgra => surface::store_bgra_rows(
                        surface.as_ptr(),
                        &geometry,
                        row,
                        count,
                        &Rgb {
                            pixels: &slot.band,
                            pitch,
                            order: Order::Bgra,
                        },
                    ),
                }
            }
            let row = row + count;
            slot.stage = match (picture.input, plane) {
                _ if row < rows => Stage::Storing { frame, plane, row },
                (EncoderInput::Nv12, Plane::Luma) => Stage::Storing {
                    frame,
                    plane: Plane::Chroma,
                    row: 0,
                },
                _ => return Ok(()),
            };
        }
    }

    /// Looks at the frame in flight until it completes or the poll budget
    /// is spent; a completed frame is `Done` or `Refused`, and a timeout is
    /// `Lost`.
    fn poll(&self, slot: &mut Slot<P::Node>, started: u64) -> Result<(), Fault> {
        let Stage::InFlight { frame } = slot.stage else {
            return Ok(());
        };
        // One ask a call lets the host's Linux run and tells a busy
        // channel from an idle one.
        slot.session.ask_fence();
        loop {
            match slot.session.poll(false) {
                Ok(Some(encoded)) => {
                    let picture = slot.picture.expect("a frame in flight has its picture");
                    let total = self.platform.now_ns().saturating_sub(picture.began);
                    slot.period.frame(&encoded, picture.convert_ns, total);
                    slot.stage = Stage::Done { frame, encoded };
                    return Ok(());
                }
                Ok(None) => {
                    if self.platform.now_ns().saturating_sub(started) >= POLL_BUDGET_NS {
                        return Ok(());
                    }
                    self.platform.pause();
                }
                Err(session::Error::Engine { why, status }) => {
                    slot.period.errors += 1;
                    if slot.total.errors + slot.period.errors <= 4 {
                        self.log(format_args!(
                            "{} frame {frame} dropped: {why}: error_status {} \
                             ucode_error_status {:#x}; the next frame is an IDR",
                            slot.id, status.error_status, status.ucode_error_status
                        ));
                    }
                    let fault = Fault::new(Status::Dropped, why);
                    slot.stage = Stage::Refused { frame, fault };
                    return Ok(());
                }
                Err(session::Error::Timeout { value, job_done }) => {
                    slot.period.errors += 1;
                    self.log(format_args!(
                        "{} LOST at frame {frame}: no completion in {} ms, the semaphore held \
                         {value:#x}, nouveau's fence {}",
                        slot.id,
                        FRAME_BOUND_NS / 1_000_000,
                        if job_done {
                            "signalled: the engine wrote nothing"
                        } else {
                            "pending: the channel is busy"
                        }
                    ));
                    return Err(lost());
                }
                Err(error) => return Err(refusal(error)),
            }
        }
    }

    /// Marks slot `index` lost: it takes no more frames, and a GPU whose
    /// channel stayed busy opens no more sessions.
    fn lost_slot(&self, state: &mut State<P::Node>, index: usize) -> Fault {
        let slot = &mut state.sessions[index];
        slot.lost = true;
        if slot.session.dead() == Some(true) {
            state.refused = Some("an encoder session hung this boot");
        }
        lost()
    }

    /// A submission nouveau refused: the frame is refused, and the session
    /// with it if it can take no more.
    fn failed(
        &self,
        state: &mut State<P::Node>,
        index: usize,
        frame: u64,
        error: session::Error,
    ) -> Fault {
        let slot = &mut state.sessions[index];
        slot.period.errors += 1;
        self.log(format_args!(
            "{} frame {frame} not submitted: {error:?}",
            slot.id
        ));
        let fault = refusal(error);
        slot.stage = Stage::Refused { frame, fault };
        if matches!(error, session::Error::Dead | session::Error::Refused { .. }) {
            return self.lost_slot(state, index);
        }
        fault
    }

    fn close(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        encoder: u64,
    ) -> Result<(), Fault> {
        let index = slot_of(state, connection, encoder)?;
        self.close_slot(state, index, "the frontend closed it");
        Ok(())
    }

    fn call(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        operation: Operation,
        request: &[u8],
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        match operation {
            Operation::EncoderCapabilities => Self::capabilities(state, response),
            Operation::EncoderOpen => {
                let request: EncoderOpenRequest = wire::read(request)?;
                self.open(state, connection, &request, response, caller)
            }
            Operation::EncoderEncode => {
                let request: EncoderEncodeRequest = wire::read(request)?;
                self.encode(state, connection, &request, response, caller)
            }
            Operation::EncoderClose => {
                let Handle { handle } = wire::read(request)?;
                self.close(state, connection, handle)?;
                wire::write(&wire::Empty {}, response)
            }
            _ => Err(
                Fault::new(Status::UnknownOperation, "not an encoder operation")
                    .value(operation.raw()),
            ),
        }
    }
}

/// The slot of session `encoder`, which `connection` must hold.
fn slot_of<N: Node>(
    state: &State<N>,
    connection: NonZeroU64,
    encoder: u64,
) -> Result<usize, Fault> {
    state
        .sessions
        .iter()
        .position(|slot| slot.id == encoder && slot.connection == connection)
        .ok_or(Fault::STALE)
}

fn lost() -> Fault {
    Fault::new(Status::Lost, "the encoder session stopped answering")
}

/// The fault a session's refusal answers.
fn refusal(error: session::Error) -> Fault {
    match error {
        session::Error::NoEncoder => Fault::new(
            Status::Unsupported,
            "the GPU has no encoder this port drives",
        ),
        session::Error::Config(why) | session::Error::Input(why) => {
            Fault::new(Status::Invalid, why)
        }
        session::Error::Refused { step, errno } => {
            Fault::new(Status::Unsupported, step).signed(errno)
        }
        session::Error::TooLarge { name, .. } | session::Error::Map { name } => {
            Fault::new(Status::OutOfMemory, name)
        }
        session::Error::Engine { why, .. } => Fault::new(Status::Dropped, why),
        session::Error::Timeout { .. } | session::Error::Dead => lost(),
    }
}

/// Answers frame `frame`'s `encoded`: its bytes to `out` when they fit.
fn answer(
    encoded: &Encoded,
    frame: u64,
    picture: Option<Picture>,
    request: &EncoderEncodeRequest,
    response: &mut [u8],
    caller: &mut dyn CallerMemory,
) -> Result<(), Fault> {
    let length = encoded.bytes.len() as u64;
    if length <= request.out.length {
        caller
            .write(request.out.address, &encoded.bytes)
            .map_err(fault)?;
    }
    let micros = |ns: u64| u32::try_from(ns / 1000).unwrap_or(u32::MAX);
    wire::write(
        &EncoderEncodeResponse {
            bytes: length,
            frame,
            idr: u32::from(encoded.idr),
            picture_index: encoded.picture,
            error_status: encoded.status.error_status,
            average_qp: encoded.status.average_qp,
            convert_us: micros(picture.map_or(0, |picture| picture.convert_ns)),
            gpu_us: micros(encoded.timing.wait_ns),
        },
        response,
    )
}

impl<P: EncoderPlatform> GateEncoders for Encoders<P> {
    fn serve(
        &self,
        connection: NonZeroU64,
        operation: Operation,
        request: &[u8],
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        let Some(mut held) = self.try_hold() else {
            return Err(BUSY);
        };
        let state = held.state();
        let mut result = Err(Fault::new(Status::Invalid, "the platform ran nothing"));
        self.platform.run(&mut || {
            result = self.call(state, connection, operation, request, response, caller);
        });
        result
    }

    fn disconnect(&self, connection: NonZeroU64) {
        let mut held = self.hold();
        let state = held.state();
        self.platform.run(&mut || {
            while let Some(index) = state
                .sessions
                .iter()
                .position(|slot| slot.connection == connection)
            {
                self.close_slot(state, index, "its connection closed");
            }
        });
    }

    fn report(&self) {
        let Some(mut held) = self.try_hold() else {
            return;
        };
        let state = held.state();
        for slot in &mut state.sessions {
            if slot.period.calls == 0 {
                continue;
            }
            let period = core::mem::take(&mut slot.period);
            slot.total.add(&period);
            let line = alloc::format!(
                "{} {}: {period}{}",
                slot.id,
                slot.describe(),
                if slot.lost { " LOST" } else { "" }
            );
            self.log(format_args!("{line}"));
        }
    }

    fn close_all(&self) {
        let mut held = self.hold();
        let state = held.state();
        self.platform.run(&mut || {
            while !state.sessions.is_empty() {
                self.close_slot(state, 0, "the PC stops");
            }
        });
    }
}

const _: () = assert!(size_of::<EncoderEncodeRequest>() <= RECORD_BYTES);
const _: () = assert!(size_of::<EncoderOpenRequest>() <= RECORD_BYTES);
const _: () = assert!(size_of::<EncoderEncodeResponse>() <= RECORD_BYTES);
