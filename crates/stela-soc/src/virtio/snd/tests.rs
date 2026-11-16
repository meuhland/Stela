//! The sound device as its driver sees it: control requests, the stream's
//! states, transfers paced by a host clock the test moves, and the sinks.
use super::sink::{Capture, Counting, Dbfs, Format, Meter, RateClock, Sink, fingerprint};
use super::*;
use crate::bus::{Access, MmioDevice, Width};
use crate::virtio::mmio::Mmio;
use alloc::rc::Rc;
use core::cell::Cell;
use sha2::{Digest, Sha256};
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::{AicInput, InterruptSink, Wire};
use stela_contracts::virtio::{features, mmio::*, ring, status};

const RAM: u64 = 0x8_0000_0000;
const QUEUE_ENTRIES: u16 = 64;
/// Each queue's areas, one block per queue.
const QUEUES: u64 = RAM;
/// Each descriptor's own buffer, one slot per descriptor per queue.
const BUFFERS: u64 = RAM + 0x2_0000;
const SLOT: u64 = 0x2000;
const HZ: u64 = 24_000_000;
/// One 10 ms period at 48 kHz, stereo S16.
const PERIOD_FRAMES: u64 = 480;
const PERIOD_BYTES: u32 = 1920;
const PERIOD_TICKS: u64 = HZ / 100;

fn queue_base(index: usize) -> u64 {
    QUEUES + 0x4000 * index as u64
}

fn buffer(queue: usize, descriptor: u16) -> u64 {
    BUFFERS + (u64::from(QUEUE_ENTRIES) * queue as u64 + u64::from(descriptor)) * SLOT
}

/// A chain the driver posted: its head and where its writable bytes are.
struct Posted {
    head: u16,
    writable: u64,
}

/// The driver's side, by hand, and a host clock the test moves.
struct Bench {
    device: Snd,
    ram: Ram,
    queues: Vec<Option<Queue>>,
    next: [u16; 4],
    avail: [u16; 4],
    used: [u16; 4],
    host: Rc<Cell<u64>>,
    /// The guest's counter, which the device is stepped at.
    guest: u64,
}

impl Bench {
    fn with_sink(sink: Box<dyn Sink>) -> Self {
        let host = Rc::new(Cell::new(1_000_000));
        let clock = {
            let host = host.clone();
            Box::new(move || host.get())
        };
        let queues = (0..4)
            .map(|index| {
                let base = queue_base(index);
                Some(
                    Queue::split(
                        base,
                        base + 0x1000,
                        base + 0x2000,
                        u64::from(QUEUE_ENTRIES),
                        256,
                    )
                    .unwrap(),
                )
            })
            .collect();
        Self {
            device: Snd::new(sink, Some(clock), HZ),
            ram: Ram::new(RAM, 0x30_0000),
            queues,
            next: [0; 4],
            avail: [0; 4],
            used: [0; 4],
            host,
            guest: 500,
        }
    }

    fn new() -> Self {
        Self::with_sink(Box::new(Counting::new(HZ)))
    }

    /// Posts a chain on `queue`: `readable` buffers' bytes, then writable
    /// buffers of these sizes.
    fn post(&mut self, queue: usize, readable: &[&[u8]], writable: &[u32]) -> Posted {
        let base = queue_base(queue);
        let count = readable.len() + writable.len();
        let head = self.next[queue];
        let mut writable_at = 0;
        for index in 0..count {
            let descriptor = (head + index as u16) % QUEUE_ENTRIES;
            let next = (descriptor + 1) % QUEUE_ENTRIES;
            let address = buffer(queue, descriptor);
            let (bytes, flags) = match readable.get(index) {
                Some(data) => {
                    assert!(self.ram.write(address, data));
                    (data.len() as u32, 0)
                }
                None => {
                    let bytes = writable[index - readable.len()];
                    if writable_at == 0 {
                        writable_at = address;
                    }
                    assert!(self.ram.write(address, &vec![0xee; bytes as usize]));
                    (bytes, ring::DESC_F_WRITE)
                }
            };
            let flags = flags
                | if index + 1 < count {
                    ring::DESC_F_NEXT
                } else {
                    0
                };
            let entry = [
                address.to_le_bytes().as_slice(),
                &bytes.to_le_bytes(),
                &flags.to_le_bytes(),
                &next.to_le_bytes(),
            ]
            .concat();
            assert!(self.ram.write(base + 16 * u64::from(descriptor), &entry));
        }
        self.next[queue] = (head + count as u16) % QUEUE_ENTRIES;
        let slot = u64::from(self.avail[queue] % QUEUE_ENTRIES);
        assert!(
            self.ram
                .write(base + 0x1000 + 4 + 2 * slot, &head.to_le_bytes())
        );
        self.avail[queue] = self.avail[queue].wrapping_add(1);
        assert!(
            self.ram
                .write(base + 0x1000 + 2, &self.avail[queue].to_le_bytes())
        );
        Posted {
            head,
            writable: writable_at,
        }
    }

