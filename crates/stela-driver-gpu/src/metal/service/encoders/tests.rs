//! The gate's encoders on the mock GPU (`crate::nvenc::mock`): sessions
//! opened, fed from the caller's memory a band at a time and closed, the
//! refusals, a hung engine, and sessions closed with their connection.
extern crate std;

use super::*;
use crate::metal::device::fixture::{MockBackend, MockCompiler, device_on};
use crate::metal::service::{
    Served, Service, ServiceHost, SharedService,
    identity::DeviceEpoch,
    wire::{EncoderOpenResponse, Record, Value},
};
use crate::nvenc::{
    SYNCOBJ_WAIT,
    bits::nal_types,
    layout::Geometry,
    mock::{Engine, Gpu, Timing},
    surface,
};
use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Mutex;
use stela_contracts::gate::GateService;

/// GA106, the RTX 3050's chipset.
const GA106: u16 = 0x176;
const ONE: NonZeroU64 = NonZeroU64::MIN;
const TWO: NonZeroU64 = NonZeroU64::new(2).unwrap();

/// A GPU of mock render node files.
struct Platform {
    engine: Engine,
    chipset: u16,
    /// Requests every new file refuses, with their errno.
    refuse: Vec<(u32, i64)>,
    /// The clock, and how far each reading moves it.
    clock: AtomicU64,
    step: AtomicU64,
    opened: AtomicU32,
    closed: Mutex<Vec<(Gpu, bool)>>,
    lines: Mutex<Vec<String>>,
    /// The timing model every file shares, its clock then the platform's,
    /// and whether `kick` runs Linux here.
    timing: Option<Timing>,
    kicks: bool,
    /// The GPU's state the platform reports, if any.
    state: Option<&'static str>,
}

impl Platform {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            chipset: GA106,
            refuse: Vec::new(),
            clock: AtomicU64::new(1_000_000_000),
            step: AtomicU64::new(1000),
            opened: AtomicU32::new(0),
            closed: Mutex::new(Vec::new()),
            lines: Mutex::new(Vec::new()),
            timing: None,
            kicks: false,
            state: None,
        }
    }

    /// A platform on the timing model: an engine taking `engine_ns` a
    /// frame once Linux pushed it, and Linux run in place by `kick` or not.
    fn timed(engine_ns: u64, kicks: bool) -> Self {
        let mut platform = Self::new(Engine::Encode);
        platform.timing = Some(Timing {
            clock: Arc::new(AtomicU64::new(1_000_000_000)),
            engine_ns,
            pushes: Arc::new(AtomicU64::new(0)),
        });
        platform.kicks = kicks;
        platform
    }

    /// The shared clock, which the frontend's sleeps move too.
    fn clock(&self) -> &AtomicU64 {
        self.timing
            .as_ref()
            .map_or(&self.clock, |timing| &timing.clock)
    }

    fn logged(&self, part: &str) -> bool {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(part))
    }
}

impl EncoderPlatform for Platform {
    type Node = Gpu;

    fn index(&self) -> usize {
        0
    }

    fn chipset(&self) -> u16 {
        self.chipset
    }

