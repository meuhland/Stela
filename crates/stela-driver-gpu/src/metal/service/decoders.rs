//! The GPU's video decoder through the Metal service's gate: NVDEC sessions
//! ([`crate::nvdec::decoder`]) that StelaVD opens with a stream's `avcC` or
//! `hvcC`, feeds sample by sample and closes, the twin of the encoders'
//! ([`super::encoders`]).
//!
//! As the encoders, the decoders are no part of the Metal device: the
//! shared service hands every decoder operation (`0x0a10..=0x0a1f`) to the
//! GPU's [`GateDecoders`] before it looks for a turn. They have a lock of
//! their own, which a call only tries (a call that finds it held is
//! answered `Pending`), and run on the platform's stack
//! ([`EncoderPlatform::run`]: the encoders' platform serves them too).
//!
//! **No processor stalls.** A call works at most about [`CALL_BUDGET_NS`],
//! then answers `Pending` with `retry_us`, and the frontend calls again
//! with the same frame; what the call did stays done.
//!
//! - `DecoderOpen` makes one buffer a call piece ([`Decoder::bring_up_step`]).
//! - `DecoderDecode` first shows the pictures waiting (each blitted by the
//!   copy engine from its block-linear surface to pitch-linear NV12 or P010
//!   in the session's staging buffer, then copied by the host's CPU into
//!   the caller's destination planes, a band of rows at a time), then takes
//!   the sample: parsed on the host, submitted to NVDEC, and looked at until
//!   the engine releases it, when the pictures it shows join the queue and
//!   are shown in the same call. This is the stream's caller contract: no
//!   sample is submitted while a picture waits to be shown, so no surface
//!   is decoded into before its picture is copied out. A call whose
//!   destinations run out first answers `taken` 0 and the frontend calls
//!   again, with more, for the same sample.
//! - **Waits are bounded.** A picture the engine has not released within
//!   [`FRAME_BOUND_NS`], or a copy within the copier's 500 ms, makes the
//!   session `Lost`: it takes no more samples, and the frontend decodes in
//!   software. A channel whose job is still pending keeps its file open,
//!   and no session opens on the GPU again this boot.
//!
//! **Pacing** ([`super::pace`], as the encoders'): a picture expected past
//! the call's budget is answered `Pending` at once with the wait left less
//! the frontend's wake-up, and the engine's expected time is what the looks
//! measured, never the frontend's sleeps.
//!
//! **Limits and cleanup.** A GPU serves at most [`MAX_SESSIONS`] sessions at
//! once, admitted while their video memory together stays within
//! [`VRAM_BUDGET`] (each at most `decoder::VRAM_BUDGET`). A connection's
//! close closes its sessions ([`GateDecoders::disconnect`]), as the PC's
//! stop closes every one.
//!
//! **Eviction.** VideoToolbox may leave a session open and never call it
//! again (the Aerial wallpaper's AVPlayerLooper leaves one behind each loop,
//! 0.43 GiB of video memory each). An open that does not fit closes the
//! sessions idle at least [`EVICT_IDLE_NS`], least recently called first,
//! when that makes room (else nothing is closed and the open is refused,
//! listing the sessions open and how long each has been idle); and a
//! session idle [`RELEASE_IDLE_NS`] is closed at the next decoder call
//! (of another session) to give its video memory back. An evicted
//! session's calls answer `Evicted`, which StelaVD takes as "open a new
//! session at the next sync sample", and its close answers `Ok`.
//!
//! **Report.** Each session logs `NVDEC SESSION` lines when it opens, at
//! each platform report while it decodes, when it is lost and when it
//! closes: frames, outputs, errors, and the average and most of `gpu_us`
//! (the engine's decode), `ce_us` (the copy engine's readback), `copy_us`
//! (the CPU's copy into the caller) and `total_us` (a sample's calls).
use super::encoders::EncoderPlatform;
use super::gate::fault;
use super::pace::{Pace, RETRY_AT_ONCE_US};
use super::wire::{
    self, DECODE_FLUSH, DECODE_RESET, DECODER_OPEN_DECODE_ORDER, DECODER_PROFILE_H264_BASELINE,
    DECODER_PROFILE_H264_HIGH, DECODER_PROFILE_H264_MAIN, DECODER_PROFILE_HEVC_MAIN,
    DECODER_PROFILE_HEVC_MAIN10, DESTINATION_ERRORS, DESTINATION_WRITTEN,
    DecoderCapabilitiesResponse, DecoderDecodeRequest, DecoderDecodeResponse, DecoderFormat,
    DecoderOpenRequest, DecoderOpenResponse, DecoderPlanes, EncoderCodec, Fault, GateCall, Handle,
    Operation, Span, Status,
};
use crate::nvdec::decoder::{self, Decoder, Error, Format, MAX_SIDE, MIN_SIDE, Order};
use crate::nvdec::{Codec, session};
use crate::nvenc::Node;
use alloc::string::String;
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

/// The answer to a decoder operation where the host serves none: no NVIDIA
/// GPU with a decoder this port drives, `--no-nvdec`, or a GPU whose
/// decoder failed its probe or self-test.
pub const NONE: Fault = Fault::new(Status::Unsupported, "the host serves no video decoder");

/// Sessions one GPU decodes at once, at most: GA106 has one NVDEC, which
/// GSP-RM time-slices between channels. What admits a session is mostly
/// [`VRAM_BUDGET`]: a client may hold several (the Aerial wallpaper holds
/// two at once, which left none for any other client at 2).
pub const MAX_SESSIONS: usize = 8;
/// The video memory the GPU's decoder sessions take together, at most: half
/// the RTX 3050's 8 GiB, the rest left to macOS (NVK) and the encoders. A
/// 4K Main 10 session takes about 0.45 GiB ([`decoder::vram_bytes`]).
pub const VRAM_BUDGET: u64 = 4 << 30;
/// How long a session must have gone without a call before an open that
/// does not fit may evict it: 10 s. A playing session calls at least once a
/// frame (every 42 ms at 24 fps, 1 s even at 1 fps), and AVFoundation's
/// preroll of the next item calls within a second or two of opening, so
/// anything idle this long is paused or abandoned (Aerial's left-behind
/// sessions were idle 85-170 s). An evicted session that comes back only
/// loses the frames until its next sync sample.
pub const EVICT_IDLE_NS: u64 = 10_000_000_000;
/// How long a session may go without a call before it is evicted anyway,
/// to give its video memory back to macOS (NVK): 60 s, a paused movie's
/// price being frames dropped until the next sync sample when it resumes.
pub const RELEASE_IDLE_NS: u64 = 60_000_000_000;
/// The least time between two looks for sessions idle past
/// [`RELEASE_IDLE_NS`].
const SWEEP_NS: u64 = 1_000_000_000;
/// Evicted sessions remembered, so their calls answer `Evicted` rather than
/// a stale handle; the oldest is forgotten past this.
const MOST_EVICTED: usize = 64;
/// Destinations one decode call may give.
pub const MAX_DESTINATIONS: u32 = 16;
/// About how long one call works before it answers `Pending`, as the
/// encoders' (`encoders::CALL_BUDGET_NS`): under the gate's 5 ms exit
/// target.
pub const CALL_BUDGET_NS: u64 = 4_500_000;
/// How long a picture may take on NVDEC: past this the session is `Lost`.
pub const FRAME_BOUND_NS: u64 = 1_000_000_000;
/// The largest sample a call takes.
pub const MOST_SAMPLE: u64 = 16 << 20;
/// Rows copied into the caller between two looks at the budget.
const BAND_ROWS: usize = 64;
/// Past the engine's expected time by this much a picture is late, and each
/// call asks nouveau's fence once.
const LATE_NS: u64 = 2_000_000;
/// The engine's time for a 1080p picture before any is measured: the
/// self-test's on hardware (2026-11-18), H.264 and HEVC.
const H264_1080P_NS: u64 = 2_000_000;
const HEVC_1080P_NS: u64 = 1_200_000;
/// A 1080p readback by the copy engine (0.86 ms on hardware).
const COPY_1080P_NS: u64 = 900_000;

