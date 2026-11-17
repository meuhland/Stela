//! The block device through the register page, as the kext drives it, on
//! RAM a test owns, in front of the RAM disk and of test disks that are
//! read-only, have a write cache or run their requests queued.
use super::*;
use crate::bus::{Access, MmioDevice, Width};
use crate::virtio::mmio::{Event, Mmio};
use alloc::{collections::BTreeMap, rc::Rc};
use core::cell::RefCell;
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::{AicInput, InterruptSink, Wire};
use stela_contracts::virtio::{features as transport, mmio::*, ring, status};
use stela_drivers::block::{BlockError, Command, Features, Identity, Transport};

const RAM: u64 = 0x8_0000_0000;
const IRQ: AicInput = AicInput::new(0, 0x2a4);
const DESC: u64 = RAM;
const AVAIL: u64 = RAM + 0x1000;
const USED: u64 = RAM + 0x2000;
/// Headers, statuses and data.
const BUFFERS: u64 = RAM + 0x1_0000;
const SIZE: u16 = 64;
const BLOCK: u32 = 4096;

#[derive(Default)]
struct Line {
    level: Option<bool>,
}

impl InterruptSink for Line {
    fn set_level(&mut self, wire: Wire, level: bool) {
        assert_eq!(wire, Wire::Aic(IRQ));
        self.level = Some(level);
    }
}

/// What a test disk keeps, shared with the test.
#[derive(Default)]
struct State {
    bytes: Vec<u8>,
    flushes: usize,
    /// Queued disks: commands started and not completed, by tag.
    started: BTreeMap<u32, Command>,
    completed: BTreeMap<u32, Finished>,
    next_tag: u32,
}

/// A disk of `blocks` 4 KiB blocks, read-only or with a write cache, that
/// runs its commands where asked or queued until the test completes them.
struct Disk {
    identity: Identity,
    queued: bool,
    state: Rc<RefCell<State>>,
}

impl Disk {
    fn new(blocks: u64, read_only: bool, queued: bool) -> (Self, Rc<RefCell<State>>) {
        let state = Rc::new(RefCell::new(State {
            bytes: vec![0; (blocks * u64::from(BLOCK)) as usize],
            ..State::default()
        }));
        let disk = Self {
            identity: Identity {
                transport: Transport::Ram,
                location: "test".into(),
                model: "test disk".into(),
                serial: "TESTSERIAL".into(),
                blocks,
                block_size: BLOCK,
                read_only,
                features: Features {
                    write_cache: true,
                    ..Features::default()
                },
            },
            queued,
            state: state.clone(),
        };
        (disk, state)
    }
}

impl BlockDevice for Disk {
    fn identity(&self) -> &Identity {
        &self.identity
    }

    fn read(&mut self, lba: u64, buffer: &mut [u8]) -> Result<(), BlockError> {
        self.identity.check(lba, buffer.len())?;
        let start = (lba * u64::from(BLOCK)) as usize;
        buffer.copy_from_slice(&self.state.borrow().bytes[start..start + buffer.len()]);
        Ok(())
    }