    fn open_node(&self) -> Result<Gpu, &'static str> {
        self.opened.fetch_add(1, Ordering::Relaxed);
        let mut gpu = Gpu::new(self.engine);
        gpu.refuse = self.refuse.clone();
        gpu.timing = self.timing.clone();
        Ok(gpu)
    }

    fn close_node(&self, node: Gpu, keeps_file: bool) {
        self.closed.lock().unwrap().push((node, keeps_file));
    }

    fn now_ns(&self) -> u64 {
        let step = self.step.load(Ordering::Relaxed);
        self.clock().fetch_add(step, Ordering::Relaxed) + step
    }

    fn pause(&self) {
        let pause = if self.timing.is_some() {
            10_000
        } else {
            50_000
        };
        self.clock().fetch_add(pause, Ordering::Relaxed);
    }

    fn kick(&self) -> bool {
        match &self.timing {
            Some(timing) if self.kicks => {
                timing.pushes.fetch_add(1, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }

    fn gpu_state(&self) -> Option<String> {
        self.state.map(String::from)
    }

    fn run(&self, body: &mut dyn FnMut()) {
        body();
    }

    fn log(&self, line: fmt::Arguments<'_>) {
        self.lines.lock().unwrap().push(alloc::format!("{line}"));
    }
}

/// The calling task's memory: one run of bytes at [`BASE`], of which a
/// range may be out of reach.
struct Memory {
    bytes: Vec<u8>,
    away: Option<core::ops::Range<u64>>,
}

const BASE: u64 = 0x1_0000_0000;

impl Memory {
    fn new(bytes: usize) -> Self {
        Self {
            bytes: vec![0; bytes],
            away: None,
        }
    }

    fn range(&self, address: u64, length: usize) -> Result<core::ops::Range<usize>, CallerFault> {
        let start = address
            .checked_sub(BASE)
            .ok_or(CallerFault::NotRam { address })? as usize;
        if start + length > self.bytes.len() {
            return Err(CallerFault::NotRam { address });
        }
        if let Some(away) = &self.away
            && address < away.end
            && away.start < address + length as u64
        {
            return Err(CallerFault::Denied {
                address: away.start.max(address),
            });
        }
        Ok(start..start + length)
    }
}

impl CallerMemory for Memory {
    fn read(&mut self, address: u64, buffer: &mut [u8]) -> Result<(), CallerFault> {
        let range = self.range(address, buffer.len())?;
        buffer.copy_from_slice(&self.bytes[range]);
        Ok(())
    }

    fn write(&mut self, address: u64, data: &[u8]) -> Result<(), CallerFault> {
        let range = self.range(address, data.len())?;
        self.bytes[range].copy_from_slice(data);
        Ok(())
    }
}

/// Where the test's caller keeps each thing.
const SETS: u64 = BASE;
const OUT: u64 = BASE + 0x1000;
const OUT_BYTES: u64 = 0x80_0000;
const PICTURE: u64 = OUT + OUT_BYTES;

fn geometry() -> Geometry {
    Geometry::new(1920, 1080).unwrap()
}

/// A caller holding frame `frame` of the test pattern as pitch-wide NV12
/// at [`PICTURE`].
fn caller_with(frame: u32) -> Memory {
    let geometry = geometry();
    let nv12 = surface::pattern(&geometry, frame);
    let mut memory = Memory::new((PICTURE - BASE) as usize + nv12.len());
    let at = (PICTURE - BASE) as usize;
    memory.bytes[at..at + nv12.len()].copy_from_slice(&nv12);
    memory
}

fn put_picture(memory: &mut Memory, frame: u32) {
    let nv12 = surface::pattern(&geometry(), frame);
    let at = (PICTURE - BASE) as usize;
    memory.bytes[at..at + nv12.len()].copy_from_slice(&nv12);
}

fn open_request(token: u64, codec: EncoderCodec) -> EncoderOpenRequest {
    EncoderOpenRequest {
        token,
        codec: codec.raw(),
        width: 1920,
        height: 1080,
        fps_milli: 60_000,
        rate: EncoderRate::Cbr.raw(),
        bitrate: 20_000_000,
        qp: 0,
        flags: 0,
        parameter_sets: Span {
            address: SETS,
            length: 0x1000,
        },
    }
}

fn encode_request(encoder: u64, frame: u64, flags: u32) -> EncoderEncodeRequest {
    let geometry = geometry();
    let pitch = geometry.pitch as u64;
    let luma_bytes = pitch * geometry.coded_height as u64;
    EncoderEncodeRequest {
        encoder,
        frame,
        luma: Span {
            address: PICTURE,
            length: luma_bytes,
        },
        chroma: Span {
            address: PICTURE + luma_bytes,
            length: luma_bytes / 2,
        },
        luma_pitch: pitch as u32,
        chroma_pitch: pitch as u32,
        input: EncoderInput::Nv12.raw(),
        flags,
        bitrate: 0,
        reserved: 0,
        out: Span {
            address: OUT,
            length: OUT_BYTES,
        },
    }
}

/// One call of `operation`: its outcome and response.
fn call<R: Record, A: Record + Default>(
    encoders: &dyn GateEncoders,
    connection: NonZeroU64,
    operation: Operation,
    request: &R,
    memory: &mut Memory,
) -> (Result<(), Fault>, A) {
    let mut response = A::default();
    let mut bytes = vec![0; size_of::<A>()];
    let outcome = encoders.serve(
        connection,
        operation,
        wire::bytes(request),
        &mut bytes,
        memory,
    );
    if outcome.is_ok() || operation == Operation::EncoderEncode {
        response = wire::read(&bytes).unwrap();
    }
    (outcome, response)
}

/// Calls again while the answer is `Pending`, as the frontend does:
/// the outcome, the response and how many calls it took.
fn call_through<R: Record, A: Record + Default>(
    encoders: &dyn GateEncoders,
    connection: NonZeroU64,
    operation: Operation,
    request: &R,
    memory: &mut Memory,
) -> (Result<(), Fault>, A, usize) {
    for calls in 1..=1_000_000 {
        let (outcome, response) = call(encoders, connection, operation, request, memory);
        match outcome {
            Err(fault) if fault.status == Status::Pending => continue,
            outcome => return (outcome, response, calls),
        }
    }
    panic!("the call never ended");
}

fn open(
    encoders: &dyn GateEncoders,
    connection: NonZeroU64,
    token: u64,
    codec: EncoderCodec,
    memory: &mut Memory,
) -> Result<EncoderOpenResponse, Fault> {
    let (outcome, response, _) = call_through(
        encoders,
        connection,
        Operation::EncoderOpen,
        &open_request(token, codec),
        memory,
    );
    outcome.map(|()| response)
}

fn encode(
    encoders: &dyn GateEncoders,
    request: &EncoderEncodeRequest,
    memory: &mut Memory,
) -> (Result<(), Fault>, EncoderEncodeResponse, Vec<u8>) {
    let (outcome, response, _) = call_through::<_, EncoderEncodeResponse>(
        encoders,
        ONE,
        Operation::EncoderEncode,
        request,
        memory,
    );
    let at = (OUT - BASE) as usize;
    let bytes = memory.bytes[at..at + response.bytes as usize].to_vec();
    (outcome, response, bytes)
}

fn close(encoders: &dyn GateEncoders, connection: NonZeroU64, encoder: u64, memory: &mut Memory) {
    let (outcome, _) = call::<_, wire::Empty>(
        encoders,
        connection,
        Operation::EncoderClose,
        &Handle { handle: encoder },
        memory,
    );
    outcome.unwrap();
}

/// A session opens with its parameter sets, encodes an IDR, a P frame and
/// an IDR asked for from the pictures in the caller's memory, and closes,
/// giving back every buffer, the syncobj and the channel.
#[test]
fn a_session_opens_encodes_and_closes_on_the_mock_gpu() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut memory = caller_with(0);
    let (outcome, capabilities) = call::<_, EncoderCapabilitiesResponse>(
        &encoders,
        ONE,
        Operation::EncoderCapabilities,
        &wire::Empty {},
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(capabilities.codecs, 0b110);
    assert_eq!(capabilities.max_sessions, MAX_SESSIONS as u32);
    assert_eq!(capabilities.open_sessions, 0);
    let opened = open(&encoders, ONE, 7, EncoderCodec::H264, &mut memory).unwrap();
    assert_eq!(opened.parameter_sets_length, 36);
    assert_eq!((opened.coded_width, opened.coded_height), (1920, 1088));
    let sets = &memory.bytes[..36];
    assert_eq!(nal_types(sets, false), [7, 8]);
    assert!(sets.starts_with(&[0, 0, 0, 1]));
    assert!(
        encoders
            .platform()
            .logged("open for connection 1: h264 1920x1080 60 fps cbr 20000 kb/s")
    );
    let mut frames = Vec::new();
    for (frame, flags) in [(1, 0), (2, 0), (3, ENCODE_FORCE_IDR)] {
        put_picture(&mut memory, frame as u32);
        let request = encode_request(opened.encoder, frame, flags);
        let (outcome, response, bytes) = encode(&encoders, &request, &mut memory);
        outcome.unwrap();
        assert_eq!(response.frame, frame);
        assert!(bytes.starts_with(&[0, 0, 0, 1]));
        frames.push((response, bytes));
    }
    assert_eq!(frames[0].0.idr, 1);
    assert_eq!(nal_types(&frames[0].1, false), [7, 8, 5]);
    assert_eq!(frames[0].1[..36], memory.bytes[..36]);
    assert_eq!(frames[1].0.idr, 0);
    assert_eq!(nal_types(&frames[1].1, false), [1]);
    assert_eq!(frames[2].0.idr, 1);
    assert_eq!(frames[2].0.picture_index, 2);
    assert_eq!(frames[1].0.error_status, 2);
    close(&encoders, ONE, opened.encoder, &mut memory);
    assert_eq!(encoders.open_sessions(), 0);
    let closed = encoders.platform().closed.lock().unwrap();
    let (gpu, keeps_file) = &closed[0];
    assert!(!keeps_file);
    assert!(gpu.channel_freed && gpu.syncobj_destroyed);
    assert_eq!(gpu.closed.len(), gpu.buffers.len());
    assert_eq!(gpu.submissions.len(), 3);
    assert!(
        encoders
            .platform()
            .logged("closed (the frontend closed it)")
    );
}

/// The engine reads the picture the caller's memory held, stored band by
/// band as `store_nv12` stores it whole.
#[test]
fn the_engine_reads_the_callers_picture() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut memory = caller_with(5);
    let opened = open(&encoders, ONE, 1, EncoderCodec::Hevc, &mut memory).unwrap();
    assert_eq!(opened.parameter_sets_length, 95);
    let (outcome, response, bytes) = encode(
        &encoders,
        &encode_request(opened.encoder, 1, 0),
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(nal_types(&bytes, true)[..4], [32, 33, 34, 19]);
    assert_eq!(response.idr, 1);
    close(&encoders, ONE, opened.encoder, &mut memory);
    let mut closed = encoders.platform().closed.lock().unwrap();
    let (gpu, _) = &mut closed[0];
    let geometry = geometry();
    let nv12 = surface::pattern(&geometry, 5);
    let mut expected = vec![0u8; geometry.surface_bytes() as usize];
    // SAFETY: the buffer is the surface's size; the pattern fits it.
    unsafe {
        surface::store_nv12(
            expected.as_mut_ptr(),
            &geometry,
            &surface::pattern_picture(&geometry, &nv12),
        );
    }
    // The input the submission named: the reconstruction the mock copied.
    let submission = gpu.submissions[0].clone();
    let input = submission
        .chunks(2)
        .find(|pair| (pair[0] & 0xfff) << 2 == crate::nvenc::method::SET_IN_CUR_PIC)
        .map(|pair| u64::from(pair[1]) << 8)
        .unwrap();
    assert_eq!(gpu.read(input, expected.len()), expected);
}

/// With a clock that runs out the budget at each band, the open and each
/// frame take several calls, answered `Pending` between; a band out of
/// reach is `Unmapped` and goes on once mapped. The frame is the same.
#[test]
fn pending_and_unmapped_calls_go_on_where_they_stopped() {
    let platform = Platform::new(Engine::Encode);
    platform.step.store(STORE_BUDGET_NS / 2, Ordering::Relaxed);
    let encoders = Encoders::new(platform);
    let mut memory = caller_with(0);
    let (outcome, opened, calls) = call_through::<_, EncoderOpenResponse>(
        &encoders,
        ONE,
        Operation::EncoderOpen,
        &open_request(3, EncoderCodec::H264),
        &mut memory,
    );
    outcome.unwrap();
    assert!(calls > 10, "the open took {calls} calls");
    let request = encode_request(opened.encoder, 1, 0);
    // The chroma's first band is away the first time.
    memory.away = Some(request.chroma.address + 100..request.chroma.address + 200);
    let mut unmapped = 0;
    let mut pending = 0;
    let (response, bytes) = loop {
        let (outcome, response) = call::<_, EncoderEncodeResponse>(
            &encoders,
            ONE,
            Operation::EncoderEncode,
            &request,
            &mut memory,
        );
        match outcome {
            Ok(()) => {
                let at = (OUT - BASE) as usize;
                break (
                    response,
                    memory.bytes[at..at + response.bytes as usize].to_vec(),
                );
            }
            Err(fault) if fault.status == Status::Pending => pending += 1,
            Err(fault) if fault.status == Status::Unmapped => {
                unmapped += 1;
                memory.away = None;
            }
            Err(fault) => panic!("{fault}"),
        }
    };
    assert_eq!(unmapped, 1);
    assert!(pending > 3, "{pending} calls answered Pending");
    assert_eq!(nal_types(&bytes, false), [7, 8, 5]);
    assert_eq!(response.idr, 1);
    // The same frame asked again is answered again, not encoded again.
    let (outcome, again, bytes_again) = encode(&encoders, &request, &mut memory);
    outcome.unwrap();
    assert_eq!((again.picture_index, bytes_again), (0, bytes));
    close(&encoders, ONE, opened.encoder, &mut memory);
    assert_eq!(
        encoders.platform().closed.lock().unwrap()[0]
            .0
            .submissions
            .len(),
        1
    );
}

/// An output span too short gets the length and nothing written; the same
/// frame with a longer span gets the bytes.
#[test]
fn a_short_output_span_gets_the_length_then_the_frame() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    let mut request = encode_request(opened.encoder, 1, 0);
    request.out.length = 16;
    let (outcome, response, _) = call_through::<_, EncoderEncodeResponse>(
        &encoders,
        ONE,
        Operation::EncoderEncode,
        &request,
        &mut memory,
    );
    outcome.unwrap();
    assert!(response.bytes > 16);
    assert!(memory.bytes[(OUT - BASE) as usize..][..4] == [0; 4]);
    request.out.length = OUT_BYTES;
    let (outcome, again, bytes) = encode(&encoders, &request, &mut memory);
    outcome.unwrap();
    assert_eq!(again.bytes, response.bytes);
    assert!(bytes.starts_with(&[0, 0, 0, 1]));
}

/// A frame's bitrate re-templates a CBR session from that frame on.
#[test]
fn a_bitrate_change_takes_effect_at_the_next_frame() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    encode(
        &encoders,
        &encode_request(opened.encoder, 1, 0),
        &mut memory,
    )
    .0
    .unwrap();
    let mut request = encode_request(opened.encoder, 2, 0);
    request.bitrate = 8_000_000;
    let (outcome, response, _) = encode(&encoders, &request, &mut memory);
    outcome.unwrap();
    assert_eq!(response.idr, 0);
    assert!(encoders.platform().logged("bitrate 8000 kb/s from frame 2"));
    encoders.report();
    assert!(
        encoders
            .platform()
            .logged("h264 1920x1080 60 fps cbr 8000 kb/s: frames=2")
    );
}