/// A GPU's decoders as the shared service reaches them.
pub trait GateDecoders: Send + Sync {
    /// Runs `operation`, one of [`is_decoder_operation`]'s, for
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
    /// Logs each session that decoded since the last report, unless a call
    /// holds the decoders now.
    fn report(&self);
    /// Closes every session, for the PC's stop.
    fn close_all(&self);
}

/// Whether `operation` is a decoder's, which never reaches the device.
pub fn is_decoder_operation(operation: u32) -> bool {
    operation & 0xfff0 == 0x0a10
}

/// The largest request or response record of a decoder operation.
const RECORD_BYTES: usize = 128;

/// Serves the decoder call `call` at `record` as the encoders' calls are
/// served (`encoders::serve_gate`): its records through the caller's
/// memory, its status and reason in the record.
pub fn serve_gate(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    record: u64,
    mut call: GateCall,
    caller: &mut dyn CallerMemory,
) -> Result<Status, CallerFault> {
    let outcome = serve_records(decoders, connection, &call, caller);
    let (status, reason) = match outcome {
        Ok(()) => (Status::Ok, String::new()),
        Err(fault) => (fault.status, alloc::format!("{fault}")),
    };
    call.status = status.raw();
    call.reason_length = reason.len() as u64;
    if !reason.is_empty() && call.reason.length >= call.reason_length {
        let _ = caller.write(call.reason.address, reason.as_bytes());
    }
    let mut raw = [0; size_of::<GateCall>()];
    wire::write(&call, &mut raw).expect("a record of its own size");
    caller.write(record, &raw)?;
    Ok(status)
}

fn serve_records(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    call: &GateCall,
    caller: &mut dyn CallerMemory,
) -> Result<(), Fault> {
    let operation = Operation::from_raw(call.operation)
        .filter(|operation| is_decoder_operation(operation.raw()))
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
    let served = decoders.serve(connection, operation, request, response, caller);
    // A decode answered `Pending` still answers its response: `retry_us`.
    let hints = matches!(served, Err(fault) if fault.status == Status::Pending)
        && operation == Operation::DecoderDecode;
    if served.is_ok() || hints {
        caller
            .write(call.response.address, response)
            .map_err(fault)?;
    }
    served
}

/// What a session did since a report, or since it opened.
#[derive(Clone, Copy, Default)]
struct Counts {
    /// Samples taken, pictures decoded and shown, and errors.
    samples: u64,
    frames: u64,
    outputs: u64,
    errors: u64,
    gpu_ns: u64,
    gpu_max: u64,
    ce_ns: u64,
    ce_max: u64,
    copy_ns: u64,
    copy_max: u64,
    total_ns: u64,
    total_max: u64,
    calls: u64,
    pending: u64,
    asks: u64,
    kicks: u64,
    /// The engine's time where a look saw it closely
    /// ([`super::pace::ACCURATE_NS`]).
    engine_ns: u64,
    engine_max: u64,
    engine_samples: u64,
    /// RASL pictures skipped, references stood in for, and CRAs that
    /// started afresh ([`crate::nvdec::parse::hevc::Events`]).
    skipped: u64,
    concealed: u64,
    restarted: u64,
}

impl Counts {
    fn engine(&mut self, ns: u64) {
        self.engine_ns += ns;
        self.engine_max = self.engine_max.max(ns);
        self.engine_samples += 1;
    }

    fn add(&mut self, other: &Self) {
        self.engine_ns += other.engine_ns;
        self.engine_max = self.engine_max.max(other.engine_max);
        self.engine_samples += other.engine_samples;
        self.skipped += other.skipped;
        self.concealed += other.concealed;
        self.restarted += other.restarted;
        self.samples += other.samples;
        self.frames += other.frames;
        self.outputs += other.outputs;
        self.errors += other.errors;
        self.gpu_ns += other.gpu_ns;
        self.gpu_max = self.gpu_max.max(other.gpu_max);
        self.ce_ns += other.ce_ns;
        self.ce_max = self.ce_max.max(other.ce_max);
        self.copy_ns += other.copy_ns;
        self.copy_max = self.copy_max.max(other.copy_max);
        self.total_ns += other.total_ns;
        self.total_max = self.total_max.max(other.total_max);
        self.calls += other.calls;
        self.pending += other.pending;
        self.asks += other.asks;
        self.kicks += other.kicks;
    }
}

/// `samples=… frames=… outputs=… errors=… skipped=… concealed=…
/// restarted=… gpu_us=avg/max ce_us=avg/max copy_us=avg/max total_us=avg/max
/// calls=… pending=… engine_us=avg/max/samples asks=… kicks=…`: `gpu_us`
/// per picture decoded, `ce_us` and `copy_us` per picture shown, `total_us`
/// per sample; `engine_us` where a look saw the picture land.
impl fmt::Display for Counts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let average = |total: u64, count: u64| total.checked_div(count).unwrap_or(0) / 1000;
        write!(
            formatter,
            "samples={} frames={} outputs={} errors={} skipped={} concealed={} restarted={} \
             gpu_us={}/{} ce_us={}/{} copy_us={}/{} total_us={}/{} calls={} pending={} \
             engine_us={}/{}/{} asks={} kicks={}",
            self.samples,
            self.frames,
            self.outputs,
            self.errors,
            self.skipped,
            self.concealed,
            self.restarted,
            average(self.gpu_ns, self.frames),
            self.gpu_max / 1000,
            average(self.ce_ns, self.outputs),
            self.ce_max / 1000,
            average(self.copy_ns, self.outputs),
            self.copy_max / 1000,
            average(self.total_ns, self.samples),
            self.total_max / 1000,
            self.calls,
            self.pending,
            average(self.engine_ns, self.engine_samples),
            self.engine_max / 1000,
            self.engine_samples,
            self.asks,
            self.kicks,
        )
    }
}