    fn write(&mut self, lba: u64, data: &[u8]) -> Result<(), BlockError> {
        self.identity.check_write(lba, data.len())?;
        let start = (lba * u64::from(BLOCK)) as usize;
        self.state.borrow_mut().bytes[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush_cache(&mut self) -> Result<(), BlockError> {
        self.state.borrow_mut().flushes += 1;
        Ok(())
    }

    fn depth(&self) -> usize {
        if self.queued { 8 } else { 0 }
    }

    fn room(&self) -> bool {
        self.queued
    }

    fn start(&mut self, command: Command) -> Result<u32, BlockError> {
        let mut state = self.state.borrow_mut();
        let tag = state.next_tag;
        state.next_tag += 1;
        state.started.insert(tag, command);
        Ok(tag)
    }

    fn finish(&mut self, tag: u32) -> Option<Finished> {
        self.state.borrow_mut().completed.remove(&tag)
    }
}

/// Completes the queued disk's command `tag`, as its hardware would.
fn complete(disk: &mut Disk, tag: u32) {
    let command = disk.state.borrow_mut().started.remove(&tag).unwrap();
    let finished = disk.execute(command);
    disk.state.borrow_mut().completed.insert(tag, finished);
}

struct Bench {
    device: Mmio,
    ram: Ram,
    line: Line,
    /// The next free descriptor, and the available index published.
    next: u16,
    avail: u16,
    /// Where the next buffer goes in `BUFFERS`.
    free: u64,
}

impl Bench {
    fn new(device: Blk) -> Self {
        Self {
            device: Mmio::new(Box::new(device), IRQ, None),
            ram: Ram::new(RAM, 0x40_0000),
            line: Line::default(),
            next: 0,
            avail: 0,
            free: BUFFERS,
        }
    }

    fn ram_disk() -> Self {
        Self::new(ram(RAM_DEFAULT_MIB).unwrap())
    }

    fn read(&mut self, register: u64) -> u32 {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device
            .read(register, Width::Word, &mut access)
            .unwrap() as u32
    }

    fn config(&mut self, offset: u64, width: Width) -> Option<u64> {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device.read(CONFIG + offset, width, &mut access).ok()
    }

    fn write(&mut self, register: u64, value: u32) {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device
            .write(register, Width::Word, u64::from(value), &mut access)
            .unwrap();
    }

    fn status(&mut self, bits: u64) {
        let now = self.read(STATUS);
        self.write(STATUS, now | bits as u32);
    }

    fn offered(&mut self) -> u64 {
        let mut offered = 0;
        for select in 0..2 {
            self.write(DEVICE_FEATURES_SEL, select);
            offered |= u64::from(self.read(DEVICE_FEATURES)) << (32 * select);
        }
        offered
    }

    /// The driver's start: reset, `accepted`, queue 0 of [`SIZE`], DRIVER_OK.
    fn start(&mut self, accepted: u64) {
        self.write(STATUS, 0);
        self.status(status::ACKNOWLEDGE | status::DRIVER);
        for select in 0..2 {
            self.write(DRIVER_FEATURES_SEL, select);
            self.write(DRIVER_FEATURES, (accepted >> (32 * select)) as u32);
        }
        self.status(status::FEATURES_OK);
        assert_ne!(self.read(STATUS) & status::FEATURES_OK as u32, 0);
        self.write(QUEUE_SEL, 0);
        self.write(QUEUE_NUM, u32::from(SIZE));
        for (low, address) in [
            (QUEUE_DESC_LOW, DESC),
            (QUEUE_AVAIL_LOW, AVAIL),
            (QUEUE_USED_LOW, USED),
        ] {
            self.write(low, address as u32);
            self.write(low + 4, (address >> 32) as u32);
        }
        self.write(QUEUE_READY, 1);
        self.status(status::DRIVER_OK);
        self.step();
    }

    fn step(&mut self) {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device.step(&mut self.ram, &mut access);
    }

    /// A buffer of `bytes` in `BUFFERS`, filled with `fill`.
    fn buffer(&mut self, bytes: u32, fill: u8) -> u64 {
        let at = self.free;
        self.ram.write(at, &vec![fill; bytes as usize]);
        self.free += u64::from(bytes).next_multiple_of(16);
        at
    }

    fn header(&mut self, kind: u64, sector: u64) -> u64 {
        let at = self.buffer(16, 0);
        let header = [
            (kind as u32).to_le_bytes().as_slice(),
            &[0; 4],
            &sector.to_le_bytes(),
        ]
        .concat();
        self.ram.write(at, &header);
        at
    }

    /// Makes `buffers` (address, bytes, device-writable) one chain and
    /// publishes it; answers its head.
    fn chain(&mut self, buffers: &[(u64, u32, bool)]) -> u16 {
        let head = self.next;
        for (index, &(address, bytes, write)) in buffers.iter().enumerate() {
            let at = (self.next + index as u16) % SIZE;
            let last = index + 1 == buffers.len();
            let flags = if write { ring::DESC_F_WRITE } else { 0 }
                | if last { 0 } else { ring::DESC_F_NEXT };
            let next = (at + 1) % SIZE;
            let descriptor = [
                address.to_le_bytes().as_slice(),
                &bytes.to_le_bytes(),
                &flags.to_le_bytes(),
                &next.to_le_bytes(),
            ]
            .concat();
            self.ram.write(DESC + 16 * u64::from(at), &descriptor);
        }
        self.next = (self.next + buffers.len() as u16) % SIZE;
        self.ram.write(
            AVAIL + 4 + 2 * u64::from(self.avail % SIZE),
            &head.to_le_bytes(),
        );
        self.avail = self.avail.wrapping_add(1);
        self.ram.write(AVAIL + 2, &self.avail.to_le_bytes());
        head
    }

    fn notify(&mut self) {
        self.write(QUEUE_NOTIFY, 0);
        self.step();
    }

    fn used_index(&mut self) -> u16 {
        let mut index = [0u8; 2];
        self.ram.read(USED + 2, &mut index);
        u16::from_le_bytes(index)
    }

    fn used(&mut self, index: u16) -> (u32, u32) {
        let mut element = [0u8; 8];
        self.ram
            .read(USED + 4 + 8 * u64::from(index % SIZE), &mut element);
        (
            u32::from_le_bytes(element[..4].try_into().unwrap()),
            u32::from_le_bytes(element[4..].try_into().unwrap()),
        )
    }

    fn byte(&mut self, at: u64) -> u8 {
        let mut byte = [0u8];
        self.ram.read(at, &mut byte);
        byte[0]
    }

    fn bytes(&mut self, at: u64, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        self.ram.read(at, &mut bytes);
        bytes
    }

    /// A request of `kind` at `sector` with one data buffer of `bytes`
    /// (writable for a read) filled with `fill`, and its status byte:
    /// answers the data's address and the status's.
    fn request(&mut self, kind: u64, sector: u64, bytes: u32, fill: u8) -> (u64, u64) {
        let header = self.header(kind, sector);
        let data = self.buffer(bytes, fill);
        let status = self.buffer(1, 0xff);
        let write = kind != request::T_OUT;
        let mut buffers = vec![(header, 16, false)];
        if bytes != 0 {
            buffers.push((data, bytes, write));
        }
        buffers.push((status, 1, true));
        self.chain(&buffers);
        (data, status)
    }

    fn counts(&mut self) -> BTreeMap<&'static str, u64> {
        self.device.device_counters().into_iter().collect()
    }
}

const ACCEPTED: u64 = transport::VERSION_1
    | features::FLUSH
    | features::BLK_SIZE
    | features::SEG_MAX
    | features::SIZE_MAX;

#[test]
fn the_ram_disk_is_a_zeroed_64_mib_block_device() {
    let device = ram(RAM_DEFAULT_MIB).unwrap();
    assert_eq!(device.bytes(), 64 << 20);
    assert_eq!(device.block_size(), 4096);
    assert!(!device.read_only());
    assert_eq!(RAM_SERIAL.len(), request::ID_BYTES as usize);
    assert!(ram(0).is_err());
    assert!(ram(RAM_MOST_MIB + 1).is_err());
    assert!(ram(RAM_MOST_MIB).is_ok());

    let mut bench = Bench::new(device);
    assert_eq!(bench.read(DEVICE_ID), ids::BLOCK as u32);
    let offered = bench.offered();
    assert_eq!(
        offered,
        ACCEPTED | transport::EVENT_IDX,
        "no RO, DISCARD or WRITE_ZEROES"
    );
    bench.write(QUEUE_SEL, 0);
    assert_eq!(bench.read(QUEUE_NUM_MAX), u32::from(QUEUE_MAX));
    bench.write(QUEUE_SEL, 1);
    assert_eq!(bench.read(QUEUE_NUM_MAX), 0, "one request queue");
    // The configuration: capacity in sectors, read whole or in halves.
    let sectors = (64u64 << 20) / 512;
    assert_eq!(bench.config(0, Width::Double), Some(sectors));
    assert_eq!(bench.config(0, Width::Word), Some(sectors & 0xffff_ffff));
    assert_eq!(bench.config(4, Width::Word), Some(sectors >> 32));
    assert_eq!(bench.config(8, Width::Word), Some(u64::from(SIZE_MAX)));
    assert_eq!(bench.config(12, Width::Word), Some(u64::from(SEG_MAX)));
    assert_eq!(bench.config(20, Width::Word), Some(4096));
    assert_eq!(bench.config(16, Width::Half), Some(0), "no geometry");
    assert_eq!(bench.config(56, Width::Word), Some(0));
    // Past the structure, or misaligned, nothing answers.
    assert_eq!(bench.config(60, Width::Word), None);
    assert_eq!(bench.config(2, Width::Word), None);

    bench.start(ACCEPTED);
    let (data, status) = bench.request(request::T_IN, 8, 8192, 0xee);
    bench.notify();
    assert_eq!(bench.used_index(), 1);
    assert_eq!(bench.used(0), (0, 8193));
    assert_eq!(bench.byte(status), request::S_OK as u8);
    assert_eq!(bench.bytes(data, 8192), vec![0; 8192], "zero-filled");
}

#[test]
fn reads_writes_flushes_and_ids_round_trip() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED | transport::EVENT_IDX);
    // A write of two blocks at block 3 (sector 24), then a read of them.
    let (_, written) = bench.request(request::T_OUT, 24, 8192, 0x5a);
    let (data, read) = bench.request(request::T_IN, 24, 8192, 0);
    bench.notify();
    assert_eq!(bench.used_index(), 2);
    assert_eq!(bench.used(0), (0, 1));
    assert_eq!(bench.used(1), (3, 8193));
    assert_eq!(bench.byte(written), request::S_OK as u8);
    assert_eq!(bench.byte(read), request::S_OK as u8);
    assert_eq!(bench.bytes(data, 8192), vec![0x5a; 8192]);
    // A flush, then the serial.
    let (_, flushed) = bench.request(request::T_FLUSH, 0, 0, 0);
    let (id, named) = bench.request(request::T_GET_ID, 0, 20, 0);
    bench.notify();
    assert_eq!(bench.byte(flushed), request::S_OK as u8);
    assert_eq!(bench.byte(named), request::S_OK as u8);
    assert_eq!(bench.bytes(id, 20), RAM_SERIAL.as_bytes());
    assert_eq!(bench.used(3), (8, 21));
    let counts = bench.counts();
    assert_eq!(
        (
            counts["requests"],
            counts["reads"],
            counts["writes"],
            counts["flushes"],
            counts["ids"]
        ),
        (4, 1, 1, 1, 1)
    );
    assert_eq!(
        (counts["bytes_in"], counts["bytes_out"], counts["errors"]),
        (8192, 8192, 0)
    );
    // The driver asked to hear of each, and was told.
    assert!(bench.device.stats().interrupts >= 1);
    assert_eq!(bench.line.level, Some(true));
}