/// No encoder for the chipset, a refused channel, full range and a third
/// session are refused; a channel nouveau refused refuses every later open.
#[test]
fn opens_are_refused_as_they_should_be() {
    let mut platform = Platform::new(Engine::Encode);
    platform.chipset = 0x166;
    let encoders = Encoders::new(platform);
    let mut memory = caller_with(0);
    let refused = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap_err();
    assert_eq!(refused.status, Status::Unsupported);
    assert_eq!(encoders.platform().closed.lock().unwrap().len(), 1);

    let mut platform = Platform::new(Engine::Encode);
    platform.refuse = vec![(crate::nvenc::CHANNEL_ALLOC, -38)];
    let encoders = Encoders::new(platform);
    let refused = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap_err();
    assert_eq!(
        (refused.status, refused.reason),
        (Status::Unsupported, "channel")
    );
    assert_eq!(
        open(&encoders, ONE, 2, EncoderCodec::H264, &mut memory)
            .unwrap_err()
            .reason,
        "nouveau refused the encoder's channel this boot"
    );
    let (outcome, _) = call::<_, EncoderCapabilitiesResponse>(
        &encoders,
        ONE,
        Operation::EncoderCapabilities,
        &wire::Empty {},
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Unsupported);
    // Each refused file went back, none kept.
    let closed = encoders.platform().closed.lock().unwrap();
    assert_eq!(closed.len(), 1);
    assert!(!closed[0].1);
    drop(closed);

    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut full = open_request(1, EncoderCodec::H264);
    full.flags = ENCODER_OPEN_FULL_RANGE;
    let (outcome, _) =
        call::<_, EncoderOpenResponse>(&encoders, ONE, Operation::EncoderOpen, &full, &mut memory);
    assert_eq!(outcome.unwrap_err().status, Status::Unsupported);
    let mut odd = open_request(2, EncoderCodec::H264);
    odd.width = 1919;
    let (outcome, _) =
        call::<_, EncoderOpenResponse>(&encoders, ONE, Operation::EncoderOpen, &odd, &mut memory);
    assert_eq!(outcome.unwrap_err().status, Status::Invalid);
    open(&encoders, ONE, 3, EncoderCodec::H264, &mut memory).unwrap();
    open(&encoders, TWO, 4, EncoderCodec::Hevc, &mut memory).unwrap();
    let third = open(&encoders, ONE, 5, EncoderCodec::H264, &mut memory).unwrap_err();
    assert_eq!(third.status, Status::Unsupported);
    // Another connection's session is not this one's.
    let (outcome, _) = call::<_, EncoderEncodeResponse>(
        &encoders,
        TWO,
        Operation::EncoderEncode,
        &encode_request(1, 1, 0),
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err(), Fault::STALE);
}

