//! The gate's decoders on the mock GPU (`crate::nvdec::mock`): every
//! captured clip opened from its record, fed sample by sample and shown
//! into the caller's destinations end to end; destinations running out,
//! pages out of reach, budgets, a hung engine, refusals, limits, and
//! sessions closed with their connection.
extern crate std;

use super::*;
use crate::metal::service::wire::{Record, Value};
use crate::nvdec::layout::Layout;
use crate::nvdec::mock::{self, Engine, Gpu};
use crate::nvdec::stream::{self, Stream};
use crate::nvdec::surface::Tiling;
use crate::nvdec::{H264_CLIP, HEVC_CLIP, Picture, captured};
use crate::nvenc::layout::Geometry;
use alloc::{string::String, vec, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Mutex;

/// GA106, the RTX 3050's chipset.
const GA106: u16 = 0x176;
const ONE: NonZeroU64 = NonZeroU64::MIN;
const TWO: NonZeroU64 = NonZeroU64::new(2).unwrap();

/// A GPU of mock render node files.
struct Platform {
    engine: Engine,
    /// The pictures the next files' engine writes: pitch and rows.
    size: Mutex<(u32, u32)>,
    clock: AtomicU64,
    step: AtomicU64,
    opened: AtomicU32,
    closed: Mutex<Vec<(Gpu, bool)>>,
    lines: Mutex<Vec<String>>,
    /// The host's heap the platform reports: none unless a test sets it.
    heap: Mutex<Option<HostHeap>>,
}

impl Platform {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            size: Mutex::new((1920, 1088)),
            clock: AtomicU64::new(1_000_000_000),
            step: AtomicU64::new(1000),
            opened: AtomicU32::new(0),
            closed: Mutex::new(Vec::new()),
            lines: Mutex::new(Vec::new()),
            heap: Mutex::new(None),
        }
    }

    fn set_heap(&self, used: u64, largest: u64) {
        *self.heap.lock().unwrap() = Some(HostHeap {
            total: 4 << 30,
            used,
            largest,
        });
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
        GA106
    }

    fn open_node(&self) -> Result<Gpu, &'static str> {
        self.opened.fetch_add(1, Ordering::Relaxed);
        let (pitch, rows) = *self.size.lock().unwrap();
        Ok(Gpu::new(self.engine).with_size(pitch, rows))
    }

    fn close_node(&self, node: Gpu, keeps_file: bool) {
        self.closed.lock().unwrap().push((node, keeps_file));
    }

    fn now_ns(&self) -> u64 {
        let step = self.step.load(Ordering::Relaxed);
        self.clock.fetch_add(step, Ordering::Relaxed) + step
    }

    fn pause(&self) {
        self.clock.fetch_add(50_000, Ordering::Relaxed);
    }

    fn relax(&self) {
        core::hint::spin_loop();
    }

    fn run(&self, body: &mut dyn FnMut()) {
        body();
    }

    fn log(&self, line: fmt::Arguments<'_>) {
        self.lines.lock().unwrap().push(alloc::format!("{line}"));
    }

    fn heap(&self) -> Option<HostHeap> {
        *self.heap.lock().unwrap()
    }
}

/// The calling task's memory: one run of bytes at [`BASE`], of which a
/// range may be out of reach.
struct Memory {
    bytes: Vec<u8>,
    away: Option<core::ops::Range<u64>>,
}

const BASE: u64 = 0x1_0000_0000;
/// Where the caller keeps the record, the sample, the destinations and the
/// pictures.
const RECORD: u64 = BASE;
const SAMPLE: u64 = BASE + 0x1_0000;
const SAMPLE_BYTES: usize = 0x40_0000;
const DESTINATIONS: u64 = SAMPLE + SAMPLE_BYTES as u64;
const PICTURES: u64 = DESTINATIONS + 0x1000;

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

    fn put(&mut self, address: u64, data: &[u8]) {
        let at = (address - BASE) as usize;
        self.bytes[at..at + data.len()].copy_from_slice(data);
    }

    fn get(&self, address: u64, length: usize) -> &[u8] {
        let at = (address - BASE) as usize;
        &self.bytes[at..at + length]
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

/// One call of `operation`: its outcome and response.
fn call<R: Record, A: Record + Default>(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    operation: Operation,
    request: &R,
    memory: &mut Memory,
) -> (Result<(), Fault>, A) {
    let mut response = A::default();
    let mut bytes = vec![0; size_of::<A>()];
    let outcome = decoders.serve(
        connection,
        operation,
        wire::bytes(request),
        &mut bytes,
        memory,
    );
    if outcome.is_ok() || operation == Operation::DecoderDecode {
        response = wire::read(&bytes).unwrap();
    }
    (outcome, response)
}

/// Calls again while the answer is `Pending`, as the frontend does.
fn call_through<R: Record, A: Record + Default>(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    operation: Operation,
    request: &R,
    memory: &mut Memory,
) -> (Result<(), Fault>, A, usize) {
    for calls in 1..=100_000 {
        let (outcome, response) = call(decoders, connection, operation, request, memory);
        match outcome {
            Err(fault) if fault.status == Status::Pending => continue,
            outcome => return (outcome, response, calls),
        }
    }
    panic!("the call never ended");
}

/// The pitch and coded rows of a stream's surfaces, which the mock writes.
fn surface_size(codec: Codec, record: &[u8]) -> (u32, u32) {
    let sequence = Stream::open(codec, record).unwrap().sequence().unwrap();
    let geometry = Geometry::new(
        sequence.coded.0.next_multiple_of(16),
        sequence.coded.1.next_multiple_of(16),
    )
    .unwrap();
    let slots = usize::from(sequence.surfaces);
    let layout = Layout::with(codec, &geometry, sequence.bit_depth, slots, slots).unwrap();
    (layout.format.pitch, geometry.coded_height)
}

fn wire_codec(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => EncoderCodec::H264.raw(),
        Codec::Hevc => EncoderCodec::Hevc.raw(),
    }
}