/// The work of one frame's calls.
#[derive(Clone, Copy)]
struct Work {
    frame: u64,
    flags: u32,
    /// The sample was parsed and, with a picture, submitted.
    taken: bool,
    /// It held a picture; when it was submitted, and when last looked at.
    decoded: bool,
    submitted: Option<u64>,
    looked: u64,
    reset: bool,
    flushed: bool,
    /// A call of it was answered `Ok`: one made again for it brings fresh
    /// destinations, for the pictures still waiting.
    answered: bool,
    /// Destinations filled, and the rows of the next one copied.
    outputs: u32,
    row: usize,
    /// Whether the head's readback was counted.
    staged: bool,
    status: session::Status,
    gpu_ns: u64,
    ce_ns: u64,
    copy_ns: u64,
    began: u64,
    calls: u32,
}

impl Work {
    fn new(frame: u64, flags: u32, began: u64) -> Self {
        Self {
            frame,
            flags,
            taken: false,
            decoded: false,
            submitted: None,
            looked: began,
            reset: false,
            flushed: false,
            answered: false,
            outputs: 0,
            row: 0,
            staged: false,
            status: session::Status::default(),
            gpu_ns: 0,
            ce_ns: 0,
            copy_ns: 0,
            began,
            calls: 0,
        }
    }
}

struct Slot<N: Node> {
    id: u64,
    connection: NonZeroU64,
    token: u64,
    decoder: Decoder<N>,
    opening: bool,
    lost: bool,
    work: Option<Work>,
    /// When the engine should be done with a picture, and when the
    /// frontend should call again ([`Pace`]).
    pace: Pace,
    /// The copy engine's expected time for a readback: from the self-test,
    /// then a running mean.
    copy_ns: u64,
    opened_at: u64,
    /// When the frontend last called for it.
    last_call: u64,
    period: Counts,
    total: Counts,
}

impl<N: Node> Slot<N> {
    /// `h264 1920x1080 8-bit nv12 display order`.
    fn describe(&self) -> String {
        let opened = self.decoder.opened();
        alloc::format!(
            "{} {}x{} {}-bit {} {} order",
            opened.codec.name(),
            opened.display.0,
            opened.display.1,
            opened.bit_depth,
            opened.format.name(),
            match self.decoder.order() {
                Order::Display => "display",
                Order::Decode => "decode",
            }
        )
    }
}

/// The decoders' state, which their lock gives one call at a time.
struct State<N: Node> {
    sessions: Vec<Slot<N>>,
    next_id: u64,
    /// Why no session opens on this GPU any more: one hung with its channel
    /// busy, or nouveau refused the decoder's channel.
    refused: Option<&'static str>,
    /// Sessions evicted and not yet closed by their frontend: id and
    /// connection, oldest first.
    evicted: Vec<(u64, NonZeroU64)>,
    /// When idle sessions were last looked for.
    swept: u64,
}

fn evicted() -> Fault {
    Fault::new(
        Status::Evicted,
        "the decoder session was evicted, idle, for another: open a new one at a sync sample",
    )
}

/// A GPU's decoders, over its platform.
pub struct Decoders<P: EncoderPlatform> {
    platform: P,
    /// The video memory its sessions may take together ([`VRAM_BUDGET`]).
    vram_budget: u64,
    held: AtomicBool,
    state: UnsafeCell<State<P::Node>>,
}

// SAFETY: the lock gives one processor at a time the state. A session's
// mappings are the host's, which every processor reaches at the same
// addresses, and its node is `Send`.
unsafe impl<P: EncoderPlatform> Send for Decoders<P> {}
// SAFETY: as above.
unsafe impl<P: EncoderPlatform> Sync for Decoders<P> {}

struct Held<'a, P: EncoderPlatform>(&'a Decoders<P>);

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

/// The fault of a decoder call that found another holding the decoders.
const BUSY: Fault = Fault::new(Status::Pending, "the decoders serve another call");
/// The fault of a call whose work for now is done, and which goes on when
/// made again.
const GOES_ON: Fault = Fault::new(Status::Pending, "the decoder goes on with the call");

fn lost() -> Fault {
    Fault::new(Status::Lost, "the decoder session stopped answering")
}

/// The fault a decoder's refusal answers.
fn refusal(error: Error) -> Fault {
    match error {
        Error::Unsupported(why) => Fault::new(Status::Unsupported, why),
        Error::Parse(error) => Fault::new(Status::Dropped, error.describe()),
        Error::Order(why) => Fault::new(Status::Invalid, why),
        Error::Session(session::Error::NoDecoder) => Fault::new(
            Status::Unsupported,
            "the GPU has no decoder this port drives",
        ),
        Error::Session(session::Error::Config(why)) => Fault::new(Status::Unsupported, why),
        Error::Session(session::Error::Refused { step, errno }) => {
            Fault::new(Status::Unsupported, step).signed(errno)
        }
        Error::Session(session::Error::TooLarge { name, .. } | session::Error::Map { name }) => {
            Fault::new(Status::OutOfMemory, name)
        }
        Error::Copy(crate::nvdec::copy::Error::Refused { step, errno }) => {
            Fault::new(Status::Unsupported, step).signed(errno)
        }
        Error::Copy(
            crate::nvdec::copy::Error::TooLarge { name, .. }
            | crate::nvdec::copy::Error::Map { name },
        ) => Fault::new(Status::OutOfMemory, name),
        Error::Copy(crate::nvdec::copy::Error::Config(why)) => Fault::new(Status::Unsupported, why),
        Error::Session(session::Error::Timeout { .. } | session::Error::Dead)
        | Error::Copy(
            crate::nvdec::copy::Error::Timeout { .. } | crate::nvdec::copy::Error::Dead,
        ) => lost(),
    }
}

/// The response of a decode answered `Pending`.
fn write_retry(response: &mut [u8], frame: u64, retry_us: u32, calls: u32) {
    let reply = DecoderDecodeResponse {
        frame,
        retry_us,
        calls,
        ..DecoderDecodeResponse::default()
    };
    // The response record is the operation's own size.
    let _ = wire::write(&reply, response);
}

/// A time scaled from 1080p to `width` by `height`, at least 0.3 ms.
fn scaled(at_1080p: u64, width: u32, height: u32, sample_bytes: u32) -> u64 {
    (at_1080p * u64::from(width) * u64::from(height) * u64::from(sample_bytes) / (1920 * 1080))
        .max(300_000)
}

/// Each plane's rows fit their destination span at its pitch.
fn fits(span: Span, pitch: u32, rows: usize, row_bytes: usize) -> bool {
    let pitch = pitch as usize;
    rows == 0 || (pitch >= row_bytes && span.length as usize >= (rows - 1) * pitch + row_bytes)
}

impl<P: EncoderPlatform> Decoders<P> {
    pub fn new(platform: P) -> Self {
        Self::with_vram_budget(platform, VRAM_BUDGET)
    }