/// A frame the engine never finishes makes the session `Lost` once the
/// bound passes; the session takes no more frames, its busy channel keeps
/// its file, and the GPU opens no session again.
#[test]
fn a_hung_frame_loses_the_session_and_keeps_its_file() {
    let encoders = Encoders::new(Platform::new(Engine::HangsAfter(1)));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    encode(
        &encoders,
        &encode_request(opened.encoder, 1, 0),
        &mut memory,
    )
    .0
    .unwrap();
    let (outcome, _, calls) = call_through::<_, EncoderEncodeResponse>(
        &encoders,
        ONE,
        Operation::EncoderEncode,
        &encode_request(opened.encoder, 2, 0),
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Lost);
    assert!(calls > 100, "lost after {calls} calls");
    assert!(encoders.platform().logged("LOST at frame 2"));
    assert!(encoders.platform().logged("the channel is busy"));
    let (outcome, _) = call::<_, EncoderEncodeResponse>(
        &encoders,
        ONE,
        Operation::EncoderEncode,
        &encode_request(opened.encoder, 3, 0),
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Lost);
    assert_eq!(
        open(&encoders, TWO, 2, EncoderCodec::H264, &mut memory)
            .unwrap_err()
            .status,
        Status::Unsupported
    );
    close(&encoders, ONE, opened.encoder, &mut memory);
    let closed = encoders.platform().closed.lock().unwrap();
    let (gpu, keeps_file) = &closed[0];
    assert!(keeps_file);
    assert!(!gpu.channel_freed && gpu.closed.is_empty());
}