/// Opens a session of `codec` for `record`, put in the caller's memory.
fn open(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    token: u64,
    codec: Codec,
    record: &[u8],
    memory: &mut Memory,
) -> (Result<(), Fault>, DecoderOpenResponse) {
    open_with(decoders, connection, token, codec, record, 0, memory)
}

/// [`open`] with `DECODER_OPEN_*` `flags`.
fn open_with(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    token: u64,
    codec: Codec,
    record: &[u8],
    flags: u32,
    memory: &mut Memory,
) -> (Result<(), Fault>, DecoderOpenResponse) {
    memory.put(RECORD, record);
    let request = DecoderOpenRequest {
        token,
        codec: wire_codec(codec),
        flags,
        config: Span {
            address: RECORD,
            length: record.len() as u64,
        },
    };
    let (outcome, response, _) = call_through(
        decoders,
        connection,
        Operation::DecoderOpen,
        &request,
        memory,
    );
    (outcome, response)
}

/// The caller's pictures: `count` destinations of the session's shown size
/// at [`PICTURES`], each plane `pad` bytes wider a row than its picture.
struct Pictures {
    row_bytes: usize,
    rows: usize,
    pitch: usize,
    picture_bytes: usize,
}

impl Pictures {
    fn new(opened: &DecoderOpenResponse, pad: usize) -> Self {
        let sample = if opened.format == DecoderFormat::P010.raw() {
            2
        } else {
            1
        };
        let row_bytes = opened.display_width as usize * sample;
        let rows = opened.display_height as usize;
        let pitch = row_bytes + pad;
        Self {
            row_bytes,
            rows,
            pitch,
            picture_bytes: pitch * (rows + rows / 2),
        }
    }

    fn luma(&self, k: usize) -> u64 {
        PICTURES + (k * self.picture_bytes) as u64
    }

    fn chroma(&self, k: usize) -> u64 {
        self.luma(k) + (self.pitch * self.rows) as u64
    }

    /// Puts `count` fresh destinations in the caller's memory.
    fn destinations(&self, memory: &mut Memory, count: usize) {
        for k in 0..count {
            let destination = DecoderPlanes {
                luma: Span {
                    address: self.luma(k),
                    length: (self.pitch * self.rows) as u64,
                },
                chroma: Span {
                    address: self.chroma(k),
                    length: (self.pitch * (self.rows / 2)) as u64,
                },
                luma_pitch: self.pitch as u32,
                chroma_pitch: self.pitch as u32,
                ..DecoderPlanes::default()
            };
            memory.put(
                DESTINATIONS + (k * size_of::<DecoderPlanes>()) as u64,
                wire::bytes(&destination),
            );
        }
    }

    fn destination(&self, memory: &Memory, k: usize) -> DecoderPlanes {
        wire::read(memory.get(
            DESTINATIONS + (k * size_of::<DecoderPlanes>()) as u64,
            size_of::<DecoderPlanes>(),
        ))
        .unwrap()
    }

    /// Destination `k`'s picture, its rows packed.
    fn picture(&self, memory: &Memory, k: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 0..self.rows {
            out.extend_from_slice(
                memory.get(self.luma(k) + (row * self.pitch) as u64, self.row_bytes),
            );
        }
        for row in 0..self.rows / 2 {
            out.extend_from_slice(
                memory.get(self.chroma(k) + (row * self.pitch) as u64, self.row_bytes),
            );
        }
        out
    }
}

/// The displayed picture the mock decodes as its `index`th, with the
/// stream's crop (origin 0, 0 in every captured clip).
fn expected(size: (u32, u32), pictures: &Pictures, index: u32) -> Vec<u8> {
    let (luma, chroma) = mock::picture(index, size.0, size.1);
    let pitch = size.0 as usize;
    let mut out = Vec::new();
    for y in 0..pictures.rows {
        out.extend_from_slice(&luma[y * pitch..][..pictures.row_bytes]);
    }
    for y in 0..pictures.rows / 2 {
        out.extend_from_slice(&chroma[y * pitch..][..pictures.row_bytes]);
    }
    out
}

fn decode_request(
    decoder: u64,
    frame: u64,
    sample_bytes: usize,
    count: u32,
    flags: u32,
) -> DecoderDecodeRequest {
    DecoderDecodeRequest {
        decoder,
        frame,
        sample: Span {
            address: SAMPLE,
            length: sample_bytes as u64,
        },
        destinations: Span {
            address: DESTINATIONS,
            length: u64::from(count) * size_of::<DecoderPlanes>() as u64,
        },
        count,
        flags,
    }
}

/// What a stream showed: each picture's frame token and bytes' check.
struct Shown {
    frames: Vec<u64>,
    calls: usize,
}

/// Decodes `samples` through the gate as StelaVD does, `count`
/// destinations a call, then flushes; each picture shown is checked against
/// what the mock decoded for it.
fn decode_all(
    decoders: &dyn GateDecoders,
    connection: NonZeroU64,
    opened: &DecoderOpenResponse,
    samples: &[Vec<u8>],
    size: (u32, u32),
    count: u32,
    memory: &mut Memory,
) -> Shown {
    let pictures = Pictures::new(opened, 64);
    let mut shown = Shown {
        frames: Vec::new(),
        calls: 0,
    };
    // The decode index of each frame that held a picture.
    let mut decoded_of: Vec<(u64, u32)> = Vec::new();
    let mut decodes = 0u32;
    let last = samples.len();
    for n in 0..=last {
        let frame = 100 + n as u64;
        let (bytes, flags) = match samples.get(n) {
            Some(sample) => {
                memory.put(SAMPLE, sample);
                (sample.len(), 0)
            }
            None => (0, DECODE_FLUSH),
        };
        loop {
            pictures.destinations(memory, count as usize);
            let request = decode_request(opened.decoder, frame, bytes, count, flags);
            let (outcome, response, calls) = call_through::<_, DecoderDecodeResponse>(
                decoders,
                connection,
                Operation::DecoderDecode,
                &request,
                memory,
            );
            shown.calls += calls;
            outcome.unwrap_or_else(|fault| panic!("frame {frame}: {fault}"));
            assert_eq!(response.frame, frame);
            for k in 0..response.outputs as usize {
                let destination = pictures.destination(memory, k);
                assert_ne!(destination.flags & DESTINATION_WRITTEN, 0);
                assert_eq!(destination.flags & DESTINATION_ERRORS, 0);
                let index = decoded_of
                    .iter()
                    .find(|(f, _)| *f == destination.frame)
                    .map(|(_, d)| *d)
                    .unwrap_or_else(|| {
                        // Shown by the frame decoding it, in its own call.
                        assert_eq!(destination.frame, frame);
                        decodes
                    });
                assert!(
                    pictures.picture(memory, k) == expected(size, &pictures, index),
                    "frame {frame} destination {k}: decode {index} differs"
                );
                shown.frames.push(destination.frame);
            }
            if response.taken == 1 {
                if response.decoded == 1 {
                    decoded_of.push((frame, decodes));
                    decodes += 1;
                }
                if response.queued == 0 || bytes != 0 {
                    break;
                }
            } else {
                assert!(response.queued > 0, "not taken with nothing waiting");
            }
        }
    }
    shown
}