#[test]
fn a_short_id_buffer_takes_what_fits() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED);
    let (id, status) = bench.request(request::T_GET_ID, 0, 5, 0xff);
    bench.notify();
    assert_eq!(bench.byte(status), request::S_OK as u8);
    assert_eq!(bench.bytes(id, 5), &RAM_SERIAL.as_bytes()[..5]);
}

#[test]
fn other_requests_are_unsupported() {
    let mut bench = Bench::ram_disk();
    // FLUSH not accepted: the driver may not send it, and it is refused.
    bench.start(transport::VERSION_1 | features::BLK_SIZE);
    let (_, flush) = bench.request(request::T_FLUSH, 0, 0, 0);
    let (_, discard) = bench.request(request::T_DISCARD, 0, 16, 0);
    let (_, zeroes) = bench.request(request::T_WRITE_ZEROES, 0, 16, 0);
    let (_, lifetime) = bench.request(10, 0, 8, 0);
    bench.notify();
    for status in [flush, discard, zeroes, lifetime] {
        assert_eq!(bench.byte(status), request::S_UNSUPP as u8);
    }
    let counts = bench.counts();
    assert_eq!(
        (counts["unsupported"], counts["errors"], counts["requests"]),
        (4, 0, 4)
    );
    // The device goes on.
    assert_eq!(bench.device.error(), None);
}