/// An engine that refuses a frame's output drops it, and the next frame is
/// an IDR.
#[test]
fn a_refused_output_drops_the_frame() {
    let encoders = Encoders::new(Platform::new(Engine::WrongType));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    // The IDR is an IDR slice either way; the P frame's is not a P slice.
    encode(
        &encoders,
        &encode_request(opened.encoder, 1, 0),
        &mut memory,
    )
    .0
    .unwrap();
    let (outcome, _, _) = encode(
        &encoders,
        &encode_request(opened.encoder, 2, 0),
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Dropped);
    let (outcome, response, _) = encode(
        &encoders,
        &encode_request(opened.encoder, 3, 0),
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(response.idr, 1);
    assert!(encoders.platform().logged("frame 2 dropped"));
}

/// A connection's close closes its sessions alone.
#[test]
fn a_closed_connection_closes_its_sessions() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let mut memory = caller_with(0);
    open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    let other = open(&encoders, TWO, 1, EncoderCodec::H264, &mut memory).unwrap();
    encoders.disconnect(ONE);
    assert_eq!(encoders.open_sessions(), 1);
    assert!(encoders.platform().logged("closed (its connection closed)"));
    let (outcome, _) = call::<_, EncoderEncodeResponse>(
        &encoders,
        TWO,
        Operation::EncoderEncode,
        &encode_request(other.encoder, 1, 0),
        &mut memory,
    );
    // Its first band is stored, then the call goes on, or it completes.
    assert!(outcome.is_ok() || outcome.unwrap_err().status == Status::Pending);
    encoders.close_all();
    assert_eq!(encoders.open_sessions(), 0);
    assert!(
        encoders
            .platform()
            .closed
            .lock()
            .unwrap()
            .iter()
            .all(|(gpu, keeps)| !keeps && gpu.channel_freed)
    );
}