/// Every captured clip (H.264 with B-frames, CQM and CAVLC slices; HEVC
/// with B-frames, slices, Main 10 4K) through the gate: opened from its
/// record, each sample decoded in one call or a few, each picture shown
/// once into the caller's destinations, in the stream's display order,
/// with the frame of the sample that decoded it, its bytes the picture the
/// engine wrote, cropped and at the caller's pitch.
#[test]
fn every_captured_clip_decodes_through_the_gate() {
    for (name, codec, clip) in captured::clips() {
        let (record, samples) = stream::samples(codec, &clip).unwrap();
        let size = surface_size(codec, &record);
        let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
        *platform.size.lock().unwrap() = size;
        let decoders = Decoders::new(platform);
        let opened = Pictures::new(
            &DecoderOpenResponse {
                display_width: 4096,
                display_height: 2176,
                format: DecoderFormat::P010.raw(),
                ..DecoderOpenResponse::default()
            },
            64,
        );
        let mut memory = Memory::new((PICTURES - BASE) as usize + 3 * opened.picture_bytes);
        let (outcome, opened) = open(&decoders, ONE, 1, codec, &record, &mut memory);
        outcome.unwrap_or_else(|fault| panic!("{name}: {fault}"));
        let ten = name.contains("main10");
        assert_eq!(
            opened.format,
            if ten {
                DecoderFormat::P010.raw()
            } else {
                DecoderFormat::Nv12.raw()
            },
            "{name}"
        );
        assert_eq!(opened.bit_depth, if ten { 10 } else { 8 });
        // The order Picture::all shows the clip in: decode indices.
        let order: Vec<u64> = Picture::all(codec, &clip)
            .unwrap()
            .iter()
            .flat_map(|p| p.outputs.iter().map(|o| u64::from(o.decoded)))
            .collect();
        let shown = decode_all(&decoders, ONE, &opened, &samples, size, 3, &mut memory);
        // Every sample of the captured clips holds a picture, so decode n
        // is frame 100 + n.
        let frames: Vec<u64> = order.iter().map(|d| 100 + d).collect();
        assert_eq!(shown.frames, frames, "{name}: display order");
        let platform = decoders.platform();
        assert!(
            platform.logged("NVDEC SESSION GPU 0 1 open for connection 1: "),
            "{name}"
        );
        decoders.report();
        assert!(platform.logged(&alloc::format!("NVDEC SESSION GPU 0 1 {} ", codec.name())));
        assert!(platform.logged(&alloc::format!(
            "frames={} outputs={}",
            order.len(),
            order.len()
        )));
        let (outcome, _) = call::<_, wire::Empty>(
            &decoders,
            ONE,
            Operation::DecoderClose,
            &Handle {
                handle: opened.decoder,
            },
            &mut memory,
        );
        outcome.unwrap();
        let closed = platform.closed.lock().unwrap();
        let (gpu, keeps) = closed.last().unwrap();
        assert!(!keeps && gpu.channel_freed && gpu.ce_freed, "{name}");
        assert!(platform.logged("NVDEC SESSION GPU 0 1 closed (the frontend closed it)"));
    }
}

