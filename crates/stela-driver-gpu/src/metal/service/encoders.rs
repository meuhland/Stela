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
//! **No processor stalls.** A call works at most about [`CALL_BUDGET_NS`]
//! (an open, [`STORE_BUDGET_NS`]), then answers `Pending`, and the frontend
//! calls again; what the call did stays done.
//!
//! - `EncoderOpen` makes one piece of the session at a time
//!   ([`Session::bring_up_step`]): the channel, then each buffer, every one
//!   a host kernel call or a few.
//! - `EncoderEncode` reads the picture's rows from the caller's memory,
//!   [`surface::BAND_ROWS`] at a time with both planes, and stores each band
//!   into the session's block-linear input surface as whole block rows in
//!   address order ([`surface::store_nv12_band`], or
//!   [`surface::store_bgra_band`], which converts BGRA on the way):
//!   pitch-linear pictures cannot reach the engine any other way. Right
//!   after the last band it submits the frame in the same call, has the
//!   host's Linux push the job at once ([`EncoderPlatform::kick`]), and
//!   looks at the frame's semaphore for what is left of the call's budget,
//!   when the engine's time (the session's measure of it) says the frame
//!   completes within it. A call made again for the same frame goes on
//!   where the last stopped; one for a later frame starts it.
//! - **Waiting.** A call answered `Pending` writes its response's
//!   `retry_us`: when the frame should be done, from the engine's measured
//!   time, so the frontend sleeps once a frame instead of polling. Only a
//!   late frame asks nouveau's fence (`SYNCOBJ_WAIT`, a call the boot CPU's
//!   Linux serves); the semaphore the stream releases is read directly.
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
//! **The real-time profile** ([`Config::realtime`]): every session opens
//! with it unless the platform says not ([`EncoderPlatform::realtime_profile`],
//! `--no-nvenc-realtime-profile`). StelaVT is the only frontend, and it is a
//! real-time encoder to every client: it reports `RealTime` true and
//! `AllowFrameReordering` false whatever the client set, codes one frame in
//! and one out, and passes neither property to the host. Parsec, the client
//! this serves, sets both that way. A frame the engine refuses turns the
//! session's profile off from its next frame (an IDR anyway), and a session
//! lost with it on keeps later sessions of the GPU from opening with it.
//!
//! **Report.** Each session logs `NVENC SESSION` lines: when it opens (with
//! its profile and rate control), at each platform report while it encodes
//! ([`GateEncoders::report`]), when it is lost and when it closes. The
//! report and close lines carry the frames' sizes, P and IDR apart, and the
//! engine's QP. The open and report lines end with the GPU's P-state and
//! PCIe link when the platform knows them ([`EncoderPlatform::gpu_state`]):
//! the engine's time follows the P-state's clocks, which GSP-RM's own policy
//! picks.
use super::gate::fault;
#[cfg(test)]
use super::pace::RETRY_LATE_US;
use super::pace::{Pace, RETRY_AT_ONCE_US};
use super::wire::{
    self, ENCODE_FORCE_IDR, ENCODER_OPEN_FULL_RANGE, EncoderCapabilitiesResponse, EncoderCodec,
    EncoderEncodeRequest, EncoderEncodeResponse, EncoderInput, EncoderOpenRequest,
    EncoderOpenResponse, EncoderRate, Fault, GateCall, Handle, Operation, Span, Status,
};
use crate::nvenc::{
    Codec, Config, Node, Rate,
    layout::MAX_SIDE,
    session::{self, Encoded, Frame, Options, Session},
    setup::RateControl,
    surface::{self, Order, Rgb, Staging},
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
/// About how long one call makes a session, at least one piece, before it
/// answers `Pending`: the service's own budgets are 1 to 2 ms
/// (`WAIT_BOUND_MILLIS`, `LIBRARY_MICROS`).
pub const STORE_BUDGET_NS: u64 = 1_500_000;
/// About how long one encode call stores, submits and waits for its frame
/// before it answers `Pending`, at least one band: a frame then takes one
/// call or two, where the frontend's sleep between calls costs it more
/// than this host time does. It stays under the gate's 5 ms exit target
/// (`GPU EXIT target_us`), which XNU's view of a held processor sets.
pub const CALL_BUDGET_NS: u64 = 4_500_000;
/// How long a frame may take from its submission: past this the session is
/// `Lost`.
pub const FRAME_BOUND_NS: u64 = 1_000_000_000;
/// Past the engine's expected time by this much a frame is late, and each
/// call asks nouveau's fence once, which also lets the host's Linux run.
pub const LATE_NS: u64 = 2_000_000;
/// The engine's time for a 1080p frame before any is measured: the boot
/// self-test's (2026-11-17), H.264 and HEVC. HEVC's is the GPU's low
/// P-state's (1.85 ms at P0): [`Pace`] measures the session's own.
const H264_1080P_NS: u64 = 2_300_000;
const HEVC_1080P_NS: u64 = 5_800_000;

/// The host's heap, in bytes, as [`EncoderPlatform::heap`] reads it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostHeap {
    pub total: u64,
    /// Allocated and not freed.
    pub used: u64,
    /// The largest free extent.
    pub largest: u64,
}