/// The shared service: without encoders the device answers `Unsupported`;
/// with them a call through the gate reaches them, and a closed
/// connection closes its sessions.
#[test]
fn the_shared_service_hands_encoder_calls_to_its_encoders() {
    const HOST: ServiceHost = ServiceHost {
        relax: || {},
        relax_within: || true,
        stack: 0x4000,
        run: |_, call| call(),
        report: |_| {},
        note: |_| {},
        ticks: || 7,
        served: |_: Served| {},
        wake: || {},
        bundle: |_| {},
        job_waiting: || {},
    };
    let shared = || {
        SharedService::<MockBackend, MockCompiler>::new(
            Service::new(
                device_on(MockBackend::new()),
                DeviceEpoch::from_registered(NonZeroU64::MIN),
            ),
            HOST,
        )
    };
    // The record, the request and the response at the caller's.
    const RECORD: u64 = BASE + 0x100;
    const REQUEST: u64 = BASE + 0x200;
    const RESPONSE: u64 = BASE + 0x300;
    const REASON: u64 = BASE + 0x400;
    let gate_call = |service: &dyn GateService,
                     connection: NonZeroU64,
                     operation: Operation,
                     request: &[u8],
                     memory: &mut Memory| {
        let (_, response_bytes) = operation.sizes();
        memory.write(REQUEST, request).unwrap();
        let call = GateCall {
            operation: operation.raw(),
            status: Status::Closed.raw(),
            request: Span {
                address: REQUEST,
                length: request.len() as u64,
            },
            response: Span {
                address: RESPONSE,
                length: response_bytes as u64,
            },
            reason: Span {
                address: REASON,
                length: 0x100,
            },
            reason_length: 0,
        };
        memory.write(RECORD, wire::bytes(&call)).unwrap();
        service.serve(connection, RECORD, memory).unwrap();
        let mut raw = [0; size_of::<GateCall>()];
        memory.read(RECORD, &mut raw).unwrap();
        let answered: GateCall = wire::read(&raw).unwrap();
        let mut response = vec![0; response_bytes];
        memory.read(RESPONSE, &mut response).unwrap();
        (Status::from_raw(answered.status).unwrap(), response)
    };
    let mut memory = caller_with(0);
    let plain = shared();
    let connection = plain.connect().unwrap();
    let versions = wire::NegotiateRequest {
        major: wire::MAJOR,
        minor: wire::MINOR,
    };
    let (status, _) = gate_call(
        &plain,
        connection,
        Operation::Negotiate,
        wire::bytes(&versions),
        &mut memory,
    );
    assert_eq!(status, Status::Ok);
    let (status, _) = gate_call(
        &plain,
        connection,
        Operation::EncoderCapabilities,
        &[],
        &mut memory,
    );
    assert_eq!(status, Status::Unsupported);

    let served = shared();
    let encoders = Arc::new(Encoders::new(Platform::new(Engine::Encode)));
    served.set_encoders(encoders.clone() as Arc<dyn GateEncoders>);
    let connection = served.connect().unwrap();
    let (status, response) = gate_call(
        &served,
        connection,
        Operation::EncoderCapabilities,
        &[],
        &mut memory,
    );
    assert_eq!(status, Status::Ok);
    let capabilities: EncoderCapabilitiesResponse = wire::read(&response).unwrap();
    assert_eq!(capabilities.codecs, 0b110);
    let request = open_request(9, EncoderCodec::H264);
    let opened = loop {
        let (status, response) = gate_call(
            &served,
            connection,
            Operation::EncoderOpen,
            wire::bytes(&request),
            &mut memory,
        );
        match status {
            Status::Pending => continue,
            Status::Ok => break wire::read::<EncoderOpenResponse>(&response).unwrap(),
            status => panic!("{status:?}"),
        }
    };
    assert_ne!(opened.encoder, 0);
    // The device never saw the call: no object was made.
    let (status, response) = gate_call(
        &served,
        connection,
        Operation::AllocatedSize,
        &[],
        &mut memory,
    );
    // Not negotiated: the device refuses, which proves it is the device.
    assert_eq!(status, Status::Invalid);
    let _ = wire::read::<Value>(&response);
    assert_eq!(encoders.open_sessions(), 1);
    served.disconnect(connection);
    assert_eq!(encoders.open_sessions(), 0);
}