/// One destination a call: a decode whose sample shows more pictures than
/// the call holds fills what it can and answers `taken` 0; the frontend
/// calls again with the same sample, and nothing is lost or shown twice.
#[test]
fn a_call_short_of_destinations_takes_its_sample_later() {
    let (_, codec, clip) = captured::clips()
        .into_iter()
        .find(|(name, ..)| *name == "hevc-main-bf-1080p")
        .unwrap();
    let (record, samples) = stream::samples(codec, &clip).unwrap();
    let size = surface_size(codec, &record);
    let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
    *platform.size.lock().unwrap() = size;
    let decoders = Decoders::new(platform);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 1984 * 1080 * 3 / 2 + 0x10_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, codec, &record, &mut memory);
    outcome.unwrap();
    let order: Vec<u64> = Picture::all(codec, &clip)
        .unwrap()
        .iter()
        .flat_map(|p| p.outputs.iter().map(|o| 100 + u64::from(o.decoded)))
        .collect();
    let shown = decode_all(&decoders, ONE, &opened, &samples, size, 1, &mut memory);
    assert_eq!(shown.frames, order);
    // With no destination at all, a waiting picture keeps the sample out.
    let pictures = Pictures::new(&opened, 0);
    memory.put(SAMPLE, &samples[0]);
    let request = decode_request(opened.decoder, 7, samples[0].len(), 0, DECODE_RESET);
    let (outcome, response, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(
        (response.taken, response.decoded),
        (1, 1),
        "a reset empties the queue first"
    );
    let request = decode_request(opened.decoder, 8, samples[1].len(), 0, 0);
    memory.put(SAMPLE, &samples[1]);
    let (outcome, response, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    outcome.unwrap();
    if response.queued > 0 {
        assert_eq!(response.taken, 0);
    }
    let _ = pictures;
}

/// A destination page out of the caller's reach answers `Unmapped`, as the
/// encoders' spans do; the call made again once it is back goes on where it
/// stopped, and the picture comes whole.
#[test]
fn an_unmapped_destination_goes_on_when_called_again() {
    let (record, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
    let decoders = Decoders::new(platform);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 0x40_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, Codec::H264, &record, &mut memory);
    outcome.unwrap();
    let pictures = Pictures::new(&opened, 0);
    pictures.destinations(&mut memory, 1);
    memory.put(SAMPLE, &samples[0]);
    // The chroma plane's first page is away.
    let away = pictures.chroma(0);
    memory.away = Some(away..away + 0x1000);
    let request = decode_request(opened.decoder, 1, samples[0].len(), 1, 0);
    let (outcome, _, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Unmapped);
    memory.away = None;
    let (outcome, response, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!((response.taken, response.outputs), (1, 1));
    assert_eq!(pictures.destination(&memory, 0).frame, 1);
    assert!(pictures.picture(&memory, 0) == expected((1920, 1088), &pictures, 0));
    // One sample decoded, once.
    let gpu_submissions = {
        decoders.disconnect(ONE);
        let closed = decoders.platform().closed.lock().unwrap();
        closed.last().unwrap().0.submissions.len()
    };
    assert_eq!(gpu_submissions, 1);
}

/// A call that runs out of its budget answers `Pending` with `retry_us`,
/// and the frontend's next call goes on: the frame still decodes whole.
#[test]
fn a_spent_budget_answers_pending_and_goes_on() {
    let (record, samples) = stream::samples(Codec::Hevc, HEVC_CLIP).unwrap();
    let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
    // Every reading of the clock moves it 0.5 ms: a call's budget is a
    // handful of readings.
    platform.step.store(500_000, Ordering::Relaxed);
    let decoders = Decoders::new(platform);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 0x40_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, Codec::Hevc, &record, &mut memory);
    outcome.unwrap();
    let shown = decode_all(
        &decoders,
        ONE,
        &opened,
        &samples[..4],
        (1920, 1088),
        2,
        &mut memory,
    );
    assert_eq!(shown.frames, [100, 101, 102, 103]);
    assert!(shown.calls > 8, "{} calls", shown.calls);
    // A pending answer carries its retry.
    pictures_pending(&decoders, &opened, &samples[4], &mut memory);
}

fn pictures_pending(
    decoders: &Decoders<Platform>,
    opened: &DecoderOpenResponse,
    sample: &[u8],
    memory: &mut Memory,
) {
    let pictures = Pictures::new(opened, 0);
    pictures.destinations(memory, 2);
    memory.put(SAMPLE, sample);
    let request = decode_request(opened.decoder, 500, sample.len(), 2, 0);
    let (outcome, response) =
        call::<_, DecoderDecodeResponse>(decoders, ONE, Operation::DecoderDecode, &request, memory);
    if let Err(fault) = outcome {
        assert_eq!(fault.status, Status::Pending);
        assert!(response.retry_us > 0 && response.calls == 1);
        assert_eq!(response.frame, 500);
    }
}

/// A decoder that never releases its picture is lost within the bound: the
/// session takes no more samples, its file stays open with the busy
/// channel when it closes, and no session opens on the GPU again.
#[test]
fn a_hung_decoder_is_lost_and_the_gpu_refuses_new_sessions() {
    let (record, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let decoders = Decoders::new(Platform::new(Engine::HangsAfter(1)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 0x40_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, Codec::H264, &record, &mut memory);
    outcome.unwrap();
    let shown = decode_all(
        &decoders,
        ONE,
        &opened,
        &samples[..1],
        (1920, 1088),
        2,
        &mut memory,
    );
    assert_eq!(shown.frames, [100]);
    let pictures = Pictures::new(&opened, 0);
    pictures.destinations(&mut memory, 2);
    memory.put(SAMPLE, &samples[1]);
    let request = decode_request(opened.decoder, 201, samples[1].len(), 2, 0);
    let (outcome, _, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Lost);
    let again = decode_request(opened.decoder, 202, 0, 2, 0);
    let (outcome, _) = call::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &again,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Lost);
    assert!(decoders.platform().logged("NVDEC SESSION GPU 0 1 LOST"));
    let (outcome, _) = call::<_, DecoderCapabilitiesResponse>(
        &decoders,
        ONE,
        Operation::DecoderCapabilities,
        &wire::Empty {},
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Unsupported);
    decoders.disconnect(ONE);
    let closed = decoders.platform().closed.lock().unwrap();
    assert!(
        closed.last().unwrap().1,
        "the busy channel's file stays open"
    );
}

/// Capabilities, the session limit, refused streams and records, and a
/// connection's close.
#[test]
fn capabilities_limits_and_refusals() {
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let (outcome, caps) = call::<_, DecoderCapabilitiesResponse>(
        &decoders,
        ONE,
        Operation::DecoderCapabilities,
        &wire::Empty {},
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(caps.codecs, 0b110);
    assert_eq!((caps.profiles_h264, caps.profiles_hevc), (0b111, 0b11));
    assert_eq!((caps.depths, caps.formats), (1 << 8 | 1 << 10, 0b11));
    assert_eq!(
        (caps.max_sessions, caps.open_sessions),
        (MAX_SESSIONS as u32, 0)
    );
    assert_eq!(
        (caps.max_width, caps.min_width, caps.max_destinations),
        (4096, 48, 16)
    );
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let (hevc, _) = stream::samples(Codec::Hevc, HEVC_CLIP).unwrap();
    let (first, a) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    let (second, b) = open(&decoders, TWO, 1, Codec::Hevc, &hevc, &mut memory);
    first.unwrap();
    second.unwrap();
    assert_ne!(a.decoder, b.decoder);
    // A client may hold several: up to MAX_SESSIONS, then refused, with
    // the sessions open logged.
    let mut more = Vec::new();
    for token in 2..MAX_SESSIONS as u64 {
        let (outcome, opened) = open(&decoders, ONE, token, Codec::H264, &h264, &mut memory);
        outcome.unwrap();
        more.push(opened.decoder);
    }
    let (one_more, _) = open(&decoders, ONE, 100, Codec::H264, &h264, &mut memory);
    assert_eq!(one_more.unwrap_err().status, Status::Unsupported);
    assert!(
        decoders
            .platform()
            .logged("8 sessions open (1 for connection 1, ")
    );
    for decoder in more {
        let (outcome, _) = call::<_, wire::Empty>(
            &decoders,
            ONE,
            Operation::DecoderClose,
            &Handle { handle: decoder },
            &mut memory,
        );
        outcome.unwrap();
    }
    assert_eq!(decoders.open_sessions(), 2);
    // Another connection cannot use a session it does not hold.
    let request = decode_request(a.decoder, 1, 0, 0, 0);
    let (outcome, _) = call::<_, DecoderDecodeResponse>(
        &decoders,
        TWO,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::StaleHandle);
    decoders.disconnect(TWO);
    assert_eq!(decoders.open_sessions(), 1);
    // A record that is not one is a stream the host does not take; a
    // record of the other codec too. Neither leaves a file open.
    let opened_before = decoders.platform().opened.load(Ordering::Relaxed);
    let closed_before = decoders.platform().closed.lock().unwrap().len() as u32;
    let (refused, _) = open(
        &decoders,
        TWO,
        3,
        Codec::H264,
        &[1, 100, 0, 40, 0xff, 0xe0, 0],
        &mut memory,
    );
    assert_eq!(refused.unwrap_err().status, Status::Unsupported);
    let (refused, _) = open(&decoders, TWO, 4, Codec::Hevc, &h264, &mut memory);
    assert_eq!(refused.unwrap_err().status, Status::Unsupported);
    let platform = decoders.platform();
    assert_eq!(
        platform.opened.load(Ordering::Relaxed) - opened_before,
        platform.closed.lock().unwrap().len() as u32 - closed_before,
        "every refused open gave its file back"
    );
    assert!(platform.logged("refused a h264 stream for connection 2"));
    // Bad flags and too many destinations are refused before any work.
    let request = DecoderDecodeRequest {
        flags: 4,
        ..decode_request(a.decoder, 1, 0, 0, 0)
    };
    let (outcome, _) = call::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Malformed);
    let request = decode_request(a.decoder, 1, 0, 17, 0);
    let (outcome, _) = call::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Invalid);
    decoders.close_all();
    assert_eq!(decoders.open_sessions(), 0);
    assert!(platform.logged("closed (the PC stops)"));
}

/// The shared service hands the decoder operations to its decoders and the
/// encoders' to its encoders; a service with no decoders answers them
/// `Unsupported` with the reason.
#[test]
fn the_operation_ranges_are_apart() {
    for operation in [0x0a10, 0x0a11, 0x0a12, 0x0a13] {
        assert!(is_decoder_operation(operation));
        assert!(!super::super::encoders::is_encoder_operation(operation));
    }
    for operation in [0x0a00, 0x0a01, 0x0a02, 0x0a03] {
        assert!(!is_decoder_operation(operation));
        assert!(super::super::encoders::is_encoder_operation(operation));
    }
    assert_eq!(NONE.status, Status::Unsupported);
    assert_eq!(wire::MINOR, 1);
    let _ = Value::default();
}

/// Opened in decode order (VideoToolbox without temporal processing), a
/// B-frame stream shows each picture in the call that decodes it, its own
/// bytes, and the end of the stream shows nothing more.
#[test]
fn decode_order_shows_each_picture_with_its_own_sample() {
    for name in ["h264-high-bf-1080p", "hevc-main-bf-1080p"] {
        let (_, codec, clip) = captured::clips()
            .into_iter()
            .find(|(clip, ..)| *clip == name)
            .unwrap();
        let (record, samples) = stream::samples(codec, &clip).unwrap();
        let size = surface_size(codec, &record);
        let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
        *platform.size.lock().unwrap() = size;
        let decoders = Decoders::new(platform);
        let mut memory = Memory::new((PICTURES - BASE) as usize + 3 * 0x40_0000);
        let (outcome, opened) = open_with(
            &decoders,
            ONE,
            1,
            codec,
            &record,
            DECODER_OPEN_DECODE_ORDER,
            &mut memory,
        );
        outcome.unwrap();
        let shown = decode_all(&decoders, ONE, &opened, &samples, size, 2, &mut memory);
        let frames: Vec<u64> = (0..samples.len() as u64).map(|n| 100 + n).collect();
        assert_eq!(shown.frames, frames, "{name}");
        assert!(decoders.platform().logged("order"));
        assert!(decoders.platform().logged(" decode order"), "{name}");
    }
}

/// A seek in a session, as Aerial's loop gives it: the frontend flushes
/// (finishDelayedFrames), then feeds the stream from a CRA. The CRA's RASL
/// pictures are answered taken with no picture (StelaVD emits them dropped,
/// not as errors, and does not count them towards giving the session up),
/// and pictures decoded with references stood in for are flagged.
#[test]
fn a_seek_to_a_cra_skips_its_rasl_pictures_through_the_gate() {
    let (record, samples) = stream::synthetic::hevc_with_cra();
    let size = surface_size(Codec::Hevc, &record);
    let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
    *platform.size.lock().unwrap() = size;
    let decoders = Decoders::new(platform);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x0100_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, Codec::Hevc, &record, &mut memory);
    outcome.unwrap();
    let pictures = Pictures::new(&opened, 64);
    let mut decoded = Vec::new();
    let mut errors = 0;
    let mut shown = 0;
    // Samples 0 to 4, the flush, then from the CRA (sample 5) on, and the
    // end's flush.
    let order: Vec<Option<usize>> = (0..5)
        .map(Some)
        .chain([None])
        .chain((5..samples.len()).map(Some))
        .chain([None])
        .collect();
    for (n, which) in order.iter().enumerate() {
        let frame = 100 + n as u64;
        let (bytes, flags) = match which {
            Some(k) => {
                memory.put(SAMPLE, &samples[*k]);
                (samples[*k].len(), 0)
            }
            None => (0, DECODE_FLUSH),
        };
        loop {
            pictures.destinations(&mut memory, 3);
            let request = decode_request(opened.decoder, frame, bytes, 3, flags);
            let (outcome, response, _) = call_through::<_, DecoderDecodeResponse>(
                &decoders,
                ONE,
                Operation::DecoderDecode,
                &request,
                &mut memory,
            );
            outcome.unwrap_or_else(|fault| panic!("frame {frame}: {fault}"));
            for k in 0..response.outputs as usize {
                let destination = pictures.destination(&memory, k);
                shown += 1;
                errors += usize::from(destination.flags & DESTINATION_ERRORS != 0);
            }
            if response.taken == 1 {
                if bytes != 0 {
                    decoded.push((which.unwrap(), response.decoded));
                }
                if response.queued == 0 || bytes != 0 {
                    break;
                }
            }
        }
    }
    let skipped: Vec<usize> = decoded
        .iter()
        .filter(|(_, d)| *d == 0)
        .map(|(k, _)| *k)
        .collect();
    assert_eq!(skipped, [6, 7, 8]);
    assert_eq!(shown, samples.len() - 3);
    assert!(errors > 0, "no picture flagged");
    let platform = decoders.platform();
    assert!(platform.logged("a CRA starts afresh"));
    assert!(platform.logged("skipped (shown dropped)"));
    assert!(platform.logged("stood in for by the nearest"));
    decoders.report();
    assert!(platform.logged("skipped=3 concealed="));
    assert!(platform.logged(" restarted=1 "));
}