    fn step(&mut self) -> Result<Served, &'static str> {
        self.device
            .step(&mut self.queues, 0b1111, &mut self.ram, false, self.guest)
    }

    /// Moves both clocks by `ticks` and steps.
    fn after(&mut self, ticks: u64) {
        self.host.set(self.host.get() + ticks);
        self.guest += ticks;
        self.step().unwrap();
    }

    /// The used entries `queue` gained since the last call: (head, length).
    fn used(&mut self, queue: usize) -> Vec<(u16, u32)> {
        let base = queue_base(queue) + 0x2000;
        let mut index = [0u8; 2];
        assert!(self.ram.read(base + 2, &mut index));
        let index = u16::from_le_bytes(index);
        let mut entries = Vec::new();
        while self.used[queue] != index {
            let slot = u64::from(self.used[queue] % QUEUE_ENTRIES);
            let mut element = [0u8; 8];
            assert!(self.ram.read(base + 4 + 8 * slot, &mut element));
            entries.push((
                u32::from_le_bytes(element[..4].try_into().unwrap()) as u16,
                u32::from_le_bytes(element[4..].try_into().unwrap()),
            ));
            self.used[queue] = self.used[queue].wrapping_add(1);
        }
        entries
    }

    fn bytes(&mut self, address: u64, count: usize) -> Vec<u8> {
        let mut bytes = vec![0; count];
        assert!(self.ram.read(address, &mut bytes));
        bytes
    }

    /// A control request and its answer's status and body.
    fn request(&mut self, request: &[u8], room: u32) -> (u64, Vec<u8>) {
        let posted = self.post(0, &[request], &[room]);
        self.step().unwrap();
        let used = self.used(0);
        assert_eq!(used.len(), 1);
        assert_eq!(used[0].0, posted.head);
        let answer = self.bytes(posted.writable, used[0].1 as usize);
        (u64::from(word(&answer, 0)), answer[4..].to_vec())
    }

    fn pcm(&mut self, code: u64, stream: u32) -> u64 {
        let request = [(code as u32).to_le_bytes(), stream.to_le_bytes()].concat();
        self.request(&request, 4).0
    }

    fn set_params(
        &mut self,
        buffer: u32,
        period: u32,
        channels: u8,
        format: u64,
        rate: u64,
    ) -> u64 {
        let mut request = [(R_PCM_SET_PARAMS as u32).to_le_bytes(), 0u32.to_le_bytes()].concat();
        request.extend_from_slice(&buffer.to_le_bytes());
        request.extend_from_slice(&period.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&[channels, format as u8, rate as u8, 0]);
        self.request(&request, 4).0
    }

    /// 48 kHz stereo S16 in periods of 10 ms, prepared.
    fn prepared(&mut self) {
        assert_eq!(
            self.set_params(8 * PERIOD_BYTES, PERIOD_BYTES, 2, FMT_S16, RATE_48000),
            S_OK
        );
        assert_eq!(self.pcm(R_PCM_PREPARE, 0), S_OK);
    }

    /// A transfer on the TX queue: stream `stream`, `frames` of `pcm`.
    fn transfer(&mut self, stream: u32, pcm: &[u8]) -> Posted {
        self.post(2, &[&stream.to_le_bytes(), pcm], &[8])
    }

    /// The (status, latency_bytes) a transfer was completed with.
    fn status(&mut self, posted: &Posted) -> (u64, u32) {
        let answer = self.bytes(posted.writable, 8);
        (u64::from(word(&answer, 0)), word(&answer, 4))
    }
}

/// A period of stereo S16 frames, every sample `value`.
fn period(value: i16) -> Vec<u8> {
    value.to_le_bytes().repeat(2 * PERIOD_FRAMES as usize)
}

fn query(code: u64, start: u32, count: u32, size: u32) -> Vec<u8> {
    [
        (code as u32).to_le_bytes(),
        start.to_le_bytes(),
        count.to_le_bytes(),
        size.to_le_bytes(),
    ]
    .concat()
}