/// One frame on the timing model, as the frontend makes its calls: each
/// `Pending` sleeps the response's `retry_us` (the bundle's 200 us for 0).
/// The calls, the `Pending` hints and the longest call, in the model's ns.
fn timed_frame(
    encoders: &Encoders<Platform>,
    request: &EncoderEncodeRequest,
    memory: &mut Memory,
) -> (EncoderEncodeResponse, usize, Vec<u32>, u64) {
    let clock = encoders.platform().clock();
    let mut hints = Vec::new();
    let mut longest = 0;
    for calls in 1..=10_000 {
        let before = clock.load(Ordering::Relaxed);
        let (outcome, response) = call::<_, EncoderEncodeResponse>(
            encoders,
            ONE,
            Operation::EncoderEncode,
            request,
            memory,
        );
        longest = longest.max(clock.load(Ordering::Relaxed) - before);
        match outcome {
            Ok(()) => return (response, calls, hints, longest),
            Err(fault) if fault.status == Status::Pending => {
                hints.push(response.retry_us);
                let sleep = if response.retry_us == 0 {
                    200
                } else {
                    response.retry_us
                };
                clock.fetch_add(u64::from(sleep) * 1000, Ordering::Relaxed);
            }
            Err(fault) => panic!("{fault}"),
        }
    }
    panic!("the frame never ended");
}

/// The SYNCOBJ_WAITs a session's file was asked, once it closed.
fn asks_of(encoders: &Encoders<Platform>) -> usize {
    let closed = encoders.platform().closed.lock().unwrap();
    closed[0]
        .0
        .requests
        .iter()
        .filter(|&&r| r == SYNCOBJ_WAIT)
        .count()
}

/// The timing model, H.264 at the self-test's 2.3 ms: the job is pushed by
/// Linux run in place right after the submission, which comes in the call
/// that stores the last band, and the call waits for the frame within its
/// budget: one call a frame, no fence asked, no call past the budget. The
/// engine's measured time reaches the heartbeat line.
#[test]
fn timed_h264_frames_take_one_call_each() {
    let encoders = Encoders::new(Platform::timed(2_300_000, true));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    for frame in 1..=8 {
        let request = encode_request(opened.encoder, frame, 0);
        let (response, calls, hints, longest) = timed_frame(&encoders, &request, &mut memory);
        assert_eq!(response.frame, frame);
        assert_eq!((calls, response.calls), (1, 1), "frame {frame}: {hints:?}");
        assert!(
            longest < CALL_BUDGET_NS,
            "frame {frame}: a call of {longest} ns"
        );
        assert!(
            response.gpu_us >= 2300 && response.gpu_us < 2500,
            "{}",
            response.gpu_us
        );
    }
    encoders.report();
    assert!(encoders.platform().logged("engine_us=23"));
    assert!(encoders.platform().logged("asks=0 kicks="));
    // A platform that knows no GPU state ends no line with one.
    assert!(!encoders.platform().logged(" | gpu "));
    close(&encoders, ONE, opened.encoder, &mut memory);
    assert_eq!(asks_of(&encoders), 0);
}

/// The open and heartbeat lines end with the GPU's P-state and link where
/// the platform knows them, which the engine's time follows; a beat without
/// calls gives no line.
#[test]
fn session_lines_end_with_the_gpus_state() {
    let mut platform = Platform::timed(2_300_000, true);
    platform.state = Some("pstate=P8 pcie=gen1x16");
    let encoders = Encoders::new(platform);
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    assert!(
        encoders
            .platform()
            .logged("bytes of parameter sets | gpu pstate=P8 pcie=gen1x16")
    );
    let request = encode_request(opened.encoder, 1, 0);
    timed_frame(&encoders, &request, &mut memory);
    encoders.report();
    let lines = encoders.platform().lines.lock().unwrap().clone();
    let beat = lines
        .iter()
        .find(|line| line.contains("frames=1"))
        .expect("a heartbeat line");
    assert!(
        beat.ends_with(" kicks=1 | gpu pstate=P8 pcie=gen1x16"),
        "{beat}"
    );
    encoders.report();
    assert_eq!(encoders.platform().lines.lock().unwrap().len(), lines.len());
    close(&encoders, ONE, opened.encoder, &mut memory);
}