/// A decode of no sample and no destination: a call that only says the
/// session is in use.
fn touch(
    decoders: &Decoders<Platform>,
    connection: NonZeroU64,
    decoder: u64,
    memory: &mut Memory,
) -> Result<(), Fault> {
    let request = decode_request(decoder, 1, 0, 0, 0);
    call::<_, DecoderDecodeResponse>(
        decoders,
        connection,
        Operation::DecoderDecode,
        &request,
        memory,
    )
    .0
}

fn close_session(
    decoders: &Decoders<Platform>,
    connection: NonZeroU64,
    decoder: u64,
    memory: &mut Memory,
) -> Result<(), Fault> {
    call::<_, wire::Empty>(
        decoders,
        connection,
        Operation::DecoderClose,
        &Handle { handle: decoder },
        memory,
    )
    .0
}

/// An open past the session limit evicts the least recently called session
/// idle at least EVICT_IDLE_NS (Aerial's loops leave one behind each): its
/// calls then answer `Evicted`, its close `Ok`. Sessions not idle that long
/// are never evicted, and the open is refused as before.
#[test]
fn an_open_past_the_limit_evicts_the_least_recently_used_idle_session() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let mut ids = Vec::new();
    for token in 1..=MAX_SESSIONS as u64 {
        let (outcome, opened) = open(&decoders, ONE, token, Codec::H264, &h264, &mut memory);
        outcome.unwrap();
        ids.push(opened.decoder);
    }
    let platform = decoders.platform();
    platform
        .clock
        .fetch_add(EVICT_IDLE_NS / 2, Ordering::Relaxed);
    let (refused, _) = open(&decoders, TWO, 100, Codec::H264, &h264, &mut memory);
    assert_eq!(refused.unwrap_err().status, Status::Unsupported);
    assert!(!platform.logged("evicted"));
    // Idle long enough; the first is called again, so the second is the
    // least recently called.
    platform.clock.fetch_add(EVICT_IDLE_NS, Ordering::Relaxed);
    touch(&decoders, ONE, ids[0], &mut memory).unwrap();
    let (outcome, _) = open(&decoders, TWO, 101, Codec::H264, &h264, &mut memory);
    outcome.unwrap();
    assert!(platform.logged(&alloc::format!(
        "NVDEC SESSION GPU 0 {} evicted (idle ",
        ids[1]
    )));
    assert!(platform.logged(" ms) for a new session of connection 2"));
    assert!(platform.logged(&alloc::format!("{} closed (evicted) after ", ids[1])));
    assert_eq!(decoders.open_sessions(), MAX_SESSIONS);
    // Only that one.
    assert!(!platform.logged(&alloc::format!("{} evicted", ids[2])));
    assert_eq!(
        touch(&decoders, ONE, ids[1], &mut memory)
            .unwrap_err()
            .status,
        Status::Evicted
    );
    assert_eq!(
        touch(&decoders, TWO, ids[1], &mut memory)
            .unwrap_err()
            .status,
        Status::StaleHandle
    );
    close_session(&decoders, ONE, ids[1], &mut memory).unwrap();
    assert_eq!(
        touch(&decoders, ONE, ids[1], &mut memory)
            .unwrap_err()
            .status,
        Status::StaleHandle
    );
    touch(&decoders, ONE, ids[0], &mut memory).unwrap();
}