impl HostHeap {
    /// The bytes not allocated: free extents, holes between live blocks
    /// included.
    pub fn free(&self) -> u64 {
        self.total.saturating_sub(self.used)
    }
}

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
    /// Has the host's Linux run its ready work now, on this processor, for
    /// a moment at most: nouveau's `EXEC` only queues the job, and its
    /// scheduler's work item pushes it to the channel. Whether Linux ran
    /// here; when it cannot (another processor runs it, or this is the boot
    /// CPU), the encoders ask nouveau's fence instead, a posted call.
    fn kick(&self) -> bool {
        false
    }
    /// The GPU's performance state now, as GSP-RM last reported it
    /// (`pstate=P8 pcie=gen1x16`), which the session lines end with: the
    /// engine's time follows the P-state's clocks. `None` leaves it out.
    fn gpu_state(&self) -> Option<alloc::string::String> {
        None
    }
    /// The host's heap now, which backs nouveau's system memory (GART
    /// buffers, page tables) as well as the engine's own: the decoders
    /// admit sessions by it. `None` when the platform does not say.
    fn heap(&self) -> Option<HostHeap> {
        None
    }
    /// Whether sessions open with the real-time profile
    /// ([`Config::realtime`]): yes unless `--no-nvenc-realtime-profile`.
    fn realtime_profile(&self) -> bool {
        true
    }
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
    operation & 0xfff0 == 0x0a00
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
    let served = encoders.serve(connection, operation, request, response, caller);
    // An encode answered `Pending` still answers its response: its
    // `retry_us` says when to call again (zero from a call that found the
    // encoders busy).
    let hints = matches!(served, Err(fault) if fault.status == Status::Pending)
        && operation == Operation::EncoderEncode;
    if served.is_ok() || hints {
        caller
            .write(call.response.address, response)
            .map_err(fault)?;
    }
    served
}

/// Where a session's current frame is.
enum Stage {
    /// Its pieces are being made, for the open of `token`.
    Opening,
    /// No frame, or the last one answered.
    Idle,
    /// Frame `frame`'s rows are being stored, both planes' a band at a
    /// time, from picture row `row` on.
    Storing { frame: u64, row: usize },
    /// Frame `frame` was submitted at `submitted`, and last seen not yet
    /// complete at `looked`, by the platform's clock.
    InFlight {
        frame: u64,
        submitted: u64,
        looked: u64,
    },
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
    /// When its first call came, the time its calls spent storing it, and
    /// its calls so far.
    began: u64,
    convert_ns: u64,
    calls: u32,
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
    /// The engine's time where a look saw it closely
    /// ([`super::pace::ACCURATE_NS`]).
    engine_ns: u64,
    engine_max: u64,
    engine_samples: u64,
    /// nouveau's fence asked (posted to Linux), and Linux run in place to
    /// push a job ([`EncoderPlatform::kick`]).
    asks: u64,
    kicks: u64,
    /// The largest frame, the P frames' bytes and largest, the IDRs' bytes
    /// (parameter sets included) and largest.
    frame_max: u64,
    p_bytes: u64,
    p_max: u64,
    idr_bytes: u64,
    idr_max: u64,
    /// The engine's average QP of each frame, summed, and the highest.
    qp_sum: u64,
    qp_max: u64,
    /// Frames of more than twice a CBR frame's share of the bitrate.
    over: u64,
}