#[test]
fn the_device_is_a_sound_card_with_one_jack_stream_and_map() {
    let bench = Bench::new();
    let device = &bench.device;
    assert_eq!(device.id(), 25);
    assert_eq!(device.name(), "virtio-snd");
    assert_eq!(device.features(), 0);
    assert_eq!(device.queue_max(), &[64, 64, 256, 64]);
    assert_eq!(device.config_read(CONFIG_JACKS, 4), Some(1));
    assert_eq!(device.config_read(CONFIG_STREAMS, 4), Some(1));
    assert_eq!(device.config_read(CONFIG_CHMAPS, 4), Some(1));
    assert_eq!(device.config_read(CONFIG_STREAMS, 1), Some(1));
    assert_eq!(device.config_read(CONFIG_STREAMS + 1, 2), Some(0));
    assert_eq!(device.config_read(CONFIG_CHMAPS, 8), None);
    assert_eq!(device.config_read(CONFIG_BYTES, 1), None);
}

#[test]
fn the_info_requests_describe_the_jack_the_stream_and_the_map() {
    let mut bench = Bench::new();
    let (status, jack) = bench.request(&query(R_JACK_INFO, 0, 1, 24), 4 + 24);
    assert_eq!(status, S_OK);
    assert_eq!(word(&jack, 8), JACK_DEFCONF);
    assert_eq!(jack[16], 1, "connected");

    let (status, pcm) = bench.request(&query(R_PCM_INFO, 0, 1, 32), 4 + 32);
    assert_eq!(status, S_OK);
    assert_eq!(word(&pcm, PCM_INFO_FEATURES), 0);
    assert_eq!(
        u64::from_le_bytes(pcm[8..16].try_into().unwrap()),
        1 << 5 | 1 << 17
    );
    assert_eq!(
        u64::from_le_bytes(pcm[16..24].try_into().unwrap()),
        1 << 6 | 1 << 7
    );
    assert_eq!(&pcm[24..27], &[D_OUTPUT as u8, 2, 2]);

    let (status, map) = bench.request(&query(R_CHMAP_INFO, 0, 1, 24), 4 + 24);
    assert_eq!(status, S_OK);
    assert_eq!(&map[4..8], &[0, 2, 3, 4], "output, 2 channels, FL FR");

    // A larger size pads each item; a smaller one, an item past the last,
    // or no room for the answer are refused.
    let (status, padded) = bench.request(&query(R_PCM_INFO, 0, 1, 40), 4 + 40);
    assert_eq!((status, padded.len()), (S_OK, 40));
    assert_eq!(bench.request(&query(R_PCM_INFO, 0, 1, 31), 64).0, S_BAD_MSG);
    assert_eq!(
        bench.request(&query(R_PCM_INFO, 0, 2, 32), 128).0,
        S_BAD_MSG
    );
    assert_eq!(
        bench.request(&query(R_JACK_INFO, 1, 1, 24), 64).0,
        S_BAD_MSG
    );
    assert_eq!(bench.request(&query(R_PCM_INFO, 0, 1, 32), 20).0, S_BAD_MSG);
    assert_eq!(
        bench.request(&query(R_PCM_INFO, 0, 1, 32)[..12], 64).0,
        S_BAD_MSG
    );
    // Every other request is not supported.
    assert_eq!(bench.pcm(R_JACK_REMAP, 0), S_NOT_SUPP);
    assert_eq!(bench.pcm(0x300, 0), S_NOT_SUPP);
    assert_eq!(bench.request(&[1, 0], 4).0, S_BAD_MSG);
    let counters = bench.device.device_counters();
    assert_eq!((counters.requests, counters.refused), (12, 8));
}

