use super::*;
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};

#[test]
fn requests_match_the_kernel_headers() {
    // `DRM_IOCTL_NOUVEAU_*` and DRM's own, as Linux's headers spell them, by
    // size.
    assert_eq!(size_of::<ChannelAlloc>(), 88);
    assert_eq!(size_of::<GemInfo>(), 40);
    assert_eq!(size_of::<GemNew>(), 48);
    assert_eq!(size_of::<PushbufBo>(), 40);
    assert_eq!(size_of::<PushbufPush>(), 24);
    assert_eq!(size_of::<Pushbuf>(), 64);
    assert_eq!(size_of::<VmInit>(), 16);
    assert_eq!(size_of::<VmBindOp>(), 40);
    assert_eq!(size_of::<VmBind>(), 40);
    assert_eq!(size_of::<ExecPush>(), 16);
    assert_eq!(size_of::<Exec>(), 40);
    assert_eq!(size_of::<Sync>(), 16);
    assert_eq!(size_of::<SyncobjWait>(), 40);
    assert_eq!(CHANNEL_ALLOC, 0xc058_6442);
    assert_eq!(CHANNEL_FREE, 0x4004_6443);
    assert_eq!(GROBJ_ALLOC, 0x400c_6444);
    assert_eq!(VM_INIT, 0xc010_6450);
    assert_eq!(VM_BIND, 0xc028_6451);
    assert_eq!(EXEC, 0xc028_6452);
    assert_eq!(GEM_NEW, 0xc030_6480);
    assert_eq!(GEM_PUSHBUF, 0xc040_6481);
    assert_eq!(GEM_CPU_PREP, 0x4008_6482);
    assert_eq!(GEM_CLOSE, 0x4008_6409);
    assert_eq!(SYNCOBJ_CREATE, 0xc008_64bf);
    assert_eq!(SYNCOBJ_DESTROY, 0xc008_64c0);
    assert_eq!(SYNCOBJ_WAIT, 0xc028_64c3);
}

#[test]
fn headers_encode_as_nv_push_does() {
    // NVC0_FIFO_PKHDR_SQ(4, SET_APPLICATION_ID, 1) and a three-word run.
    assert_eq!(header(4, method::SET_APPLICATION_ID, 1), 0x2001_8080);
    assert_eq!(header(4, method::SEMAPHORE_A, 3), 0x2003_8090);
    assert_eq!(header(4, method::SET_OBJECT, 1), 0x2001_8000);
    assert_eq!(header(0, 0x0304, 1), 0x2001_00c1);
}

#[test]
fn the_push_binds_names_and_releases() {
    let words = push(0x12_3456_7000, PAYLOAD);
    assert_eq!(
        words,
        [
            0x2001_8000,
            0xc7b7,
            0x2001_8080,
            1,
            0x2003_8090,
            0x12,
            0x3456_7000,
            0x4e56_454e,
            0x2001_80c1,
            0,
        ]
    );
    // Run the words as the engine would and land on the same methods.
    let methods = decode(&words);
    assert_eq!(
        methods,
        vec![
            (4, method::SET_OBJECT, ENCODER_CLASS),
            (4, method::SET_APPLICATION_ID, APPLICATION_H264),
            (4, method::SEMAPHORE_A, 0x12),
            (4, method::SEMAPHORE_B, 0x3456_7000),
            (4, method::SEMAPHORE_C, PAYLOAD),
            (4, method::SEMAPHORE_D, 0),
        ]
    );
}

#[test]
fn only_ga10x_with_an_encoder_is_probed() {
    for chipset in [0x172, 0x173, 0x174, 0x176, 0x177] {
        assert_eq!(encoder_class(chipset), Some(0xc7b7), "{chipset:#x}");
    }
    // GA100 has no NVENC; Turing, Ada and unknown chipsets are not probed.
    for chipset in [0x170, 0x166, 0x194, 0x1b2, 0x999] {
        assert_eq!(encoder_class(chipset), None, "{chipset:#x}");
    }
}