#[test]
fn transfers_out_of_range_or_misaligned_are_io_errors() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED);
    let last = (64u64 << 20) / 512 - 8;
    let cases = [
        // The last block reads; one past it does not.
        (request::T_IN, last, 4096, request::S_OK),
        (request::T_IN, last + 8, 4096, request::S_IOERR),
        (request::T_IN, last, 8192, request::S_IOERR),
        // Not on a block, or not whole blocks.
        (request::T_IN, 1, 4096, request::S_IOERR),
        (request::T_OUT, 8, 512, request::S_IOERR),
        // A sector past 64 bits of bytes.
        (request::T_IN, u64::MAX / 256, 4096, request::S_IOERR),
    ];
    let statuses: Vec<_> = cases
        .iter()
        .map(|&(kind, sector, bytes, _)| bench.request(kind, sector, bytes, 0x11).1)
        .collect();
    bench.notify();
    for ((kind, sector, bytes, expected), status) in cases.iter().zip(statuses) {
        assert_eq!(
            bench.byte(status),
            *expected as u8,
            "type {kind} sector {sector} bytes {bytes}"
        );
    }
    assert_eq!(bench.counts()["errors"], 5);
}

#[test]
fn a_request_past_its_limit_is_an_io_error() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED);
    // Claimed, never touched: refused before any data moves.
    let header = bench.header(request::T_IN, 0);
    let status = bench.buffer(1, 0xff);
    bench.chain(&[
        (header, 16, false),
        (RAM + 0x100_0000, (REQUEST_MAX + 4096) as u32, true),
        (status, 1, true),
    ]);
    bench.notify();
    assert_eq!(bench.byte(status), request::S_IOERR as u8);
}