impl Counts {
    fn engine(&mut self, ns: u64) {
        self.engine_ns += ns;
        self.engine_max = self.engine_max.max(ns);
        self.engine_samples += 1;
    }

    /// A coded frame; `share` is a CBR frame's share of the bitrate, in
    /// bytes.
    fn frame(&mut self, encoded: &Encoded, convert_ns: u64, total_ns: u64, share: Option<u64>) {
        let bytes = encoded.bytes.len() as u64;
        self.frames += 1;
        self.bytes += bytes;
        self.idrs += u64::from(encoded.idr);
        self.frame_max = self.frame_max.max(bytes);
        if encoded.idr {
            self.idr_bytes += bytes;
            self.idr_max = self.idr_max.max(bytes);
        } else {
            self.p_bytes += bytes;
            self.p_max = self.p_max.max(bytes);
        }
        let qp = u64::from(encoded.status.average_qp);
        self.qp_sum += qp;
        self.qp_max = self.qp_max.max(qp);
        self.over += u64::from(share.is_some_and(|share| bytes > 2 * share));
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
        self.engine_ns += other.engine_ns;
        self.engine_max = self.engine_max.max(other.engine_max);
        self.engine_samples += other.engine_samples;
        self.asks += other.asks;
        self.kicks += other.kicks;
        self.frame_max = self.frame_max.max(other.frame_max);
        self.p_bytes += other.p_bytes;
        self.p_max = self.p_max.max(other.p_max);
        self.idr_bytes += other.idr_bytes;
        self.idr_max = self.idr_max.max(other.idr_max);
        self.qp_sum += other.qp_sum;
        self.qp_max = self.qp_max.max(other.qp_max);
        self.over += other.over;
    }
}

/// `frames=… bytes=… idr=… errors=… gpu_us=avg/max convert_us=avg/max
/// total_us=avg/max calls=… pending=… engine_us=avg/max/samples asks=…
/// kicks=… frame_bytes=avg/max p_bytes=avg/max idr_bytes=avg/max
/// qp=avg/max over_2x=…`: the sizes in bytes, the QP the engine's statuses
/// averaged, and frames past twice a CBR frame's share of the bitrate.
impl fmt::Display for Counts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let average = |total: u64| total.checked_div(self.frames).unwrap_or(0) / 1000;
        let idrs = self.idrs;
        let p_frames = self.frames - idrs.min(self.frames);
        write!(
            formatter,
            "frames={} bytes={} idr={} errors={} gpu_us={}/{} convert_us={}/{} total_us={}/{} \
             calls={} pending={} engine_us={}/{}/{} asks={} kicks={} frame_bytes={}/{} \
             p_bytes={}/{} idr_bytes={}/{} qp={}/{} over_2x={}",
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
            self.engine_ns.checked_div(self.engine_samples).unwrap_or(0) / 1000,
            self.engine_max / 1000,
            self.engine_samples,
            self.asks,
            self.kicks,
            self.bytes.checked_div(self.frames).unwrap_or(0),
            self.frame_max,
            self.p_bytes.checked_div(p_frames).unwrap_or(0),
            self.p_max,
            self.idr_bytes.checked_div(idrs).unwrap_or(0),
            self.idr_max,
            self.qp_sum.checked_div(self.frames).unwrap_or(0),
            self.qp_max,
            self.over,
        )
    }
}

/// The open line's profile: `profile realtime: cpb 167 kbit (1.0 frame),
/// bmin 2, 1 ref, …`, or `profile captured: …`.
fn profile(config: &Config, rc: &RateControl) -> alloc::string::String {
    let name = if config.realtime {
        "realtime"
    } else {
        "captured"
    };
    let rate = match config.rate {
        Rate::Cbr { .. } => {
            let tenths = rc.cpb_tenth_frames();
            alloc::format!(
                "cpb {} kbit ({}.{} frames), bmin {}",
                rc.cpb_bits / 1000,
                tenths / 10,
                tenths % 10,
                rc.bmin
            )
        }
        Rate::ConstQp(_) => alloc::string::String::from("constant QP"),
    };
    // The host picks every picture's type: an IDR first, when asked and
    // after an engine error, else P. `gop_length` only informs the rate
    // control.
    alloc::format!(
        "profile {name}: {rate}, {} ref{}, P frames only, {}, an IDR only when asked \
         (gop_length {})",
        rc.refs,
        if rc.refs == 1 { "" } else { "s" },
        if rc.lookahead {
            "lookahead"
        } else {
            "no lookahead"
        },
        rc.gop
    )
}