#[test]
fn the_codecs_send_nvidias_captured_controls() {
    let cbr = Rate::Cbr {
        bitrate: Rate::CAPTURED_BITRATE,
    };
    assert_eq!(Codec::H264.control(cbr), 0x1200_1303);
    assert_eq!(Codec::Hevc.control(cbr), 0x1200_1104);
    // RC mode 0, constant QP, as nvenc-test's --const-qp.
    assert_eq!(Codec::H264.control(Rate::ConstQp(25)), 0x1303);
    assert_eq!(Codec::Hevc.control(Rate::ConstQp(25)), 0x1104);
    assert_eq!(
        (Codec::H264.application(), Codec::Hevc.application()),
        (1, 3)
    );
}

/// The `(subchannel, method, data)` writes of incrementing method runs.
pub(super) fn decode(words: &[u32]) -> Vec<(u32, u32, u32)> {
    let mut methods = Vec::new();
    let mut index = 0;
    while index < words.len() {
        let header = words[index];
        assert_eq!(header >> 29, 1, "an incrementing header at {index}");
        let count = (header >> 16 & 0x1fff) as usize;
        let subchannel = header >> 13 & 7;
        let method = (header & 0xfff) << 2;
        for (offset, data) in words[index + 1..index + 1 + count].iter().enumerate() {
            methods.push((subchannel, method + 4 * offset as u32, *data));
        }
        index += 1 + count;
    }
    methods
}

/// What the mock's encoder does with a push.
#[derive(Clone, Copy, PartialEq)]
enum Engine {
    /// Releases the semaphore it is given, and the fence after it.
    Alive,
    /// Takes the methods and signals the fence, but writes nothing.
    Silent,
    /// Never gets past the push.
    Hung,
}

/// A GA106 render node of the patched nouveau, with an encoder.
struct Gpu {
    engine: Engine,
    /// Errnos to answer each request with, instead of performing it.
    refuse: Vec<(u32, i64)>,
    requests: Vec<u32>,
    /// Buffers by handle (index + 1), with their GPU addresses.
    buffers: Vec<(Box<[u32; BUFFER_BYTES / 4]>, u64)>,
    closed: Vec<u32>,
    mapped: usize,
    unmapped: usize,
    clock: u64,
    /// The fence signals after this many queries.
    fence_after: u32,
    queries: u32,
    channel_freed: bool,
}

impl Gpu {
    fn new(engine: Engine) -> Self {
        Self {
            engine,
            refuse: Vec::new(),
            requests: Vec::new(),
            buffers: Vec::new(),
            closed: Vec::new(),
            mapped: 0,
            unmapped: 0,
            clock: 1_000_000_000,
            fence_after: 3,
            queries: 0,
            channel_freed: false,
        }
    }

    fn buffer_at(&mut self, address: u64) -> Option<(&mut [u32; BUFFER_BYTES / 4], usize)> {
        self.buffers.iter_mut().find_map(|(words, base)| {
            (address >= *base && address < *base + BUFFER_BYTES as u64)
                .then(|| (&mut **words, ((address - *base) / 4) as usize))
        })
    }

    fn run(&mut self, submit: &Pushbuf) {
        // SAFETY: the probe's arrays, live through its ioctl.
        let (buffers, pushes) = unsafe {
            (
                core::slice::from_raw_parts(
                    submit.buffers as *const PushbufBo,
                    submit.nr_buffers as usize,
                ),
                core::slice::from_raw_parts(
                    submit.push as *const PushbufPush,
                    submit.nr_push as usize,
                ),
            )
        };
        assert_eq!(buffers.len(), 2);
        assert!(buffers[1].write_domains & DOMAIN_VRAM != 0);
        assert_eq!(pushes.len(), 1);
        let push = pushes[0];
        let handle = buffers[push.bo_index as usize].handle;
        let words = &self.buffers[handle as usize - 1].0[..push.length as usize / 4];
        let methods = decode(words);
        assert_eq!(methods[0], (4, method::SET_OBJECT, ENCODER_CLASS));
        assert_eq!(
            methods[1],
            (4, method::SET_APPLICATION_ID, APPLICATION_H264)
        );
        let field = |method: u32| {
            methods
                .iter()
                .find(|(_, m, _)| *m == method)
                .map(|(_, _, data)| *data)
                .unwrap()
        };
        let address =
            u64::from(field(method::SEMAPHORE_A)) << 32 | u64::from(field(method::SEMAPHORE_B));
        let payload = field(method::SEMAPHORE_C);
        assert_eq!(field(method::SEMAPHORE_D), 0);
        if self.engine == Engine::Alive {
            let (words, index) = self.buffer_at(address).expect("a semaphore in a buffer");
            words[index] = payload;
        }
    }
}