    /// Decoders whose sessions take at most `vram_budget` bytes together.
    pub fn with_vram_budget(platform: P, vram_budget: u64) -> Self {
        Self {
            platform,
            vram_budget,
            held: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                sessions: Vec::new(),
                next_id: 1,
                refused: None,
                evicted: Vec::new(),
                swept: 0,
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
            "NVDEC SESSION GPU {} {line}",
            self.platform.index()
        ));
    }

    /// ` | gpu pstate=… pcie=…` from the platform, or nothing.
    fn gpu_state(&self) -> String {
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
        let closed = slot.decoder.close();
        if closed.keeps_file {
            state.refused = Some("a decoder session's channel stayed busy this boot");
        }
        let lived = self.platform.now_ns().saturating_sub(slot.opened_at) / 1_000_000;
        self.log(format_args!(
            "{} closed ({why}) after {lived} ms: {description}, {total}{}",
            slot.id,
            if closed.keeps_file {
                ", a channel still busy: its file stays open, and no session opens again"
            } else {
                ""
            }
        ));
        self.platform.close_node(closed.node, closed.keeps_file);
    }

    /// Evicts the session of slot `index`, idle `idle_ns`: it is closed
    /// (its video memory and channels freed), and its calls answer
    /// `Evicted` until its frontend closes it.
    fn evict(
        &self,
        state: &mut State<P::Node>,
        index: usize,
        idle_ns: u64,
        why: fmt::Arguments<'_>,
    ) {
        let (id, connection) = (state.sessions[index].id, state.sessions[index].connection);
        self.log(format_args!(
            "{id} evicted (idle {} ms) {why}",
            idle_ns / 1_000_000
        ));
        self.close_slot(state, index, "evicted");
        if state.evicted.len() >= MOST_EVICTED {
            state.evicted.remove(0);
        }
        state.evicted.push((id, connection));
    }

    /// Whether a session of `needed` bytes of video memory fits, after
    /// evicting the sessions idle at least [`EVICT_IDLE_NS`], least
    /// recently called first, that make it fit. Nothing is evicted when
    /// evicting every such session would not make it fit.
    fn make_room(&self, state: &mut State<P::Node>, connection: NonZeroU64, needed: u64) -> bool {
        let fits =
            |count: usize, taken: u64| count < MAX_SESSIONS && taken + needed <= self.vram_budget;
        let mut count = state.sessions.len();
        let mut taken: u64 = state
            .sessions
            .iter()
            .map(|slot| slot.decoder.layout().vram_bytes())
            .sum();
        if fits(count, taken) {
            return true;
        }
        let now = self.platform.now_ns();
        let mut idle: Vec<(u64, u64, u64)> = state
            .sessions
            .iter()
            .filter(|slot| now.saturating_sub(slot.last_call) >= EVICT_IDLE_NS)
            .map(|slot| (slot.last_call, slot.id, slot.decoder.layout().vram_bytes()))
            .collect();
        idle.sort_unstable();
        let mut chosen = Vec::new();
        for (last_call, id, bytes) in idle {
            if fits(count, taken) {
                break;
            }
            count -= 1;
            taken -= bytes;
            chosen.push((id, last_call));
        }
        if !fits(count, taken) {
            return false;
        }
        for (id, last_call) in chosen {
            if let Some(index) = state.sessions.iter().position(|slot| slot.id == id) {
                self.evict(
                    state,
                    index,
                    now.saturating_sub(last_call),
                    format_args!("for a new session of connection {connection}"),
                );
            }
        }
        true
    }

    /// Evicts the sessions but `keep` idle at least [`RELEASE_IDLE_NS`],
    /// looking at most once every [`SWEEP_NS`].
    fn sweep(&self, state: &mut State<P::Node>, keep: Option<u64>) {
        let now = self.platform.now_ns();
        if now.saturating_sub(state.swept) < SWEEP_NS {
            return;
        }
        state.swept = now;
        while let Some(index) = state.sessions.iter().position(|slot| {
            Some(slot.id) != keep && now.saturating_sub(slot.last_call) >= RELEASE_IDLE_NS
        }) {
            let idle_ns = now.saturating_sub(state.sessions[index].last_call);
            self.evict(
                state,
                index,
                idle_ns,
                format_args!(
                    "to give its video memory back: no call in {} s",
                    RELEASE_IDLE_NS / 1_000_000_000
                ),
            );
        }
    }

    /// Whether `connection`'s session `decoder` was evicted.
    fn was_evicted(state: &State<P::Node>, connection: NonZeroU64, decoder: u64) -> bool {
        state
            .evicted
            .iter()
            .any(|&(id, holder)| id == decoder && holder == connection)
    }

    fn capabilities(state: &State<P::Node>, response: &mut [u8]) -> Result<(), Fault> {
        if let Some(why) = state.refused {
            return Err(Fault::new(Status::Unsupported, why));
        }
        let reply = DecoderCapabilitiesResponse {
            codecs: 1 << EncoderCodec::H264.raw() | 1 << EncoderCodec::Hevc.raw(),
            profiles_h264: DECODER_PROFILE_H264_BASELINE
                | DECODER_PROFILE_H264_MAIN
                | DECODER_PROFILE_H264_HIGH,
            profiles_hevc: DECODER_PROFILE_HEVC_MAIN | DECODER_PROFILE_HEVC_MAIN10,
            depths: 1 << 8 | 1 << 10,
            formats: 1 << DecoderFormat::Nv12.raw() | 1 << DecoderFormat::P010.raw(),
            max_sessions: MAX_SESSIONS as u32,
            open_sessions: state.sessions.len() as u32,
            max_width: MAX_SIDE,
            max_height: MAX_SIDE,
            min_width: MIN_SIDE,
            min_height: MIN_SIDE,
            max_destinations: MAX_DESTINATIONS,
        };
        wire::write(&reply, response)
    }

    fn open(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &DecoderOpenRequest,
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        if request.token == 0 {
            return Err(Fault::malformed("decoder open token").value(0u64));
        }
        let index = match state
            .sessions
            .iter()
            .position(|slot| slot.connection == connection && slot.token == request.token)
        {
            Some(index) => index,
            None => self.begin(state, connection, request, caller)?,
        };
        let started = self.platform.now_ns();
        let slot = &mut state.sessions[index];
        slot.last_call = started;
        while slot.opening {
            match slot.decoder.bring_up_step() {
                Ok(true) => {
                    slot.opening = false;
                    let (pieces, _) = slot.decoder.made();
                    let took = self.platform.now_ns().saturating_sub(slot.opened_at);
                    let opened = slot.decoder.opened();
                    let line = alloc::format!(
                        "{} open for connection {connection}: {}, coded {}x{}, {} slots on {} \
                         surfaces, {pieces} pieces in {} ms, {} MiB of video memory, copy channel \
                         {}{}",
                        slot.id,
                        slot.describe(),
                        opened.coded.0,
                        opened.coded.1,
                        opened.slots,
                        opened.surfaces,
                        took / 1_000_000,
                        slot.decoder.layout().vram_bytes() >> 20,
                        slot.decoder.copy_channel().unwrap_or(-1),
                        self.gpu_state()
                    );
                    self.log(format_args!("{line}"));
                }
                Ok(false) => {
                    if self.platform.now_ns().saturating_sub(started) >= CALL_BUDGET_NS {
                        return Err(GOES_ON);
                    }
                }
                Err(error) => {
                    let (made, planned) = slot.decoder.made();
                    let id = slot.id;
                    if matches!(
                        error,
                        Error::Session(session::Error::Refused {
                            step: "VM_INIT" | "channel" | "class",
                            ..
                        })
                    ) {
                        state.refused = Some("nouveau refused the decoder's channel this boot");
                    }
                    self.log(format_args!(
                        "{id} refused at open after {made} of {planned} pieces: {error:?}"
                    ));
                    self.close_slot(state, index, "its open failed");
                    return Err(refusal(error));
                }
            }
        }
        let slot = &state.sessions[index];
        let opened = slot.decoder.opened();
        wire::write(
            &DecoderOpenResponse {
                decoder: slot.id,
                coded_width: opened.coded.0,
                coded_height: opened.coded.1,
                display_width: opened.display.0,
                display_height: opened.display.1,
                bit_depth: u32::from(opened.bit_depth),
                format: match opened.format {
                    Format::Nv12 => DecoderFormat::Nv12.raw(),
                    Format::P010 => DecoderFormat::P010.raw(),
                },
                slots: u32::from(opened.slots),
                surfaces: u32::from(opened.surfaces),
                reserved: 0,
            },
            response,
        )
    }

    /// A new slot for `request`, its decoder begun but its buffers not made.
    fn begin(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &DecoderOpenRequest,
        caller: &mut dyn CallerMemory,
    ) -> Result<usize, Fault> {
        if let Some(why) = state.refused {
            return Err(Fault::new(Status::Unsupported, why));
        }
        let codec = match EncoderCodec::from_raw(request.codec) {
            Some(EncoderCodec::H264) => Codec::H264,
            Some(EncoderCodec::Hevc) => Codec::Hevc,
            None => return Err(Fault::malformed("decoder codec").value(request.codec)),
        };
        if request.flags & !DECODER_OPEN_DECODE_ORDER != 0 {
            return Err(Fault::malformed("decoder open flags").bits(request.flags));
        }
        if request.config.length == 0 || request.config.length > 0x1_0000 {
            return Err(Fault::new(
                Status::Invalid,
                "a configuration record of 1 byte to 64 KiB",
            )
            .value(request.config.length));
        }
        let mut record = alloc::vec![0; request.config.length as usize];
        caller
            .read(request.config.address, &mut record)
            .map_err(fault)?;
        // Admitted by video memory, before any file opens.
        let needed = match decoder::vram_bytes(codec, &record) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.log(format_args!(
                    "refused a {} stream for connection {connection}: {error:?}",
                    codec.name()
                ));
                return Err(match error {
                    // A record the parser refuses is a stream the host
                    // does not take: the frontend decodes it its way.
                    Error::Parse(error) => Fault::new(Status::Unsupported, error.describe()),
                    error => refusal(error),
                });
            }
        };
        // Sessions idle long enough make room, least recently called first.
        let room = self.make_room(state, connection, needed);
        if !room && state.sessions.len() >= MAX_SESSIONS {
            self.log(format_args!(
                "refused a {} stream for connection {connection}: {MAX_SESSIONS} sessions open \
                 ({})",
                codec.name(),
                self.idle_list(state)
            ));
            return Err(Fault::new(
                Status::Unsupported,
                "every video decoder session of the GPU is in use",
            )
            .value(MAX_SESSIONS as u64));
        }
        let taken: u64 = state
            .sessions
            .iter()
            .map(|slot| slot.decoder.layout().vram_bytes())
            .sum();
        if !room {
            self.log(format_args!(
                "refused a {} stream for connection {connection}: it takes {} MiB of video \
                 memory, {} MiB of {} MiB are taken ({})",
                codec.name(),
                needed >> 20,
                taken >> 20,
                self.vram_budget >> 20,
                self.idle_list(state)
            ));
            return Err(Fault::new(
                Status::Unsupported,
                "the video memory of the GPU's decoder sessions is taken",
            )
            .value(needed));
        }
        let node = self
            .platform
            .open_node()
            .map_err(|why| Fault::new(Status::Unsupported, why))?;
        let mut decoder = match Decoder::begin(
            node,
            self.platform.chipset(),
            codec,
            &record,
            FRAME_BOUND_NS,
        ) {
            Ok(decoder) => decoder,
            Err((error, node)) => {
                self.platform.close_node(node, false);
                self.log(format_args!(
                    "refused a {} stream for connection {connection}: {error:?}",
                    codec.name()
                ));
                return Err(match error {
                    // A record the parser refuses is a stream the host
                    // does not take: the frontend decodes it its way.
                    Error::Parse(error) => Fault::new(Status::Unsupported, error.describe()),
                    error => refusal(error),
                });
            }
        };
        if request.flags & DECODER_OPEN_DECODE_ORDER != 0 {
            decoder.set_order(Order::Decode);
        }
        let opened = decoder.opened();
        let sample_bytes = opened.format.sample_bytes();
        let (width, height) = opened.display;
        let engine_ns = scaled(
            match codec {
                Codec::H264 => H264_1080P_NS,
                Codec::Hevc => HEVC_1080P_NS,
            },
            width,
            height,
            1,
        );
        let id = state.next_id;
        state.next_id += 1;
        let now = self.platform.now_ns();
        state.sessions.push(Slot {
            id,
            connection,
            token: request.token,
            decoder,
            opening: true,
            lost: false,
            work: None,
            pace: Pace::new(engine_ns),
            copy_ns: scaled(COPY_1080P_NS, width, height, sample_bytes),
            opened_at: now,
            last_call: now,
            period: Counts::default(),
            total: Counts::default(),
        });
        Ok(state.sessions.len() - 1)
    }

    /// `3 for connection 2, 410 MiB, idle 12 ms; …`: the sessions open, which
    /// a refused open logs, so a session a client never closed shows.
    fn idle_list(&self, state: &State<P::Node>) -> String {
        let now = self.platform.now_ns();
        let mut list = String::new();
        for (k, slot) in state.sessions.iter().enumerate() {
            if k > 0 {
                list.push_str("; ");
            }
            list.push_str(&alloc::format!(
                "{} for connection {}, {} MiB, {} frames, idle {} ms",
                slot.id,
                slot.connection,
                slot.decoder.layout().vram_bytes() >> 20,
                slot.total.samples + slot.period.samples,
                now.saturating_sub(slot.last_call) / 1_000_000
            ));
        }
        list
    }

    fn decode(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        request: &DecoderDecodeRequest,
        response: &mut [u8],
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        let index = match slot_of(state, connection, request.decoder) {
            Ok(index) => index,
            Err(_) if Self::was_evicted(state, connection, request.decoder) => {
                return Err(evicted());
            }
            Err(fault) => return Err(fault),
        };
        let started = self.platform.now_ns();
        let slot = &mut state.sessions[index];
        slot.period.calls += 1;
        slot.last_call = started;
        slot.pace.arrived(started);
        if slot.lost {
            return Err(lost());
        }
        if slot.opening {
            return Err(Fault::new(
                Status::Invalid,
                "the session is still being made",
            ));
        }
        if request.count > MAX_DESTINATIONS {
            return Err(Fault::new(Status::Invalid, "at most 16 destinations").value(request.count));
        }
        if request.flags & !(DECODE_FLUSH | DECODE_RESET) != 0 {
            return Err(Fault::malformed("decode flags").bits(request.flags));
        }
        if request.sample.length > MOST_SAMPLE {
            return Err(Fault::new(Status::Invalid, "a sample larger than 16 MiB")
                .value(request.sample.length));
        }
        // A new frame: the last one's picture, if still in flight, lands
        // first (its pictures are then shown in this call).
        if slot.work.map(|work| work.frame) != Some(request.frame) {
            if slot.decoder.in_flight() {
                match self.poll(slot, started) {
                    Ok(true) => {}
                    Ok(false) => {
                        slot.period.pending += 1;
                        return Err(self.goes_on(slot, request.frame, response));
                    }
                    Err(fault) => return Err(self.fail(state, index, fault)),
                }
            }
            let slot = &mut state.sessions[index];
            slot.work = Some(Work::new(request.frame, request.flags, started));
        }
        let slot = &mut state.sessions[index];
        if let Some(work) = slot.work.as_mut() {
            work.calls += 1;
            // Called again after an answer: the destinations ran out, and
            // these are new ones for the pictures still waiting.
            if work.answered {
                work.answered = false;
                work.outputs = 0;
            }
        }
        match self.work(slot, request, started, caller) {
            Ok(true) => {
                let work = slot.work.as_mut().expect("a frame's calls have their work");
                work.answered = true;
                let work = *work;
                answer(&work, slot.decoder.queued(), response)
            }
            Ok(false) => {
                slot.period.pending += 1;
                Err(self.goes_on(slot, request.frame, response))
            }
            Err(fault) if fault.status == Status::Unmapped => Err(fault),
            Err(fault) => Err(self.fail(state, index, fault)),
        }
    }

    /// The frame's work, from where its last call left it: whether it is
    /// done (`false`: out of budget, or the engine still works).
    fn work(
        &self,
        slot: &mut Slot<P::Node>,
        request: &DecoderDecodeRequest,
        started: u64,
        caller: &mut dyn CallerMemory,
    ) -> Result<bool, Fault> {
        let mut work = slot.work.expect("a frame's calls have their work");
        let result = loop {
            if work.flags & DECODE_RESET != 0 && !work.reset && !work.taken {
                slot.decoder.reset().map_err(refusal)?;
                work.reset = true;
            }
            match self.show(slot, &mut work, request, started, caller) {
                Ok(true) => {}
                Ok(false) => break Ok(false),
                Err(fault) => break Err(fault),
            }
            if slot.decoder.queued() > 0 {
                // The destinations ran out: the frontend calls again with
                // more, before the sample is taken.
                break Ok(true);
            }
            if !work.taken {
                if let Err(fault) = self.take(slot, &mut work, request, caller) {
                    break Err(fault);
                }
                continue;
            }
            if slot.decoder.in_flight() {
                slot.work = Some(work);
                match self.poll(slot, started) {
                    Ok(true) => {
                        work = slot.work.expect("its work");
                        continue;
                    }
                    Ok(false) => {
                        work = slot.work.expect("its work");
                        break Ok(false);
                    }
                    Err(fault) => {
                        work = slot.work.expect("its work");
                        break Err(fault);
                    }
                }
            }
            if work.flags & DECODE_FLUSH != 0 && !work.flushed {
                slot.decoder.flush().map_err(refusal)?;
                work.flushed = true;
                continue;
            }
            break Ok(true);
        };
        slot.work = Some(work);
        result
    }

    /// Reads the sample and submits its picture.
    fn take(
        &self,
        slot: &mut Slot<P::Node>,
        work: &mut Work,
        request: &DecoderDecodeRequest,
        caller: &mut dyn CallerMemory,
    ) -> Result<(), Fault> {
        work.taken = true;
        if request.sample.length == 0 {
            return Ok(());
        }
        let mut sample = alloc::vec![0; request.sample.length as usize];
        if let Err(error) = caller.read(request.sample.address, &mut sample) {
            // Made again once the frontend touched its pages.
            work.taken = false;
            return Err(fault(error));
        }
        slot.period.samples += 1;
        let before = slot.decoder.events();
        let submitted = slot.decoder.submit(&sample, request.frame);
        self.note_events(slot, request.frame, before);
        match submitted {
            Ok(true) => {
                work.decoded = true;
                let now = self.platform.now_ns();
                work.submitted = Some(now);
                work.looked = now;
                // nouveau's EXEC queued the job; its scheduler pushes it
                // when Linux next runs. Run it now, or ask the fence.
                if self.platform.kick() {
                    slot.period.kicks += 1;
                } else {
                    slot.decoder.ask_fence();
                    slot.period.asks += 1;
                }
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(Error::Parse(error)) => {
                slot.period.errors += 1;
                if slot.total.errors + slot.period.errors <= 4 {
                    // What the sample was and the references then: its NAL
                    // unit, order count, reference picture set, the buffer.
                    let refusal = slot.decoder.refusal().unwrap_or("");
                    self.log(format_args!(
                        "{} frame {} dropped: {} ({}) [{refusal}]",
                        slot.id,
                        request.frame,
                        error.describe(),
                        error.kind()
                    ));
                }
                Err(Fault::new(Status::Dropped, error.describe()))
            }
            Err(error) => Err(refusal(error)),
        }
    }

    /// Counts and logs (the first few) what the stream's parser did about
    /// frame `frame`'s sample, against its events `before`.
    fn note_events(
        &self,
        slot: &mut Slot<P::Node>,
        frame: u64,
        before: crate::nvdec::parse::hevc::Events,
    ) {
        let after = slot.decoder.events();
        let skipped = after.skipped - before.skipped;
        let concealed = after.concealed - before.concealed;
        let restarted = after.restarted - before.restarted;
        if skipped + concealed + restarted == 0 {
            return;
        }
        let seen = slot.total.skipped
            + slot.period.skipped
            + slot.total.concealed
            + slot.period.concealed
            + slot.total.restarted
            + slot.period.restarted;
        slot.period.skipped += u64::from(skipped);
        slot.period.concealed += u64::from(concealed);
        slot.period.restarted += u64::from(restarted);
        if seen < 8 {
            self.log(format_args!(
                "{} frame {frame}: {}{}{}",
                slot.id,
                if restarted > 0 {
                    "a CRA starts afresh (after a flush, or its references were never decoded: \
                     a seek or a loop); "
                } else {
                    ""
                },
                if skipped > 0 {
                    "a RASL picture of a CRA that started afresh, skipped (shown dropped)"
                } else {
                    "decoded"
                },
                if concealed > 0 {
                    alloc::format!(
                        ", {concealed} reference(s) never decoded stood in for by the nearest"
                    )
                } else {
                    String::new()
                }
            ));
        }
    }

    /// Looks at the picture in flight until it is released or the call's
    /// budget from `started` is spent; whether it was released (its
    /// pictures then wait to be shown).
    fn poll(&self, slot: &mut Slot<P::Node>, started: u64) -> Result<bool, Fault> {
        let budget_end = started.saturating_add(CALL_BUDGET_NS);
        let mut asked = false;
        loop {
            match slot.decoder.poll(false) {
                Ok(Some(decoded)) => {
                    let seen = self.platform.now_ns();
                    let gpu_ns = decoded.timing.gpu_ns;
                    if let Some(work) = slot.work
                        && let Some(submitted) = work.submitted
                        && let Some(engine) = slot.pace.done(submitted, work.looked, seen)
                    {
                        slot.period.engine(engine);
                    }
                    slot.period.frames += 1;
                    slot.period.gpu_ns += gpu_ns;
                    slot.period.gpu_max = slot.period.gpu_max.max(gpu_ns);
                    if !decoded.status.clean() {
                        slot.period.errors += 1;
                    }
                    if let Some(work) = slot.work.as_mut() {
                        work.status = decoded.status;
                        work.gpu_ns = gpu_ns;
                        work.submitted = None;
                    }
                    return Ok(true);
                }
                Ok(None) => {
                    let now = self.platform.now_ns();
                    let submitted = slot.work.and_then(|work| work.submitted).unwrap_or(started);
                    if let Some(work) = slot.work.as_mut() {
                        work.looked = now;
                    }
                    // A late picture: ask nouveau's fence once a call,
                    // which also lets the host's Linux run.
                    if !asked && now > slot.pace.expected(submitted).saturating_add(LATE_NS) {
                        slot.decoder.ask_fence();
                        slot.period.asks += 1;
                        asked = true;
                    }
                    if !slot.pace.keep_looking(now, submitted, budget_end) {
                        return Ok(false);
                    }
                    self.platform.pause();
                }
                Err(error) => {
                    slot.period.errors += 1;
                    if error.lost() {
                        self.log(format_args!(
                            "{} LOST: no picture in {} ms ({error:?}): nouveau's fence {}",
                            slot.id,
                            FRAME_BOUND_NS / 1_000_000,
                            if error.pending() {
                                "pending: the channel is busy"
                            } else {
                                "signalled: the engine wrote nothing"
                            }
                        ));
                    }
                    return Err(refusal(error));
                }
            }
        }
    }

    /// Shows the waiting pictures in the request's destinations, from
    /// where the frame's calls left off: whether it may go on (`false`:
    /// out of budget, or the copy engine still works). It stops at the
    /// last destination.
    fn show(
        &self,
        slot: &mut Slot<P::Node>,
        work: &mut Work,
        request: &DecoderDecodeRequest,
        started: u64,
        caller: &mut dyn CallerMemory,
    ) -> Result<bool, Fault> {
        let budget_end = started.saturating_add(CALL_BUDGET_NS);
        let record_bytes = size_of::<DecoderPlanes>() as u64;
        if request.count > 0
            && request.destinations.length < u64::from(request.count) * record_bytes
        {
            return Err(Fault::new(
                Status::Invalid,
                "the destinations span is shorter than its count",
            ));
        }
        while slot.decoder.queued() > 0 && work.outputs < request.count {
            // The readback: the copy engine into the staging buffer.
            let mut asked = false;
            let ce_ns = loop {
                match slot.decoder.stage(false) {
                    Ok(Some(ce_ns)) => break ce_ns,
                    Ok(None) => {
                        let now = self.platform.now_ns();
                        if now >= budget_end {
                            return Ok(false);
                        }
                        if !asked && now > started.saturating_add(slot.copy_ns + LATE_NS) {
                            asked = true;
                            if let Ok(None) = slot.decoder.stage(true) {
                                slot.period.asks += 1;
                            }
                        }
                        self.platform.pause();
                    }
                    Err(error) => {
                        slot.period.errors += 1;
                        if error.lost() {
                            self.log(format_args!(
                                "{} LOST: the copy engine's readback never landed ({error:?})",
                                slot.id
                            ));
                        }
                        return Err(refusal(error));
                    }
                }
            };
            if !work.staged {
                work.staged = true;
                work.ce_ns += ce_ns;
                slot.copy_ns = (3 * slot.copy_ns + ce_ns) / 4;
                slot.period.ce_ns += ce_ns;
                slot.period.ce_max = slot.period.ce_max.max(ce_ns);
            }
            // The destination, and its planes' rows from the caller's.
            let at = request.destinations.address + u64::from(work.outputs) * record_bytes;
            let mut raw = [0; size_of::<DecoderPlanes>()];
            caller.read(at, &mut raw).map_err(fault)?;
            let mut destination: DecoderPlanes = wire::read(&raw)?;
            let copy_started = self.platform.now_ns();
            {
                let planes = slot
                    .decoder
                    .staged()
                    .ok_or(Fault::new(Status::Invalid, "the staged picture is gone"))?;
                if !fits(
                    destination.luma,
                    destination.luma_pitch,
                    planes.luma_rows,
                    planes.row_bytes,
                ) || !fits(
                    destination.chroma,
                    destination.chroma_pitch,
                    planes.chroma_rows,
                    planes.row_bytes,
                ) {
                    return Err(Fault::new(
                        Status::Invalid,
                        "a destination's planes are smaller than the session's pictures",
                    ));
                }
                let rows = planes.luma_rows + planes.chroma_rows;
                while work.row < rows {
                    if work.row > 0 && self.platform.now_ns() >= budget_end {
                        work.copy_ns += self.platform.now_ns().saturating_sub(copy_started);
                        return Ok(false);
                    }
                    let (plane, span, pitch, first) = if work.row < planes.luma_rows {
                        (
                            planes.luma,
                            destination.luma,
                            destination.luma_pitch as usize,
                            work.row,
                        )
                    } else {
                        (
                            planes.chroma,
                            destination.chroma,
                            destination.chroma_pitch as usize,
                            work.row - planes.luma_rows,
                        )
                    };
                    let plane_rows = plane.len() / planes.row_bytes;
                    let count = BAND_ROWS.min(plane_rows - first);
                    let from = first * planes.row_bytes;
                    if pitch == planes.row_bytes {
                        caller
                            .write(
                                span.address + from as u64,
                                &plane[from..from + count * planes.row_bytes],
                            )
                            .map_err(fault)?;
                    } else {
                        for row in first..first + count {
                            let source = &plane[row * planes.row_bytes..][..planes.row_bytes];
                            caller
                                .write(span.address + (row * pitch) as u64, source)
                                .map_err(fault)?;
                        }
                    }
                    work.row += count;
                }
            }
            let copy_ns = self.platform.now_ns().saturating_sub(copy_started);
            work.copy_ns += copy_ns;
            let head = slot.decoder.head().expect("a picture waits");
            destination.frame = head.token;
            destination.poc = head.poc;
            destination.flags = DESTINATION_WRITTEN
                | if head.status.clean() && !head.concealed {
                    0
                } else {
                    DESTINATION_ERRORS
                };
            caller.write(at, wire::bytes(&destination)).map_err(fault)?;
            slot.decoder.pop();
            slot.period.outputs += 1;
            slot.period.copy_ns += copy_ns;
            slot.period.copy_max = slot.period.copy_max.max(copy_ns);
            work.outputs += 1;
            work.row = 0;
            work.staged = false;
        }
        Ok(true)
    }

    /// `Pending` for a frame still worked on, its response's `retry_us`
    /// saying when to call again: about when the picture in flight lands,
    /// less the frontend's wake-up ([`Pace::retry_us`]), else at once (the
    /// call only ran out of budget showing pictures).
    fn goes_on(&self, slot: &mut Slot<P::Node>, frame: u64, response: &mut [u8]) -> Fault {
        let calls = slot.work.map_or(0, |work| work.calls);
        let now = self.platform.now_ns();
        let retry_us = match slot.work.and_then(|work| work.submitted) {
            Some(submitted) if slot.decoder.in_flight() => slot.pace.retry_us(now, submitted),
            _ => {
                slot.pace.asked(now, RETRY_AT_ONCE_US);
                RETRY_AT_ONCE_US
            }
        };
        write_retry(response, frame, retry_us, calls);
        GOES_ON
    }

    /// A fault of `index`'s frame: a lost engine makes the session lost
    /// (and a busy channel the GPU refused for new sessions); a stream the
    /// session cannot go on with makes it lost too, for the frontend's
    /// own decoder to take over.
    fn fail(&self, state: &mut State<P::Node>, index: usize, fault: Fault) -> Fault {
        let slot = &mut state.sessions[index];
        match fault.status {
            Status::Lost => {
                slot.lost = true;
                if slot.decoder.dead() == Some(true) {
                    state.refused = Some("a decoder session hung this boot");
                }
            }
            Status::Unsupported | Status::OutOfMemory => {
                slot.lost = true;
                self.log(format_args!("{} gives up on its stream: {fault}", slot.id));
            }
            _ => {}
        }
        fault
    }

    fn close(
        &self,
        state: &mut State<P::Node>,
        connection: NonZeroU64,
        decoder: u64,
    ) -> Result<(), Fault> {
        match slot_of(state, connection, decoder) {
            Ok(index) => {
                self.close_slot(state, index, "the frontend closed it");
                Ok(())
            }
            Err(_) if Self::was_evicted(state, connection, decoder) => {
                state
                    .evicted
                    .retain(|&(id, holder)| !(id == decoder && holder == connection));
                Ok(())
            }
            Err(fault) => Err(fault),
        }
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
        // Sessions long idle give their video memory back, but never the
        // one this call is for.
        let keep = match operation {
            Operation::DecoderDecode => wire::read::<DecoderDecodeRequest>(request)
                .ok()
                .map(|request| request.decoder),
            Operation::DecoderClose => wire::read::<Handle>(request).ok().map(|h| h.handle),
            _ => None,
        };
        self.sweep(state, keep);
        match operation {
            Operation::DecoderCapabilities => Self::capabilities(state, response),
            Operation::DecoderOpen => {
                let request: DecoderOpenRequest = wire::read(request)?;
                self.open(state, connection, &request, response, caller)
            }
            Operation::DecoderDecode => {
                let request: DecoderDecodeRequest = wire::read(request)?;
                let result = self.decode(state, connection, &request, response, caller);
                // A frame answered: its calls' time joins the counts.
                if result.is_ok()
                    && let Ok(index) = slot_of(state, connection, request.decoder)
                {
                    let now = self.platform.now_ns();
                    let slot = &mut state.sessions[index];
                    if let Some(work) = slot.work {
                        let total = now.saturating_sub(work.began);
                        slot.period.total_ns += total;
                        slot.period.total_max = slot.period.total_max.max(total);
                    }
                }
                result
            }
            Operation::DecoderClose => {
                let Handle { handle } = wire::read(request)?;
                self.close(state, connection, handle)?;
                wire::write(&wire::Empty {}, response)
            }
            _ => Err(
                Fault::new(Status::UnknownOperation, "not a decoder operation")
                    .value(operation.raw()),
            ),
        }
    }
}