/// A CBR frame's share of the bitrate, in bytes.
fn share(config: &Config) -> Option<u64> {
    match config.rate {
        Rate::Cbr { bitrate } => Some(u64::from(bitrate) / 8 / u64::from(config.fps.max(1))),
        Rate::ConstQp(_) => None,
    }
}

struct Slot<N: Node> {
    id: u64,
    connection: NonZeroU64,
    token: u64,
    session: Session<N>,
    stage: Stage,
    picture: Option<Picture>,
    /// The rows a band brings from the caller, each plane's, and the
    /// stores' padded rows, kept between bands.
    luma_band: Vec<u8>,
    chroma_band: Vec<u8>,
    staging: Staging,
    /// When the engine should be done with a frame, and when the frontend
    /// should call again: the self-test's time at first, then what the
    /// session's looks measure ([`Pace`]).
    pace: Pace,
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
    /// Why sessions no longer open with the real-time profile: one was lost
    /// with it.
    realtime_off: Option<&'static str>,
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
                realtime_off: None,
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

    /// ` | gpu pstate=… pcie=…` from the platform, or nothing.
    fn gpu_state(&self) -> alloc::string::String {
        self.platform
            .gpu_state()
            .map(|state| alloc::format!(" | gpu {state}"))
            .unwrap_or_default()
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
                    let profile = profile(slot.session.config(), &slot.session.rate_control());
                    let why_not = match (slot.session.config().realtime, state.realtime_off) {
                        (false, Some(why)) => alloc::format!(" ({why})"),
                        (false, None) if !self.platform.realtime_profile() => {
                            alloc::string::String::from(" (--no-nvenc-realtime-profile)")
                        }
                        _ => alloc::string::String::new(),
                    };
                    let line = alloc::format!(
                        "{} open for connection {connection}: {}, coded {}x{}, {buffers} \
                         buffers in {} ms, {} bytes of parameter sets, {profile}{why_not}{}",
                        slot.id,
                        slot.describe(),
                        slot.session.geometry().coded_width,
                        slot.session.geometry().coded_height,
                        took / 1_000_000,
                        slot.session.headers().len(),
                        self.gpu_state()
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
            realtime: self.platform.realtime_profile() && state.realtime_off.is_none(),
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
            luma_band: Vec::new(),
            chroma_band: Vec::new(),
            staging: Staging::default(),
            pace: Pace::new(expected_engine_ns(codec, request.width, request.height)),
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
        slot.pace.arrived(started);
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
        if matches!(slot.stage, Stage::InFlight { frame, .. } if frame != request.frame) {
            if let Err(fault) = self.poll(slot, started) {
                if fault.status == Status::Lost {
                    return Err(self.lost_slot(state, index));
                }
                return Err(fault);
            }
            if !matches!(slot.stage, Stage::Done { .. } | Stage::Refused { .. }) {
                slot.period.pending += 1;
                return Err(self.goes_on(slot, response));
            }
        }
        if !matches!(
            slot.stage,
            Stage::Storing { frame, .. } | Stage::InFlight { frame, .. } if frame == request.frame
        ) {
            slot.picture = Some(self.start_frame(slot, request, started)?);
            slot.stage = Stage::Storing {
                frame: request.frame,
                row: 0,
            };
        }
        if let Some(picture) = slot.picture.as_mut() {
            picture.calls += 1;
        }
        if let Stage::Storing { .. } = slot.stage {
            let stored = self.store(slot, request.frame, started, caller);
            let picture = slot
                .picture
                .as_mut()
                .expect("a frame being stored has its picture");
            picture.convert_ns += self.platform.now_ns().saturating_sub(started);
            let options = Options {
                force_idr: picture.force_idr,
            };
            match stored {
                Ok(()) => {}
                // Out of budget: the frontend calls again at once.
                Err(fault) if fault == GOES_ON => {
                    write_retry(response, RETRY_AT_ONCE_US, picture.calls);
                    return Err(fault);
                }
                Err(fault) => return Err(fault),
            }
            // Submitted right after the last band, in the same call.
            if let Err(error) = slot.session.submit(Frame::Stored, options) {
                return Err(self.failed(state, index, request.frame, error));
            }
            let submitted = self.platform.now_ns();
            slot.stage = Stage::InFlight {
                frame: request.frame,
                submitted,
                looked: submitted,
            };
            // nouveau's EXEC queued the job; its scheduler's work item
            // pushes it to the channel when Linux next runs. Run it now.
            if self.platform.kick() {
                slot.period.kicks += 1;
            } else {
                slot.session.ask_fence();
                slot.period.asks += 1;
            }
        } else if self.platform.kick() {
            // A call after a sleep: whatever Linux has ready, the job's
            // push among it, runs before the look.
            slot.period.kicks += 1;
        }
        let slot = &mut state.sessions[index];
        if let Err(fault) = self.poll(slot, started) {
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
                Err(self.goes_on(slot, response))
            }
        }
    }