/// An open over the video memory budget evicts an idle session too, and a
/// fresh session decodes the stream from its start.
#[test]
fn an_open_over_the_video_memory_evicts_an_idle_session() {
    let (h264, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let one = crate::nvdec::decoder::vram_bytes(Codec::H264, &h264).unwrap();
    let decoders =
        Decoders::with_vram_budget(Platform::new(Engine::Decode(Tiling::NVDEC)), one + one / 2);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 0x40_0000);
    let (first, a) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    first.unwrap();
    decoders
        .platform()
        .clock
        .fetch_add(EVICT_IDLE_NS, Ordering::Relaxed);
    let (second, b) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    second.unwrap();
    assert!(decoders.platform().logged(&alloc::format!(
        "{} evicted (idle 10000 ms) for a new session of connection 1",
        a.decoder
    )));
    assert_eq!(
        touch(&decoders, ONE, a.decoder, &mut memory)
            .unwrap_err()
            .status,
        Status::Evicted
    );
    let shown = decode_all(
        &decoders,
        ONE,
        &b,
        &samples[..3],
        (1920, 1088),
        2,
        &mut memory,
    );
    assert_eq!(shown.frames, [100, 101, 102]);
}

/// A session idle RELEASE_IDLE_NS gives its video memory back at the next
/// decoder call, unless the call is its own.
#[test]
fn a_session_idle_a_minute_gives_its_video_memory_back() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let (_, a) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    let (_, b) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    let platform = decoders.platform();
    platform
        .clock
        .fetch_add(RELEASE_IDLE_NS / 2, Ordering::Relaxed);
    touch(&decoders, ONE, a.decoder, &mut memory).unwrap();
    assert_eq!(decoders.open_sessions(), 2);
    platform
        .clock
        .fetch_add(RELEASE_IDLE_NS / 2 + 1, Ordering::Relaxed);
    // Its own call keeps it; another's call lets it go.
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert_eq!(decoders.open_sessions(), 2);
    platform
        .clock
        .fetch_add(RELEASE_IDLE_NS / 2, Ordering::Relaxed);
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert_eq!(decoders.open_sessions(), 1);
    assert!(platform.logged(&alloc::format!(
        "{} evicted (idle 60000 ms) to give its video memory back: no call in 60 s",
        a.decoder
    )));
    assert_eq!(
        touch(&decoders, ONE, a.decoder, &mut memory)
            .unwrap_err()
            .status,
        Status::Evicted
    );
    decoders.disconnect(ONE);
    assert_eq!(
        touch(&decoders, ONE, a.decoder, &mut memory)
            .unwrap_err()
            .status,
        Status::StaleHandle
    );
}