/// Headers and data spread over buffers of any size, and the status byte
/// at the end of a data buffer: the device reads and writes across them.
#[test]
fn scatter_gather_chains_move_data_across_their_buffers() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED);
    let header = bench.header(request::T_OUT, 16);
    let pattern: Vec<u8> = (0..8192u32).map(|byte| (byte % 251) as u8).collect();
    let pieces = [(0usize, 100usize), (100, 3000), (3100, 5092)];
    let mut buffers = vec![(header, 10, false), (header + 10, 6, false)];
    for &(start, length) in &pieces {
        let at = bench.buffer(length as u32, 0);
        bench.ram.write(at, &pattern[start..start + length]);
        buffers.push((at, length as u32, false));
    }
    let status = bench.buffer(1, 0xff);
    buffers.push((status, 1, true));
    bench.chain(&buffers);
    bench.notify();
    assert_eq!(bench.byte(status), request::S_OK as u8);

    // Read back into three writable buffers, the last also holding the status.
    let header = bench.header(request::T_IN, 16);
    let first = bench.buffer(4000, 0);
    let second = bench.buffer(4000, 0);
    let third = bench.buffer(193, 0);
    bench.chain(&[
        (header, 16, false),
        (first, 4000, true),
        (second, 4000, true),
        (third, 193, true),
    ]);
    bench.notify();
    assert_eq!(bench.byte(third + 192), request::S_OK as u8);
    let mut read = bench.bytes(first, 4000);
    read.extend(bench.bytes(second, 4000));
    read.extend(bench.bytes(third, 192));
    assert_eq!(read, pattern);
    assert_eq!(bench.used(1).1, 8193);
}