    /// `Pending` for a frame in flight, its response's `retry_us` saying
    /// when the engine should have completed it, less the frontend's
    /// wake-up ([`Pace::retry_us`]).
    fn goes_on(&self, slot: &mut Slot<P::Node>, response: &mut [u8]) -> Fault {
        let calls = slot.picture.map_or(0, |picture| picture.calls);
        let now = self.platform.now_ns();
        let retry_us = match slot.stage {
            Stage::InFlight { submitted, .. } => slot.pace.retry_us(now, submitted),
            _ => {
                slot.pace.asked(now, RETRY_AT_ONCE_US);
                RETRY_AT_ONCE_US
            }
        };
        write_retry(response, retry_us, calls);
        GOES_ON
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
                "{} bitrate {} kb/s from frame {}, cpb {} kbit",
                slot.id,
                request.bitrate / 1000,
                request.frame,
                slot.session.rate_control().cpb_bits / 1000
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
            calls: 0,
        })
    }

    /// Stores bands of the frame's rows, both planes' [`surface::BAND_ROWS`]
    /// at a time, until every row is stored, or the call's budget is spent
    /// (`Pending`), or a band is out of the caller's reach (`Unmapped`);
    /// what was stored stays.
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
        let (width, height) = (geometry.width as usize, geometry.height as usize);
        let mut first_band = true;
        loop {
            let Stage::Storing { row, .. } = slot.stage else {
                return Ok(());
            };
            if row >= height {
                return Ok(());
            }
            if !first_band && self.platform.now_ns().saturating_sub(started) >= CALL_BUDGET_NS {
                return Err(GOES_ON);
            }
            first_band = false;
            let count = surface::BAND_ROWS.min(height - row);
            // Each plane's rows of the band: the last only as wide as the
            // picture, as `start_frame` checked the spans hold.
            let read = |band: &mut Vec<u8>,
                        span: Span,
                        pitch: usize,
                        first: usize,
                        rows: usize,
                        row_bytes: usize,
                        caller: &mut dyn CallerMemory| {
                band.resize((rows - 1) * pitch + row_bytes, 0);
                caller
                    .read(span.address + (first * pitch) as u64, band)
                    .map_err(fault)
            };
            match picture.input {
                EncoderInput::Nv12 => {
                    read(
                        &mut slot.luma_band,
                        picture.luma,
                        picture.luma_pitch,
                        row,
                        count,
                        width,
                        caller,
                    )?;
                    read(
                        &mut slot.chroma_band,
                        picture.chroma,
                        picture.chroma_pitch,
                        row / 2,
                        count / 2,
                        width,
                        caller,
                    )?;
                    // SAFETY: the input surface is the session's until it
                    // submits this frame; `row` is a band's first row, and
                    // the bands hold its rows of each plane.
                    unsafe {
                        surface::store_nv12_band(
                            surface.as_ptr(),
                            &geometry,
                            row,
                            &slot.luma_band,
                            picture.luma_pitch,
                            &slot.chroma_band,
                            picture.chroma_pitch,
                            &mut slot.staging,
                        );
                    }
                }
                EncoderInput::Bgra => {
                    read(
                        &mut slot.luma_band,
                        picture.luma,
                        picture.luma_pitch,
                        row,
                        count,
                        4 * width,
                        caller,
                    )?;
                    // SAFETY: as above; the band holds its rows.
                    unsafe {
                        surface::store_bgra_band(
                            surface.as_ptr(),
                            &geometry,
                            row,
                            &Rgb {
                                pixels: &slot.luma_band,
                                pitch: picture.luma_pitch,
                                order: Order::Bgra,
                            },
                            &mut slot.staging,
                        );
                    }
                }
            }
            slot.stage = Stage::Storing {
                frame,
                row: row + count,
            };
        }
    }

    /// Looks at the frame in flight until it completes, or until the
    /// call's budget from `started` is spent, or at once when the engine's
    /// expected time ends past that; a completed frame is `Done` or
    /// `Refused`, and a timeout is `Lost`.
    fn poll(&self, slot: &mut Slot<P::Node>, started: u64) -> Result<(), Fault> {
        let Stage::InFlight {
            frame,
            submitted,
            mut looked,
        } = slot.stage
        else {
            return Ok(());
        };
        let budget_end = started.saturating_add(CALL_BUDGET_NS);
        let mut asked = false;
        loop {
            match slot.session.poll(false) {
                Ok(Some(encoded)) => {
                    let seen = self.platform.now_ns();
                    // Seen right after a look that missed it: the engine's
                    // own time, which the next frames' waits go by; else
                    // the looks bound it.
                    if let Some(engine) = slot.pace.done(submitted, looked, seen) {
                        slot.period.engine(engine);
                    }
                    let picture = slot.picture.expect("a frame in flight has its picture");
                    let total = seen.saturating_sub(picture.began);
                    let share = share(slot.session.config());
                    slot.period
                        .frame(&encoded, picture.convert_ns, total, share);
                    slot.stage = Stage::Done { frame, encoded };
                    return Ok(());
                }
                Ok(None) => {
                    let now = self.platform.now_ns();
                    looked = now;
                    slot.stage = Stage::InFlight {
                        frame,
                        submitted,
                        looked,
                    };
                    // A late frame: ask nouveau's fence once a call, which
                    // also lets the host's Linux run.
                    if !asked && now > slot.pace.expected(submitted).saturating_add(LATE_NS) {
                        slot.session.ask_fence();
                        slot.period.asks += 1;
                        asked = true;
                    }
                    if !slot.pace.keep_looking(now, submitted, budget_end) {
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
                    // The profile's rate control is the suspect: the next
                    // frame, an IDR, goes back to NVIDIA's captured one.
                    if slot.session.config().realtime && slot.session.set_realtime(false).is_ok() {
                        self.log(format_args!(
                            "{} real-time profile off from frame {}: the engine refused frame \
                             {frame}; {}",
                            slot.id,
                            frame + 1,
                            profile(slot.session.config(), &slot.session.rate_control())
                        ));
                    }
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
        if slot.session.config().realtime && state.realtime_off.is_none() {
            state.realtime_off = Some("a session was lost with the real-time profile this boot");
            self.log(format_args!(
                "{} lost with the real-time profile: later sessions open without it",
                slot.id
            ));
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
            retry_us: 0,
            calls: picture.map_or(0, |picture| picture.calls),
        },
        response,
    )
}

/// The response of an encode answered `Pending`: when to call again, and
/// the frame's calls so far.
fn write_retry(response: &mut [u8], retry_us: u32, calls: u32) {
    let reply = EncoderEncodeResponse {
        retry_us,
        calls,
        ..EncoderEncodeResponse::default()
    };
    // The response record is the operation's own size.
    let _ = wire::write(&reply, response);
}

/// The engine's time for a frame of `codec` before any is measured: the
/// self-test's 1080p time, scaled by the picture's area, at least 0.5 ms.
fn expected_engine_ns(codec: Codec, width: u32, height: u32) -> u64 {
    let at_1080p = match codec {
        Codec::H264 => H264_1080P_NS,
        Codec::Hevc => HEVC_1080P_NS,
    };
    (at_1080p * u64::from(width) * u64::from(height) / (1920 * 1080)).max(500_000)
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
        let mut gpu = None;
        for slot in &mut state.sessions {
            if slot.period.calls == 0 {
                continue;
            }
            let period = core::mem::take(&mut slot.period);
            slot.total.add(&period);
            let gpu = gpu.get_or_insert_with(|| self.gpu_state());
            let line = alloc::format!(
                "{} {}: {period}{}{gpu}",
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