// SAFETY: the mock's mappings are its boxed buffers, kept for the test.
unsafe impl Node for Gpu {
    fn control(&mut self, request: u32, argument: *mut c_void) -> i64 {
        self.requests.push(request);
        if let Some(&(_, errno)) = self.refuse.iter().find(|(r, _)| *r == request) {
            return errno;
        }
        match request {
            CHANNEL_ALLOC => {
                // SAFETY: the request's own argument.
                let alloc = unsafe { &mut *argument.cast::<ChannelAlloc>() };
                assert_eq!(alloc.fb_ctxdma_handle, !0);
                assert_eq!(alloc.tt_ctxdma_handle, FIFO_ENGINE_NVENC);
                alloc.channel = 5;
                alloc.pushbuf_domains = DOMAIN_VRAM | DOMAIN_GART;
            }
            GROBJ_ALLOC => {
                // SAFETY: as above.
                let object = unsafe { &*argument.cast::<GrobjAlloc>() };
                assert_eq!((object.channel, object.class), (5, 0xc7b7));
            }
            GEM_NEW => {
                // SAFETY: as above.
                let new = unsafe { &mut *argument.cast::<GemNew>() };
                assert_eq!(new.info.domain, DOMAIN_VRAM | DOMAIN_MAPPABLE);
                let address = 0x1_0020_0000 + 0x10000 * self.buffers.len() as u64;
                self.buffers
                    .push((Box::new([0xdead_beef; BUFFER_BYTES / 4]), address));
                new.info.handle = self.buffers.len() as u32;
                new.info.offset = address;
                new.info.map_handle = 0x1_0000_0000 * self.buffers.len() as u64;
            }
            GEM_PUSHBUF => {
                // SAFETY: as above.
                let submit = unsafe { *argument.cast::<Pushbuf>() };
                assert_eq!(submit.channel, 5);
                self.run(&submit);
            }
            GEM_CPU_PREP => {
                self.queries += 1;
                if self.engine == Engine::Hung || self.queries < self.fence_after {
                    return -16;
                }
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
        assert_eq!(bytes, BUFFER_BYTES);
        let index = (offset / 0x1_0000_0000) as usize - 1;
        self.mapped += 1;
        Some(NonNull::from(&mut *self.buffers[index].0).cast())
    }

    fn unmap(&mut self, _: NonNull<u8>, bytes: usize) {
        assert_eq!(bytes, BUFFER_BYTES);
        self.unmapped += 1;
    }

    fn now_ns(&mut self) -> u64 {
        self.clock
    }

    fn pause(&mut self) {
        self.clock += 1_000_000;
    }
}

const BOUND_NS: u64 = 500_000_000;

fn run(gpu: &mut Gpu, chipset: u16) -> (Outcome, Vec<String>) {
    let mut lines = Vec::new();
    let outcome = probe(gpu, chipset, BOUND_NS, &mut |line| {
        lines.push(format!("{line}"))
    });
    (outcome, lines)
}

fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line.contains(text))
}

#[test]
fn a_live_encoder_releases_the_semaphore_and_everything_is_given_back() {
    let mut gpu = Gpu::new(Engine::Alive);
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(outcome, Outcome::Alive { micros: 0 });
    assert!(!outcome.keeps_channel());
    assert_eq!(
        gpu.requests,
        [
            CHANNEL_ALLOC,
            GROBJ_ALLOC,
            GEM_NEW,
            GEM_NEW,
            GEM_PUSHBUF,
            GEM_CLOSE,
            GEM_CLOSE,
            CHANNEL_FREE
        ]
    );
    assert_eq!(gpu.closed, [1, 2]);
    assert_eq!((gpu.mapped, gpu.unmapped), (2, 2));
    assert!(gpu.channel_freed);
    for text in [
        "NVENC PROBE start: chipset 0x176, class 0xc7b7, bound 500 ms",
        "NVENC PROBE channel ok: engine NVENC, channel 5",
        "NVENC PROBE class ok: 0xc7b7",
        "NVENC PROBE buffers ok: push at 0x100200000, semaphore at 0x100210000",
        "NVENC PROBE submitted: 10 words",
        "NVENC PROBE ALIVE: semaphore 0x4e56454e",
        "NVENC PROBE done: alive",
    ] {
        assert!(has(&lines, text), "{text}\n{lines:#?}");
    }
}