#[test]
fn set_params_takes_only_what_the_stream_offers() {
    let mut bench = Bench::new();
    assert_eq!(bench.set_params(7680, 1920, 2, FMT_S16, 9), S_NOT_SUPP);
    assert_eq!(bench.set_params(7680, 1920, 2, 6, RATE_48000), S_NOT_SUPP);
    assert_eq!(
        bench.set_params(7680, 1920, 1, FMT_S16, RATE_48000),
        S_NOT_SUPP
    );
    // Periods of whole frames, a buffer of whole periods.
    assert_eq!(
        bench.set_params(7680, 1922, 2, FMT_S16, RATE_48000),
        S_BAD_MSG
    );
    assert_eq!(
        bench.set_params(7000, 1920, 2, FMT_S16, RATE_48000),
        S_BAD_MSG
    );
    assert_eq!(
        bench.set_params(1920, 3840, 2, FMT_S16, RATE_48000),
        S_BAD_MSG
    );
    assert_eq!(bench.set_params(0, 0, 2, FMT_S16, RATE_48000), S_BAD_MSG);
    assert_eq!(bench.device.state(), State::Initial);
    assert_eq!(bench.set_params(15360, 3840, 2, FMT_S32, RATE_44100), S_OK);
    let params = bench.device.params().unwrap();
    assert_eq!(params.format.frame_bytes(), 8);
    assert_eq!(params.format.rate, 44_100);
    assert_eq!(bench.set_params(15360, 1920, 2, FMT_S16, RATE_48000), S_OK);
    assert_eq!(bench.device.state(), State::ParamsSet);
    let notes = bench.device.take_notes();
    assert!(
        notes.iter().any(|note| note
            == "stream 0 PCM_SET_PARAMS: 2ch S16 48000 Hz, buffer 15360 bytes (8 periods), period \
                1920 bytes (480 frames, 10.0 ms): parameters set -> parameters set"),
        "{notes:?}"
    );
    // A stream ID past the one stream is a bad message.
    let mut request = [(R_PCM_SET_PARAMS as u32).to_le_bytes(), 1u32.to_le_bytes()].concat();
    request.extend_from_slice(&[0; 16]);
    assert_eq!(bench.request(&request, 4).0, S_BAD_MSG);
    assert_eq!(bench.pcm(R_PCM_PREPARE, 1), S_BAD_MSG);
}

#[test]
fn the_stream_moves_through_its_states_and_refuses_the_rest() {
    let mut bench = Bench::new();
    for code in [R_PCM_PREPARE, R_PCM_START, R_PCM_STOP, R_PCM_RELEASE] {
        assert_eq!(bench.pcm(code, 0), S_BAD_MSG, "{}", request_name(code));
    }
    bench.prepared();
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_BAD_MSG);
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    assert_eq!(bench.device.state(), State::Running);
    assert_eq!(
        bench.set_params(15360, 1920, 2, FMT_S16, RATE_48000),
        S_BAD_MSG
    );
    assert_eq!(bench.pcm(R_PCM_RELEASE, 0), S_BAD_MSG);
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    assert_eq!(bench.pcm(R_PCM_RELEASE, 0), S_OK);
    assert_eq!(bench.device.state(), State::Released);
    assert_eq!(bench.pcm(R_PCM_START, 0), S_BAD_MSG);
    assert_eq!(bench.pcm(R_PCM_PREPARE, 0), S_OK);
    assert_eq!(bench.device.state(), State::Prepared);
    let notes = bench.device.take_notes();
    for wanted in [
        "stream 0 PCM_PREPARE: parameters set -> prepared",
        "stream 0 PCM_START: prepared -> running",
        "stream 0 PCM_STOP: running -> stopped",
        "stream 0 PCM_START: stopped -> running",
        "stream 0 PCM_RELEASE: stopped -> released",
        "stream 0 PCM_PREPARE: released -> prepared",
        "stream 0 PCM_START refused (BAD_MSG) while released",
    ] {
        assert!(
            notes.iter().any(|note| note == wanted),
            "{wanted}: {notes:?}"
        );
    }
    // A reset forgets the stream.
    bench.device.reset();
    assert_eq!(bench.device.state(), State::Initial);
    assert_eq!(bench.device.params(), None);
}

#[test]
fn transfers_complete_at_the_host_clocks_rate_with_the_latency_queued() {
    let mut bench = Bench::new();
    bench.prepared();
    // The driver fills four periods before it starts.
    let posted: Vec<Posted> = (1..=4).map(|n| bench.transfer(0, &period(n))).collect();
    bench.step().unwrap();
    assert!(bench.used(2).is_empty(), "nothing plays before START");
    assert_eq!(bench.device.deadline(), None);
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    let start = bench.host.get();
    // The first completion is due a period of host time after START.
    assert_eq!(bench.device.deadline(), Some(bench.guest + PERIOD_TICKS));
    bench.after(PERIOD_TICKS - 1);
    assert!(bench.used(2).is_empty());
    assert_eq!(
        bench.device.sink().played().frames,
        PERIOD_FRAMES,
        "the first period went to the sink"
    );
    bench.after(1);
    assert_eq!(bench.used(2), [(posted[0].head, 8)]);
    // S_OK, and the three periods still queued.
    assert_eq!(bench.status(&posted[0]), (S_OK, 3 * PERIOD_BYTES));
    // Three more periods' time completes the rest, one each.
    for (index, posted) in posted.iter().enumerate().skip(1) {
        bench.after(PERIOD_TICKS);
        assert_eq!(bench.used(2), [(posted.head, 8)], "transfer {index}");
        let latency = (3 - index as u32) * PERIOD_BYTES;
        assert_eq!(bench.status(posted), (S_OK, latency));
    }
    assert_eq!(bench.host.get() - start, 4 * PERIOD_TICKS);
    let played = bench.device.sink().played();
    assert_eq!((played.frames, played.silent), (4 * PERIOD_FRAMES, 0));
    assert_eq!(bench.device.device_counters().underruns, 0);
    assert_eq!(bench.device.device_counters().transfers, 4);
}