/// A sample the parser refuses is logged with what it was: its NAL unit,
/// order count, reference picture set and the buffer then.
#[test]
fn a_refused_sample_is_logged_with_what_it_was() {
    let (record, samples) = stream::synthetic::hevc_with_cra();
    let size = surface_size(Codec::Hevc, &record);
    let platform = Platform::new(Engine::Decode(Tiling::NVDEC));
    *platform.size.lock().unwrap() = size;
    let decoders = Decoders::new(platform);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x0100_0000);
    let (outcome, opened) = open(&decoders, ONE, 1, Codec::Hevc, &record, &mut memory);
    outcome.unwrap();
    let pictures = Pictures::new(&opened, 64);
    for (n, sample) in samples[..5].iter().enumerate() {
        memory.put(SAMPLE, sample);
        pictures.destinations(&mut memory, 3);
        let request = decode_request(opened.decoder, 100 + n as u64, sample.len(), 3, 0);
        let (outcome, _, _) = call_through::<_, DecoderDecodeResponse>(
            &decoders,
            ONE,
            Operation::DecoderDecode,
            &request,
            &mut memory,
        );
        outcome.unwrap();
    }
    let bogus = stream::synthetic::hevc_p_without_references(9);
    memory.put(SAMPLE, &bogus);
    pictures.destinations(&mut memory, 3);
    let request = decode_request(opened.decoder, 900, bogus.len(), 3, 0);
    let (outcome, _, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    assert_eq!(outcome.unwrap_err().status, Status::Dropped);
    let platform = decoders.platform();
    assert!(platform.logged(
        "1 frame 900 dropped: an inter picture with no reference (invalid) [an inter picture"
    ));
    assert!(platform.logged("rps curr_before [] curr_after [] foll [5]"));
}

/// Sessions are admitted by video memory: a GPU whose budget holds one
/// session refuses a second, and logs the session open.
#[test]
fn sessions_are_admitted_by_video_memory() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let one = crate::nvdec::decoder::vram_bytes(Codec::H264, &h264).unwrap();
    let decoders =
        Decoders::with_vram_budget(Platform::new(Engine::Decode(Tiling::NVDEC)), one + one / 2);
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let (first, _) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    first.unwrap();
    let opened_before = decoders.platform().opened.load(Ordering::Relaxed);
    let (second, _) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    let fault = second.unwrap_err();
    assert_eq!(fault.status, Status::Unsupported);
    let platform = decoders.platform();
    assert_eq!(platform.opened.load(Ordering::Relaxed), opened_before);
    assert!(platform.logged(&alloc::format!(
        "it takes {} MiB of video memory, {} MiB of {} MiB are taken (1 for connection 1, ",
        one >> 20,
        one >> 20,
        (one + one / 2) >> 20
    )));
    decoders.close_all();
    let (third, _) = open(&decoders, ONE, 3, Codec::H264, &h264, &mut memory);
    third.unwrap();
}

/// Sessions are admitted by the host's heap too: an open that would leave
/// less than HEAP_FREE_LEAST free, or that finds no extent of
/// HEAP_LARGEST_LEAST, is refused before any file opens; one that fits logs
/// the host memory it holds and the heap before and after, as its close
/// does.
#[test]
fn sessions_are_admitted_by_the_host_heap() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let needs = crate::nvdec::decoder::needs(Codec::H264, &h264).unwrap();
    // 1080p H.264: four 512 KiB bitstream buffers, the push ring, the
    // fence and setup pages, the copier's ring and a 1080p NV12 staging.
    assert_eq!(
        needs.host,
        4 * 0x8_0000
            + 0x4_0000
            + 0x1000
            + 4 * 0x1000
            + 0x1_0000
            + 0x2f_7600_u64.next_multiple_of(0x1000)
    );
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let platform = decoders.platform();
    let total: u64 = 4 << 30;
    // Short of free bytes by one.
    platform.set_heap(total - (needs.host + HEAP_FREE_LEAST) + 1, 1 << 30);
    let (refused, _) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    let fault = refused.unwrap_err();
    assert_eq!(fault.status, Status::Unsupported);
    assert_eq!(fault.reason, "the host's heap is low");
    assert_eq!(platform.opened.load(Ordering::Relaxed), 0);
    assert!(platform.logged(&alloc::format!(
        "refused a h264 stream for connection 1: it takes {} MiB of host memory and the \
         host's heap is low: ",
        needs.host >> 20
    )));
    // Enough free, but in pieces.
    platform.set_heap(1 << 30, 32 << 20);
    let (refused, _) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    assert_eq!(refused.unwrap_err().status, Status::Unsupported);
    assert!(platform.logged("largest extent 32 MiB (at least 64 MiB)"));
    assert_eq!(platform.opened.load(Ordering::Relaxed), 0);
    // A heap that holds it.
    platform.set_heap(1 << 30, 1 << 30);
    let (opened, a) = open(&decoders, ONE, 3, Codec::H264, &h264, &mut memory);
    opened.unwrap();
    assert!(platform.logged(&alloc::format!(
        "{} open for connection 1: h264 1920x1080 8-bit nv12 display order",
        a.decoder
    )));
    // Without its staging, which the first picture makes.
    assert!(platform.logged(&alloc::format!(
        "MiB of video memory, 2 MiB of host memory ({} MiB with its staging), copy channel 6 \
         | heap 1024->1024M used of 4096M, largest 1024M",
        needs.host >> 20
    )));
    close_session(&decoders, ONE, a.decoder, &mut memory).unwrap();
    assert!(platform.logged(
        ", held 2 MiB of host memory (bitstream grown 0, staging let go 0) | heap \
         1024->1024M used of 4096M, largest 1024M"
    ));
}