/// Answers the frame's `work`, with `queued` pictures still waiting.
fn answer(work: &Work, queued: usize, response: &mut [u8]) -> Result<(), Fault> {
    let micros = |ns: u64| u32::try_from(ns / 1000).unwrap_or(u32::MAX);
    wire::write(
        &DecoderDecodeResponse {
            frame: work.frame,
            taken: u32::from(work.taken),
            decoded: u32::from(work.decoded),
            outputs: work.outputs,
            queued: queued as u32,
            mbs_in_error: work.status.mbs_in_error,
            error_status: work.status.error_status,
            gpu_us: micros(work.gpu_ns),
            ce_us: micros(work.ce_ns),
            copy_us: micros(work.copy_ns),
            retry_us: 0,
            calls: work.calls,
            reserved: 0,
        },
        response,
    )
}

/// The slot of session `decoder`, which `connection` must hold.
fn slot_of<N: Node>(
    state: &State<N>,
    connection: NonZeroU64,
    decoder: u64,
) -> Result<usize, Fault> {
    state
        .sessions
        .iter()
        .position(|slot| slot.id == decoder && slot.connection == connection)
        .ok_or(Fault::STALE)
}

impl<P: EncoderPlatform> GateDecoders for Decoders<P> {
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
            state.evicted.retain(|&(_, holder)| holder != connection);
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

const _: () = assert!(size_of::<DecoderDecodeRequest>() <= RECORD_BYTES);
const _: () = assert!(size_of::<DecoderOpenRequest>() <= RECORD_BYTES);
const _: () = assert!(size_of::<DecoderDecodeResponse>() <= RECORD_BYTES);
const _: () = assert!(size_of::<DecoderOpenResponse>() <= RECORD_BYTES);
const _: () = assert!(size_of::<DecoderCapabilitiesResponse>() <= RECORD_BYTES);
const _: () = assert!(decoder::MAX_SIDE == crate::nvenc::layout::MAX_SIDE);