#[test]
fn a_late_step_completes_every_transfer_the_clock_passed() {
    let mut bench = Bench::new();
    bench.prepared();
    let posted: Vec<Posted> = (1..=3).map(|n| bench.transfer(0, &period(n))).collect();
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.after(2 * PERIOD_TICKS + PERIOD_TICKS / 2);
    assert_eq!(bench.used(2), [(posted[0].head, 8), (posted[1].head, 8)]);
    assert_eq!(bench.device.sink().played().frames, 3 * PERIOD_FRAMES);
}

#[test]
fn the_host_clock_paces_whatever_the_guests_counter_does() {
    let mut bench = Bench::new();
    bench.prepared();
    let posted: Vec<Posted> = (1..=2).map(|n| bench.transfer(0, &period(n))).collect();
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    // The guest's counter held for most of the period (a PE serving): the
    // host's clock alone decides.
    bench.host.set(bench.host.get() + PERIOD_TICKS);
    bench.guest += 10;
    bench.step().unwrap();
    assert_eq!(bench.used(2), [(posted[0].head, 8)]);
    // The next deadline is a period of host time on, mapped onto the
    // guest's counter from this step: never before the host time.
    assert_eq!(bench.device.deadline(), Some(bench.guest + PERIOD_TICKS));
    // A guest counter that ran ahead changes nothing either.
    bench.guest += 5 * PERIOD_TICKS;
    bench.step().unwrap();
    assert!(bench.used(2).is_empty());
    bench.host.set(bench.host.get() + PERIOD_TICKS);
    bench.step().unwrap();
    assert_eq!(bench.used(2), [(posted[1].head, 8)]);
}

#[test]
fn a_starved_stream_plays_silence_and_resumes_from_the_clock() {
    let mut bench = Bench::new();
    bench.prepared();
    let first = bench.transfer(0, &period(100));
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.used(2), [(first.head, 8)]);
    // 25 ms with nothing queued: an underrun, its frames silence.
    bench.after(PERIOD_TICKS * 5 / 2);
    assert_eq!(bench.device.device_counters().underruns, 1);
    assert_eq!(bench.device.sink().played().silent, 1200);
    bench.after(PERIOD_TICKS);
    assert_eq!(
        bench.device.device_counters().underruns,
        1,
        "one underrun until data comes"
    );
    assert_eq!(bench.device.sink().played().silent, 1680);
    // The next transfer plays from where the clock is: a whole period on.
    let next = bench.transfer(0, &period(100));
    bench.step().unwrap();
    assert!(bench.used(2).is_empty());
    bench.after(PERIOD_TICKS - 1);
    assert!(bench.used(2).is_empty());
    bench.after(1);
    assert_eq!(bench.used(2), [(next.head, 8)]);
    assert_eq!(bench.status(&next), (S_OK, 0));
    // A second starvation is a second underrun.
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.device.device_counters().underruns, 2);
}

#[test]
fn stop_holds_the_queue_and_release_completes_it() {
    let mut bench = Bench::new();
    bench.prepared();
    let posted: Vec<Posted> = (1..=3).map(|n| bench.transfer(0, &period(n))).collect();
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.host.set(bench.host.get() + PERIOD_TICKS);
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    // The period the clock passed before STOP completes; the rest wait.
    assert_eq!(bench.used(2), [(posted[0].head, 8)]);
    bench.after(10 * PERIOD_TICKS);
    assert!(bench.used(2).is_empty());
    assert_eq!(bench.device.deadline(), None);
    assert_eq!(bench.device.device_counters().underruns, 0);
    // START resumes where it stopped.
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.used(2), [(posted[1].head, 8)]);
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    assert_eq!(bench.pcm(R_PCM_RELEASE, 0), S_OK);
    assert_eq!(bench.used(2), [(posted[2].head, 8)]);
    assert_eq!(bench.status(&posted[2]).0, S_OK);
}