/// HEVC at 5.8 ms, past one call's budget: the call that submits answers
/// `Pending` at once with a hint near the engine's time, rather than spin
/// out its budget, and the call after the frontend's one sleep gets the
/// frame. Two calls and one sleep a frame.
#[test]
fn timed_hevc_frames_sleep_once() {
    let encoders = Encoders::new(Platform::timed(5_800_000, true));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::Hevc, &mut memory).unwrap();
    for frame in 1..=6 {
        let request = encode_request(opened.encoder, frame, 0);
        let (response, calls, hints, longest) = timed_frame(&encoders, &request, &mut memory);
        assert_eq!(calls, 2, "frame {frame}: {hints:?}");
        assert_eq!(response.calls, 2);
        assert!(
            hints[0] >= 5_000 && hints[0] <= 6_000,
            "frame {frame}: hint {hints:?}"
        );
        // The submitting call did not wait out its budget.
        assert!(longest < 1_000_000, "frame {frame}: a call of {longest} ns");
    }
    close(&encoders, ONE, opened.encoder, &mut memory);
    assert_eq!(asks_of(&encoders), 0);
}

/// Where Linux cannot run in place (the boot CPU), the call asks nouveau's
/// fence once right after the submission, which pushes the job; the frame
/// still takes one call.
#[test]
fn timed_frames_without_a_kick_ask_once() {
    let encoders = Encoders::new(Platform::timed(2_300_000, false));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    for frame in 1..=4 {
        let request = encode_request(opened.encoder, frame, 0);
        let (_, calls, hints, _) = timed_frame(&encoders, &request, &mut memory);
        assert_eq!(calls, 1, "frame {frame}: {hints:?}");
    }
    close(&encoders, ONE, opened.encoder, &mut memory);
    assert_eq!(asks_of(&encoders), 4);
}

/// An engine slower than the session expects: the calls wait out their
/// budgets, a late frame asks the fence once a call, the hints say "late",
/// and the measured time catches up so later frames are expected right.
#[test]
fn timed_slow_engine_is_learned() {
    let encoders = Encoders::new(Platform::timed(9_000_000, true));
    let mut memory = caller_with(0);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    let request = encode_request(opened.encoder, 1, 0);
    let (_, first_calls, first_hints, longest) = timed_frame(&encoders, &request, &mut memory);
    assert!(first_calls >= 2, "{first_hints:?}");
    assert!(first_hints.contains(&RETRY_LATE_US), "{first_hints:?}");
    assert!(
        longest <= CALL_BUDGET_NS + 100_000,
        "a call of {longest} ns"
    );
    let mut calls = 0;
    for frame in 2..=12 {
        let request = encode_request(opened.encoder, frame, 0);
        calls = timed_frame(&encoders, &request, &mut memory).1;
    }
    assert_eq!(calls, 2, "once learned, a submit and one look");
}

/// A BGRA picture through the gate stores what `store_bgra` stores whole.
#[test]
fn a_bgra_picture_reaches_the_engine_converted() {
    let encoders = Encoders::new(Platform::new(Engine::Encode));
    let geometry = geometry();
    let pitch = 4 * 1920 + 64;
    let pixels: Vec<u8> = (0..pitch * 1080)
        .map(|i| (i * 31 + i / pitch * 5) as u8)
        .collect();
    let mut memory = Memory::new((PICTURE - BASE) as usize + pixels.len());
    let at = (PICTURE - BASE) as usize;
    memory.bytes[at..at + pixels.len()].copy_from_slice(&pixels);
    let opened = open(&encoders, ONE, 1, EncoderCodec::H264, &mut memory).unwrap();
    let mut request = encode_request(opened.encoder, 1, 0);
    request.input = EncoderInput::Bgra.raw();
    request.luma = Span {
        address: PICTURE,
        length: pixels.len() as u64,
    };
    request.luma_pitch = pitch as u32;
    request.chroma = Span::default();
    request.chroma_pitch = 0;
    let (outcome, response, _) = encode(&encoders, &request, &mut memory);
    outcome.unwrap();
    assert_eq!(response.idr, 1);
    close(&encoders, ONE, opened.encoder, &mut memory);
    let mut closed = encoders.platform().closed.lock().unwrap();
    let (gpu, _) = &mut closed[0];
    let mut expected = vec![0u8; geometry.surface_bytes() as usize];
    // SAFETY: the buffer is the surface's size; the picture fits it.
    unsafe {
        surface::store_bgra(
            expected.as_mut_ptr(),
            &geometry,
            &surface::Rgb {
                pixels: &pixels,
                pitch,
                order: surface::Order::Bgra,
            },
        );
    }
    let input = gpu.submissions[0]
        .chunks(2)
        .find(|pair| (pair[0] & 0xfff) << 2 == crate::nvenc::method::SET_IN_CUR_PIC)
        .map(|pair| u64::from(pair[1]) << 8)
        .unwrap();
    assert_eq!(gpu.read(input, expected.len()), expected);
}