#[test]
fn a_read_only_disk_refuses_writes_and_says_so() {
    let (disk, state) = Disk::new(16, true, false);
    state.borrow_mut().bytes[4096..8192].fill(0x77);
    let device = Blk::new(Box::new(disk)).unwrap();
    assert!(device.read_only());
    let mut bench = Bench::new(device);
    assert_ne!(bench.offered() & features::RO, 0);
    bench.start(ACCEPTED | features::RO);
    let (_, write) = bench.request(request::T_OUT, 8, 4096, 0x99);
    let (data, read) = bench.request(request::T_IN, 8, 4096, 0);
    bench.notify();
    assert_eq!(bench.byte(write), request::S_IOERR as u8);
    assert_eq!(bench.byte(read), request::S_OK as u8);
    assert_eq!(bench.bytes(data, 4096), vec![0x77; 4096]);
    assert_eq!(state.borrow().bytes[4096..8192], [0x77; 4096], "untouched");
    let counts = bench.counts();
    assert_eq!(
        (counts["writes"], counts["errors"], counts["reads"]),
        (0, 1, 1)
    );
}

#[test]
fn a_flush_reaches_a_disk_with_a_write_cache() {
    let (disk, state) = Disk::new(16, false, false);
    let mut bench = Bench::new(Blk::new(Box::new(disk)).unwrap());
    bench.start(ACCEPTED);
    bench.request(request::T_OUT, 0, 4096, 1);
    let (_, flushed) = bench.request(request::T_FLUSH, 0, 0, 0);
    bench.notify();
    assert_eq!(bench.byte(flushed), request::S_OK as u8);
    assert_eq!(state.borrow().flushes, 1);
    let (id, _) = bench.request(request::T_GET_ID, 0, 20, 0xff);
    bench.notify();
    assert_eq!(&bench.bytes(id, 20)[..11], b"TESTSERIAL\0");
}

/// A disk that runs requests queued: each completes when the disk does,
/// out of order, and the device stays due meanwhile.
#[test]
fn queued_requests_complete_in_the_disks_order() {
    let (disk, state) = Disk::new(16, false, true);
    state.borrow_mut().bytes[..4096].fill(0x42);
    // The test keeps a second handle on the disk's state to complete
    // its commands; the device owns the disk itself.
    let mut handle = Disk {
        identity: disk.identity.clone(),
        queued: true,
        state: state.clone(),
    };
    let mut bench = Bench::new(Blk::new(Box::new(disk)).unwrap());
    bench.start(ACCEPTED);
    let (data, read) = bench.request(request::T_IN, 0, 4096, 0);
    let (_, write) = bench.request(request::T_OUT, 8, 4096, 0x24);
    bench.notify();
    assert_eq!(bench.used_index(), 0, "both on the disk");
    assert!(bench.device.due());
    let tags: Vec<u32> = state.borrow().started.keys().copied().collect();
    assert_eq!(tags.len(), 2);
    // The write completes first.
    complete(&mut handle, tags[1]);
    bench.step();
    assert_eq!(bench.used_index(), 1);
    assert_eq!(bench.used(0), (3, 1));
    assert_eq!(bench.byte(write), request::S_OK as u8);
    assert!(bench.device.due(), "the read still runs");
    complete(&mut handle, tags[0]);
    bench.step();
    assert_eq!(bench.used_index(), 2);
    assert_eq!(bench.byte(read), request::S_OK as u8);
    assert_eq!(bench.bytes(data, 4096), vec![0x42; 4096]);
    assert!(!bench.device.due());
    assert_eq!(state.borrow().bytes[8 * 512..8 * 512 + 4096], [0x24; 4096]);
}

#[test]
fn a_reset_forgets_the_requests_the_disk_runs() {
    let (disk, state) = Disk::new(16, false, true);
    let mut bench = Bench::new(Blk::new(Box::new(disk)).unwrap());
    bench.start(ACCEPTED);
    bench.request(request::T_IN, 0, 4096, 0);
    bench.notify();
    assert!(bench.device.due());
    bench.write(STATUS, 0);
    assert!(!bench.device.due());
    assert_eq!(state.borrow().started.len(), 1, "the disk's own business");
}