#[test]
fn bad_transfers_are_answered_at_once_and_broken_ones_stop_the_device() {
    let mut bench = Bench::new();
    // Before PREPARE.
    let early = bench.transfer(0, &period(1));
    bench.step().unwrap();
    assert_eq!(bench.used(2), [(early.head, 8)]);
    assert_eq!(bench.status(&early), (S_BAD_MSG, 0));
    bench.prepared();
    // Another stream, a part of a frame, no frames.
    let other = bench.transfer(1, &period(1));
    let partial = bench.transfer(0, &[0; 6]);
    let empty = bench.post(2, &[&0u32.to_le_bytes()], &[8]);
    bench.step().unwrap();
    assert_eq!(bench.used(2).len(), 3);
    for posted in [&other, &partial, &empty] {
        assert_eq!(bench.status(posted).0, S_BAD_MSG);
    }
    assert_eq!(bench.device.device_counters().bad_transfers, 4);
    // A transfer without room for its status cannot be answered.
    bench.post(2, &[&0u32.to_le_bytes(), &period(1)], &[4]);
    assert_eq!(
        bench.step(),
        Err("a sound transfer without its header or status")
    );
    let mut bench = Bench::new();
    bench.post(0, &[&query(R_PCM_INFO, 0, 1, 32)], &[2]);
    assert_eq!(
        bench.step(),
        Err("a sound control request without room for its status")
    );
}

#[test]
fn the_report_line_comes_every_ten_host_seconds_while_it_plays() {
    let mut bench = Bench::new();
    bench.prepared();
    // A square wave at half scale: peak and RMS -6.0 dBFS.
    for _ in 0..4 {
        bench.transfer(0, &period(16384));
    }
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.device.take_notes();
    bench.after(4 * PERIOD_TICKS);
    assert!(bench.device.take_notes().is_empty());
    bench.after(REPORT_SECONDS * HZ);
    // The interval's RMS counts its silence: 3840 samples at -6 dB in
    // 963840 is -30 dB.
    assert_eq!(
        bench.device.take_notes(),
        ["frames=1920 peak=-6.0dBFS rms=-30.0dBFS underruns=1 silent=480000 latency_ms=0"],
    );
    // STOP logs the line too.
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    let notes = bench.device.take_notes();
    assert!(
        notes[0].starts_with("frames=1920 peak=-inf rms=-inf underruns=1"),
        "{notes:?}"
    );
}

#[test]
fn the_capture_keeps_the_first_seconds_of_sound_and_fingerprints_them_once() {
    let mut bench = Bench::with_sink(Box::new(Capture::new(HZ, 1)));
    bench.prepared();
    let periods: Vec<Vec<u8>> = (0..111u16)
        .map(|n| {
            (0..2 * PERIOD_FRAMES as u16)
                .flat_map(|sample| (sample.wrapping_mul(7).wrapping_add(n) as i16).to_le_bytes())
                .collect()
        })
        .collect();
    let expected = periods.concat();
    // A silent period first: the capture starts at the first sound. One
    // period stays queued ahead, as a driver keeps it.
    let silent = bench.transfer(0, &period(0));
    bench.transfer(0, &periods[0]);
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.used(2), [(silent.head, 8)]);
    for pcm in &periods[1..] {
        bench.transfer(0, pcm);
        bench.after(PERIOD_TICKS);
        assert_eq!(bench.used(2).len(), 1);
    }
    assert_eq!(bench.device.device_counters().underruns, 0);
    // One second: 48000 frames from the first frame not silent. The first
    // sample of the first period is 0, so its first frame is (0, 7).
    let kept = &expected[..48_000 * 4];
    let notes = bench.device.take_notes();
    let captures: Vec<&String> = notes
        .iter()
        .filter(|note| note.starts_with("capture"))
        .collect();
    assert_eq!(captures.len(), 1, "{notes:?}");
    let format = Format {
        channels: 2,
        sample_bytes: 2,
        rate: 48_000,
    };
    assert_eq!(*captures[0], fingerprint(kept, format, 1, false));
    let hash = hex::encode(Sha256::digest(kept));
    assert!(
        captures[0].starts_with(&format!(
            "capture: 48000 frames (1.000 s, 192000 bytes, 2ch S16 48000 Hz) from the first sound: \
             sha256={hash} peak="
        )),
        "{}",
        captures[0]
    );
    // Once only, even when the stream ends.
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    assert_eq!(bench.pcm(R_PCM_RELEASE, 0), S_OK);
    assert!(
        !bench
            .device
            .take_notes()
            .iter()
            .any(|note| note.starts_with("capture"))
    );
}