#[test]
fn a_hung_encoder_times_out_and_keeps_its_channel() {
    let mut gpu = Gpu::new(Engine::Hung);
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Timeout {
            value: 0,
            fence_done: false
        }
    );
    assert!(outcome.keeps_channel());
    // Bounded: about one query per pause, and the bound is 500 pauses.
    let queries = gpu.requests.iter().filter(|&&r| r == GEM_CPU_PREP).count();
    assert!((499..=501).contains(&queries), "{queries}");
    // Nothing that would make nouveau wait for the push.
    assert!(!gpu.channel_freed);
    assert!(gpu.closed.is_empty());
    assert_eq!((gpu.mapped, gpu.unmapped), (2, 2));
    assert!(has(
        &lines,
        "NVENC PROBE TIMEOUT: semaphore 0x00000000 after 500000 us, channel fence pending"
    ));
    assert!(has(&lines, "NVENC PROBE done: timeout"));
}

#[test]
fn a_silent_encoder_times_out_but_its_channel_is_freed() {
    let mut gpu = Gpu::new(Engine::Silent);
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Timeout {
            value: 0,
            fence_done: true
        }
    );
    assert!(!outcome.keeps_channel());
    // The fence is asked about until it signals, then no more.
    assert_eq!(gpu.queries, 3);
    assert!(gpu.channel_freed);
    assert_eq!(gpu.closed, [1, 2]);
    assert!(has(&lines, "NVENC PROBE fence done"));
    assert!(has(
        &lines,
        "channel fence done: the engine took the methods but wrote nothing"
    ));
}

#[test]
fn an_unpatched_nouveau_refuses_the_channel() {
    let mut gpu = Gpu::new(Engine::Alive);
    gpu.refuse.push((CHANNEL_ALLOC, -38));
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Channel,
            errno: -38
        }
    );
    assert_eq!(gpu.requests, [CHANNEL_ALLOC]);
    assert!(has(
        &lines,
        "NVENC PROBE channel refused: ENOSYS (-38): this nouveau has no NVENC case"
    ));
    assert!(has(
        &lines,
        "NVENC PROBE done: failed at channel: ENOSYS (-38)"
    ));
}

#[test]
fn a_refused_class_frees_its_channel() {
    let mut gpu = Gpu::new(Engine::Alive);
    gpu.refuse.push((GROBJ_ALLOC, -22));
    let (outcome, lines) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Class,
            errno: -22
        }
    );
    assert_eq!(gpu.requests, [CHANNEL_ALLOC, GROBJ_ALLOC, CHANNEL_FREE]);
    assert!(has(
        &lines,
        "NVENC PROBE class refused: 0xc7b7: EINVAL (-22)"
    ));
}

#[test]
fn a_refused_submit_gives_its_buffers_back() {
    let mut gpu = Gpu::new(Engine::Alive);
    gpu.refuse.push((GEM_PUSHBUF, -19));
    let (outcome, _) = run(&mut gpu, 0x176);
    assert_eq!(
        outcome,
        Outcome::Failed {
            step: Step::Submit,
            errno: -19
        }
    );
    assert_eq!(gpu.closed, [1, 2]);
    assert!(gpu.channel_freed);
}

#[test]
fn other_chipsets_are_skipped_untouched() {
    let mut gpu = Gpu::new(Engine::Alive);
    let (outcome, lines) = run(&mut gpu, 0x194);
    assert_eq!(outcome, Outcome::Skipped);
    assert!(gpu.requests.is_empty());
    assert!(has(&lines, "NVENC PROBE skipped: chipset 0x194"));
}