#[test]
fn a_chain_without_a_header_or_status_stops_the_device() {
    for buffers in [
        // A header too short.
        [(BUFFERS, 8, false), (BUFFERS + 0x100, 1, true)],
        // No writable byte for the status.
        [(BUFFERS, 16, false), (BUFFERS + 0x100, 512, false)],
    ] {
        let mut bench = Bench::ram_disk();
        bench.start(ACCEPTED);
        bench.chain(&buffers);
        bench.notify();
        assert!(bench.device.error().is_some());
        assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
        let (events, _) = bench.device.take_events();
        assert!(events.iter().any(|event| matches!(event, Event::Error(_))));
    }
}

#[test]
fn a_read_with_extra_readable_bytes_or_a_write_with_extra_writable_ones_fails() {
    let mut bench = Bench::ram_disk();
    bench.start(ACCEPTED);
    let header = bench.header(request::T_IN, 0);
    let extra = bench.buffer(16, 0);
    let data = bench.buffer(4096, 0);
    let status = bench.buffer(1, 0xff);
    bench.chain(&[
        (header, 16, false),
        (extra, 16, false),
        (data, 4096, true),
        (status, 1, true),
    ]);
    let header = bench.header(request::T_OUT, 0);
    let data = bench.buffer(4096, 0);
    let spare = bench.buffer(64, 0);
    bench.chain(&[(header, 16, false), (data, 4096, false), (spare, 64, true)]);
    bench.notify();
    assert_eq!(bench.byte(status), request::S_IOERR as u8);
    assert_eq!(bench.byte(spare + 63), request::S_IOERR as u8);
}

#[test]
fn only_a_named_read_only_disk_stela_does_not_hold_may_back_the_device() {
    let held = [
        Held {
            serial: "BOOTUSB",
            role: "the disk the PC booted from",
        },
        Held {
            serial: "MACOSDISK",
            role: "the guest's system disk behind ANS",
        },
    ];
    assert_eq!(refusal("SPARE", true, &held), None);
    let refused = refusal("BOOTUSB", true, &held).unwrap();
    assert!(refused.contains("the disk the PC booted from"), "{refused}");
    assert!(refusal("MACOSDISK", true, &held).unwrap().contains("ANS"));
    assert!(
        refusal("SPARE", false, &held)
            .unwrap()
            .contains("read-only")
    );
    assert!(refusal(" ", true, &held).is_some());
    assert!(refusal("", true, &[]).is_some());
}

/// Only the engine's memory may be written through the device: a host
/// disk that is not read-only is refused, a read-only one taken.
#[test]
fn a_writable_host_disk_is_refused() {
    for transport in [
        Transport::Nvme,
        Transport::Usb,
        Transport::Ahci,
        Transport::Firmware,
    ] {
        let (mut disk, _) = Disk::new(16, false, false);
        disk.identity.transport = transport;
        let refused = Blk::new(Box::new(disk)).err().unwrap();
        assert!(
            refused.contains("only read-only"),
            "{transport:?}: {refused}"
        );
        let (mut disk, _) = Disk::new(16, true, false);
        disk.identity.transport = transport;
        assert!(Blk::new(Box::new(disk)).unwrap().read_only());
    }
}

#[test]
fn a_disk_of_a_block_size_the_device_cannot_say_is_refused() {
    for (blocks, block) in [(16, 4097), (16, 256), (16, 1 << 17), (0, 4096)] {
        let (mut disk, _) = Disk::new(1, false, false);
        disk.identity.blocks = blocks;
        disk.identity.block_size = block;
        assert!(Blk::new(Box::new(disk)).is_err(), "{blocks} x {block}");
    }
    let (mut disk, _) = Disk::new(1, false, false);
    disk.identity.block_size = 512;
    disk.identity.blocks = 3;
    let device = Blk::new(Box::new(disk)).unwrap();
    assert_eq!(device.bytes(), 1536);
}