#[test]
fn a_stream_that_ends_early_gives_a_partial_capture() {
    let mut capture = Capture::new(HZ, 10);
    let format = Format {
        channels: 2,
        sample_bytes: 2,
        rate: 48_000,
    };
    capture.start(format, 0, 0);
    capture.play(&[0, 0, 0, 0, 1, 0, 2, 0, 3, 0, 4, 0]);
    assert_eq!(capture.kept(), &[1, 0, 2, 0, 3, 0, 4, 0]);
    capture.release();
    let notes = capture.take_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("capture (partial): 2 frames (0.000 s, 8 bytes, 2ch S16 48000 Hz) from the first sound, the stream ended before 10 s: sha256="), "{}", notes[0]);
    capture.release();
    assert!(capture.take_notes().is_empty());
    // A stream with no sound captures nothing, and says nothing.
    let mut quiet = Capture::new(HZ, 10);
    quiet.start(format, 0, 0);
    quiet.play(&[0; 64]);
    quiet.release();
    assert!(quiet.take_notes().is_empty());
}

#[test]
fn levels_are_tenths_of_a_db_below_full_scale() {
    let mut meter = Meter::default();
    meter.add(&i16::MIN.to_le_bytes().repeat(4), 2);
    assert_eq!(meter.peak_dbfs().to_string(), "0.0dBFS");
    assert_eq!(meter.rms_dbfs().to_string(), "0.0dBFS");
    let mut half = Meter::default();
    half.add(&16384i16.to_le_bytes().repeat(4), 2);
    assert_eq!(half.peak_dbfs().to_string(), "-6.0dBFS");
    // A full-scale sine: RMS 3 dB under its peak.
    let mut sine = Meter::default();
    // 48 samples a cycle, by rotating (cos, sin) through the step's own,
    // from their series: core has no sin.
    let a = core::f64::consts::TAU / 48.0;
    let a2 = a * a;
    let step_cos = 1.0 - a2 / 2.0 + a2 * a2 / 24.0 - a2 * a2 * a2 / 720.0;
    let step_sin = a * (1.0 - a2 / 6.0 + a2 * a2 / 120.0 - a2 * a2 * a2 / 5040.0);
    let (mut cos, mut sin) = (1.0f64, 0.0f64);
    let mut samples = Vec::new();
    for _ in 0..4800 {
        samples.extend_from_slice(&((sin * 2_147_483_647.0) as i32).to_le_bytes());
        (cos, sin) = (
            cos * step_cos - sin * step_sin,
            sin * step_cos + cos * step_sin,
        );
    }
    sine.add(&samples, 4);
    assert_eq!(sine.rms_dbfs().to_string(), "-3.0dBFS");
    assert_eq!(Meter::default().peak_dbfs().to_string(), "-inf");
    assert_eq!(Meter::default().rms_dbfs(), Dbfs(None));
    assert_eq!(Dbfs::of_power(1e-30), Dbfs(Some(2000)));
    let mut quiet = Meter::default();
    quiet.add_silence(10);
    assert_eq!(quiet.rms_dbfs().to_string(), "-inf");
}

#[test]
fn the_rate_clock_counts_frames_of_host_time_and_holds_while_stopped() {
    let mut clock = RateClock::new(HZ);
    assert_eq!(clock.position(5), 0);
    assert_eq!(clock.when(1), None);
    clock.start(48_000, 1000, 0);
    assert!(clock.running());
    assert_eq!(clock.position(1000 + HZ), 48_000);
    assert_eq!(clock.position(1000 + 499), 0);
    assert_eq!(clock.position(1000 + 500), 1);
    assert_eq!(clock.when(1), Some(1500));
    assert_eq!(clock.when(480), Some(1000 + PERIOD_TICKS));
    clock.stop(1000 + PERIOD_TICKS);
    assert_eq!(clock.position(u64::MAX), 480);
    clock.start(44_100, 0, 480);
    assert_eq!(clock.position(HZ), 480 + 44_100);
    assert_eq!(clock.when(480 + 44_100), Some(HZ));
    assert_eq!(clock.position(0), 480);
}