/// The decoders' sessions take at most HOST_BUDGET of system memory
/// together: past it an open is refused, unless an idle session is
/// evicted for it.
#[test]
fn an_open_over_the_host_budget_is_refused_or_evicts() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let needs = crate::nvdec::decoder::needs(Codec::H264, &h264).unwrap();
    let decoders = Decoders::with_budgets(
        Platform::new(Engine::Decode(Tiling::NVDEC)),
        VRAM_BUDGET,
        needs.host + needs.host / 2,
    );
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let (first, a) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    first.unwrap();
    let (second, _) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    let fault = second.unwrap_err();
    assert_eq!(
        fault.reason,
        "the host memory of the GPU's decoder sessions is taken"
    );
    let platform = decoders.platform();
    assert!(platform.logged(&alloc::format!(
        "it takes {} MiB of host memory, {} MiB of {} MiB are taken (1 for connection 1, ",
        needs.host >> 20,
        needs.host >> 20,
        (needs.host + needs.host / 2) >> 20
    )));
    platform.clock.fetch_add(EVICT_IDLE_NS, Ordering::Relaxed);
    let (third, _) = open(&decoders, ONE, 3, Codec::H264, &h264, &mut memory);
    third.unwrap();
    assert!(platform.logged(&alloc::format!(
        "{} evicted (idle 10000 ms) for a new session of connection 1",
        a.decoder
    )));
}

/// While the host's heap is low, a session idle EVICT_IDLE_NS is evicted
/// at the next call of another; with a heap that is not, it stays.
#[test]
fn a_low_heap_evicts_idle_sessions() {
    let (h264, _) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 0x1000);
    let (_, a) = open(&decoders, ONE, 1, Codec::H264, &h264, &mut memory);
    let (_, b) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    let platform = decoders.platform();
    platform.set_heap(1 << 30, 1 << 30);
    platform.clock.fetch_add(EVICT_IDLE_NS, Ordering::Relaxed);
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert_eq!(decoders.open_sessions(), 2);
    platform.set_heap(1 << 30, 3 << 20);
    platform.clock.fetch_add(SWEEP_NS, Ordering::Relaxed);
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert_eq!(decoders.open_sessions(), 1);
    assert!(platform.logged(&alloc::format!(
        "{} evicted (idle 11000 ms) to give the host's heap back: 3072 MiB free, largest 3 MiB",
        a.decoder
    )));
    assert_eq!(
        touch(&decoders, ONE, a.decoder, &mut memory)
            .unwrap_err()
            .status,
        Status::Evicted
    );
}

/// A session idle TRIM_IDLE_NS lets its staging go at another's call; its
/// next picture makes it again, and is shown whole.
#[test]
fn an_idle_session_lets_its_staging_go_and_makes_it_again() {
    let (h264, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let needs = crate::nvdec::decoder::needs(Codec::H264, &h264).unwrap();
    let decoders = Decoders::new(Platform::new(Engine::Decode(Tiling::NVDEC)));
    let mut memory = Memory::new((PICTURES - BASE) as usize + 2 * 0x40_0000);
    let (outcome, a) = open_with(
        &decoders,
        ONE,
        1,
        Codec::H264,
        &h264,
        DECODER_OPEN_DECODE_ORDER,
        &mut memory,
    );
    outcome.unwrap();
    let (_, b) = open(&decoders, ONE, 2, Codec::H264, &h264, &mut memory);
    let shown = decode_all(
        &decoders,
        ONE,
        &a,
        &samples[..3],
        (1920, 1088),
        2,
        &mut memory,
    );
    assert_eq!(shown.frames, [100, 101, 102]);
    let held = |decoders: &Decoders<Platform>| {
        let mut held = decoders.hold();
        held.state().sessions[0].decoder.host_bytes()
    };
    assert_eq!(held(&decoders), needs.host);
    let platform = decoders.platform();
    platform
        .clock
        .fetch_add(TRIM_IDLE_NS / 2, Ordering::Relaxed);
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert!(!platform.logged("let its staging go"));
    platform
        .clock
        .fetch_add(TRIM_IDLE_NS / 2, Ordering::Relaxed);
    touch(&decoders, ONE, b.decoder, &mut memory).unwrap();
    assert!(platform.logged(&alloc::format!(
        "{} let its staging go (idle 3000 ms): {} -> 2 MiB of host memory",
        a.decoder,
        needs.host >> 20
    )));
    assert_eq!(held(&decoders), needs.host - 0x2f_8000);
    // The next picture, from the stream's start.
    let pictures = Pictures::new(&a, 64);
    memory.put(SAMPLE, &samples[0]);
    pictures.destinations(&mut memory, 2);
    let request = decode_request(a.decoder, 200, samples[0].len(), 2, DECODE_RESET);
    let (outcome, response, _) = call_through::<_, DecoderDecodeResponse>(
        &decoders,
        ONE,
        Operation::DecoderDecode,
        &request,
        &mut memory,
    );
    outcome.unwrap();
    assert_eq!(response.outputs, 1);
    assert!(pictures.picture(&memory, 0) == expected((1920, 1088), &pictures, 3));
    assert_eq!(held(&decoders), needs.host);
    close_session(&decoders, ONE, a.decoder, &mut memory).unwrap();
    assert!(platform.logged("(bitstream grown 0, staging let go 1)"));
}