/// Through the transport: the device's next completion is the step's
/// deadline, and a step before it does nothing.
#[test]
fn the_transport_steps_the_device_at_its_deadline() {
    #[derive(Default)]
    struct Line;
    impl InterruptSink for Line {
        fn set_level(&mut self, wire: Wire, _: bool) {
            assert!(matches!(wire, Wire::Aic(_)));
        }
    }
    let host = Rc::new(Cell::new(0u64));
    let clock = {
        let host = host.clone();
        Box::new(move || host.get())
    };
    let device = Snd::new(Box::new(Counting::new(HZ)), Some(clock), HZ);
    let mut mmio = Mmio::new(Box::new(device), AicInput::new(0, 0x2a5), None);
    let mut line = Line;
    let mut write = |mmio: &mut Mmio, register: u64, value: u64| {
        let mut access = Access::new(0, 0, &mut line);
        mmio.write(register, Width::Word, value, &mut access)
            .unwrap();
    };
    write(&mut mmio, STATUS, 0);
    write(&mut mmio, STATUS, status::ACKNOWLEDGE | status::DRIVER);
    write(&mut mmio, DRIVER_FEATURES_SEL, 1);
    write(&mut mmio, DRIVER_FEATURES, features::VERSION_1 >> 32);
    write(
        &mut mmio,
        STATUS,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
    );
    write(
        &mut mmio,
        STATUS,
        status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
    );
    // Running with nothing to check or play: its first step is due now,
    // then nothing until the driver notifies.
    assert_eq!(mmio.deadline(), Some(0));
    assert!(mmio.due_at(0));
    let mut ram = Ram::new(RAM, 0x1000);
    let mut line = Line;
    let mut access = Access::new(0, 7, &mut line);
    mmio.step(&mut ram, &mut access);
    assert_eq!(mmio.deadline(), None);
    assert!(!mmio.due_at(u64::MAX));
}

/// The guest-audio ring as the device's sink: it offers 48 kHz only and
/// refuses 44.1 kHz at SET_PARAMS (the ring has no resampler), the frames
/// reach the ring as the clock reaches them, the host is asked to take
/// them, and the report names the clock's source.
#[test]
fn the_ring_sink_offers_48_khz_only_and_plays_the_stream_into_the_ring() {
    use super::sink::{GuestAudioRing, GuestRing};
    let ring = GuestRing::new();
    let mut bench = Bench::with_sink(Box::new(GuestAudioRing::new(ring.clone(), HZ, None)));
    let (status, pcm) = bench.request(&query(R_PCM_INFO, 0, 1, 32), 4 + 32);
    assert_eq!(status, S_OK);
    assert_eq!(
        u64::from_le_bytes(pcm[16..24].try_into().unwrap()),
        1 << 7,
        "48 kHz only"
    );
    assert_eq!(
        u64::from_le_bytes(pcm[8..16].try_into().unwrap()),
        1 << 5 | 1 << 17,
        "S16 and S32"
    );
    assert_eq!(
        bench.set_params(8 * 1764, 1764, 2, FMT_S16, RATE_44100),
        S_NOT_SUPP
    );
    assert_eq!(
        bench.device.take_notes(),
        [
            "stream 0 PCM_SET_PARAMS refused (NOT_SUPP): 2ch S16 44100 Hz: its sink plays 48000 \
             Hz only, with no resampler"
        ]
    );
    assert_eq!(bench.device.state(), State::Initial);
    assert_eq!(
        bench.set_params(8 * 3840, 3840, 2, FMT_S32, RATE_48000),
        S_OK
    );
    bench.prepared();
    let posted: Vec<Posted> = (1..=3)
        .map(|n| bench.transfer(0, &period(1000 * n)))
        .collect();
    assert_eq!(bench.pcm(R_PCM_START, 0), S_OK);
    assert_eq!(ring.bits(), 16);
    // The next step plays the first period into the ring, which asks the
    // host for a service turn.
    bench.after(1);
    assert_eq!(ring.queued(), 480);
    assert!(bench.device.take_host_due());
    assert!(!bench.device.take_host_due());
    // The output runs and publishes: the clock follows it from the next
    // step, and the transfers complete at its pace.
    ring.publish(0, bench.host.get(), 1920);
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.used(2), [(posted[0].head, 8)]);
    let mut out = [[0; 2]; 960];
    assert_eq!(ring.take(&mut out), 960);
    assert_eq!(out[0], [1000 << 16, 1000 << 16]);
    assert_eq!(out[480], [2000 << 16, 2000 << 16]);
    let notes = bench.device.take_notes();
    assert!(
        notes
            .iter()
            .any(|note| note.starts_with("clock: hdmi-frames from frame ")),
        "{notes:?}"
    );
    // The output consumed a period: the second transfer completes.
    ring.publish(480, bench.host.get() + PERIOD_TICKS, 1920);
    bench.after(PERIOD_TICKS);
    assert_eq!(bench.used(2), [(posted[1].head, 8)]);
    assert_eq!(bench.pcm(R_PCM_STOP, 0), S_OK);
    let notes = bench.device.take_notes();
    assert!(notes[0].starts_with("frames=1440 "), "{notes:?}");
    assert!(notes[0].ends_with(" clock=hdmi-frames"), "{notes:?}");
    assert_eq!(ring.bits(), 0);
    let counters = bench.device.counters();
    assert!(counters.contains(&("ring_pushed", 1440)), "{counters:?}");
    assert!(counters.contains(&("clock_handoffs", 1)), "{counters:?}");
}
