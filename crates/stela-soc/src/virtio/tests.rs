//! The virtio-mmio transport's registers and the entropy device, through
//! the register page as the kext drives them, on RAM a test owns.
use super::mmio::{Event, Mmio};
use super::queue::Queue;
use super::rng::{CHAIN_BYTES, QUEUE_MAX, Rng};
use super::*;
use crate::bus::{Access, MmioDevice, MmioError, Width};
use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::{AicInput, InterruptSink, Wire};
use stela_contracts::virtio::{features, ids, mmio::*, ring, status};

const RAM: u64 = 0x8_0000_0000;
const IRQ: AicInput = AicInput::new(0, 0x2a0);
const DESC: u64 = RAM;
const AVAIL: u64 = RAM + 0x1000;
const USED: u64 = RAM + 0x2000;
const BUFFERS: u64 = RAM + 0x1_0000;

/// The AIC input's level, as the device last set it.
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

/// An entropy source counting up from `next`, so each byte is known.
fn counting(next: u8) -> rng::Source {
    let mut next = next;
    Box::new(move |bytes: &mut [u8]| {
        for byte in bytes {
            *byte = next;
            next = next.wrapping_add(1);
        }
        true
    })
}

struct Bench {
    device: Mmio,
    ram: Ram,
    line: Line,
}

impl Bench {
    fn new(source: rng::Source) -> Self {
        Self {
            device: Mmio::new(Box::new(Rng::new(source)), IRQ, None),
            ram: Ram::new(RAM, 0x2_0000),
            line: Line::default(),
        }
    }

    fn read(&mut self, register: u64) -> u32 {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device
            .read(register, Width::Word, &mut access)
            .unwrap() as u32
    }

    fn write(&mut self, register: u64, value: u32) {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device
            .write(register, Width::Word, u64::from(value), &mut access)
            .unwrap();
    }

    fn try_write(&mut self, register: u64, width: Width, value: u64) -> Result<(), MmioError> {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device.write(register, width, value, &mut access)
    }

    fn step(&mut self) {
        let mut access = Access::new(0, 0, &mut self.line);
        self.device.step(&mut self.ram, &mut access);
    }

    fn status(&mut self, bits: u64) {
        let now = self.read(STATUS);
        self.write(STATUS, now | bits as u32);
    }

    fn features(&mut self, accepted: u64) {
        for select in 0..2 {
            self.write(DRIVER_FEATURES_SEL, select);
            self.write(DRIVER_FEATURES, (accepted >> (32 * select)) as u32);
        }
    }

    /// Programs queue 0 with `size` entries at DESC, AVAIL and USED.
    fn queue(&mut self, size: u32) {
        self.write(QUEUE_SEL, 0);
        self.write(QUEUE_NUM, size);
        for (low, address) in [
            (QUEUE_DESC_LOW, DESC),
            (QUEUE_AVAIL_LOW, AVAIL),
            (QUEUE_USED_LOW, USED),
        ] {
            self.write(low, address as u32);
            self.write(low + 4, (address >> 32) as u32);
        }
        self.write(QUEUE_READY, 1);
    }

    /// The driver's whole start: reset, features, queue 0 of `size`.
    fn start(&mut self, accepted: u64, size: u32) {
        self.write(STATUS, 0);
        self.status(status::ACKNOWLEDGE | status::DRIVER);
        self.features(accepted);
        self.status(status::FEATURES_OK);
        assert_ne!(self.read(STATUS) & status::FEATURES_OK as u32, 0);
        self.queue(size);
        assert_eq!(self.read(QUEUE_READY), 1);
        self.status(status::DRIVER_OK);
    }

    /// Posts writable buffer `slot` of `bytes` as available entry `slot`,
    /// in a `size`-entry queue, and publishes `count` entries.
    fn post(&mut self, size: u64, slot: u16, bytes: u32, count: u16) {
        let descriptor = [
            (BUFFERS + 0x1000 * u64::from(slot))
                .to_le_bytes()
                .as_slice(),
            &bytes.to_le_bytes(),
            &ring::DESC_F_WRITE.to_le_bytes(),
            &[0; 2],
        ]
        .concat();
        self.ram.write(DESC + 16 * u64::from(slot), &descriptor);
        self.ram.write(
            AVAIL + 4 + 2 * (u64::from(slot) % size),
            &slot.to_le_bytes(),
        );
        self.ram.write(AVAIL + 2, &count.to_le_bytes());
    }

    fn used(&mut self, index: u64, size: u64) -> (u32, u32) {
        let mut element = [0u8; 8];
        self.ram.read(USED + 4 + 8 * (index % size), &mut element);
        (
            u32::from_le_bytes(element[..4].try_into().unwrap()),
            u32::from_le_bytes(element[4..].try_into().unwrap()),
        )
    }

    fn used_index(&mut self) -> u16 {
        let mut index = [0u8; 2];
        self.ram.read(USED + 2, &mut index);
        u16::from_le_bytes(index)
    }
}

#[test]
fn the_page_identifies_a_version_2_entropy_device() {
    let mut bench = Bench::new(counting(0));
    assert_eq!(bench.read(MAGIC_VALUE), 0x7472_6976);
    assert_eq!(bench.read(VERSION), 2);
    assert_eq!(bench.read(DEVICE_ID), ids::RNG as u32);
    assert_eq!(bench.read(VENDOR_ID), VENDOR as u32);
    // Features in two windows: no device bits, the transport's.
    bench.write(DEVICE_FEATURES_SEL, 0);
    assert_eq!(bench.read(DEVICE_FEATURES), features::EVENT_IDX as u32);
    bench.write(DEVICE_FEATURES_SEL, 1);
    assert_eq!(bench.read(DEVICE_FEATURES), 1);
    bench.write(DEVICE_FEATURES_SEL, 2);
    assert_eq!(bench.read(DEVICE_FEATURES), 0);
    bench.write(QUEUE_SEL, 0);
    assert_eq!(bench.read(QUEUE_NUM_MAX), u32::from(QUEUE_MAX));
    bench.write(QUEUE_SEL, 1);
    assert_eq!(bench.read(QUEUE_NUM_MAX), 0, "one queue");
    assert_eq!(bench.read(SHM_LEN_LOW), u32::MAX, "no shared memory");
    assert_eq!(bench.read(CONFIG_GENERATION), 0);
    assert_eq!(bench.device.register_name(QUEUE_READY), Some("QUEUE_READY"));
    assert_eq!(bench.device.register_name(0), Some("MAGIC_VALUE"));
    // Registers are words; there is no configuration space; nothing at 0x48.
    let mut access = Access::new(0, 0, &mut bench.line);
    assert!(
        bench
            .device
            .read(STATUS, Width::Double, &mut access)
            .is_err()
    );
    assert!(bench.device.read(CONFIG, Width::Byte, &mut access).is_err());
    assert!(bench.device.read(0x48, Width::Word, &mut access).is_err());
    assert!(bench.try_write(MAGIC_VALUE, Width::Word, 0).is_err());
    assert!(bench.try_write(STATUS, Width::Half, 1).is_err());
}

#[test]
fn features_ok_holds_only_for_an_offered_subset_with_version_1() {
    let mut bench = Bench::new(counting(0));
    bench.status(status::ACKNOWLEDGE | status::DRIVER);
    // Without VERSION_1 the device leaves FEATURES_OK clear.
    bench.features(features::EVENT_IDX);
    bench.status(status::FEATURES_OK);
    assert_eq!(bench.read(STATUS) & status::FEATURES_OK as u32, 0);
    // A bit it does not offer, likewise.
    bench.features(features::VERSION_1 | features::INDIRECT_DESC);
    bench.status(status::FEATURES_OK);
    assert_eq!(bench.read(STATUS) & status::FEATURES_OK as u32, 0);
    bench.features(features::VERSION_1 | features::EVENT_IDX);
    bench.status(status::FEATURES_OK);
    assert_ne!(bench.read(STATUS) & status::FEATURES_OK as u32, 0);
    assert_eq!(
        bench.device.accepted(),
        features::VERSION_1 | features::EVENT_IDX
    );
    let (events, dropped) = bench.device.take_events();
    assert_eq!(dropped, 0);
    let agreed: Vec<bool> = events
        .iter()
        .filter_map(|event| match event {
            Event::Features { accepted, .. } => Some(*accepted),
            _ => None,
        })
        .collect();
    assert_eq!(agreed, [false, false, true]);
    // Once agreed the features stay: a change stops the device.
    bench.write(DRIVER_FEATURES, 0);
    assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
    // A reset clears everything, the error with it.
    bench.write(STATUS, 0);
    assert_eq!(bench.read(STATUS), 0);
    assert_eq!(bench.device.accepted(), 0);
    assert_eq!(bench.device.error(), None);
}

#[test]
fn a_queue_is_ready_only_on_places_the_split_ring_takes() {
    let mut bench = Bench::new(counting(0));
    // Before FEATURES_OK no queue runs.
    bench.status(status::ACKNOWLEDGE | status::DRIVER);
    bench.queue(8);
    assert_eq!(bench.read(QUEUE_READY), 0);
    bench.features(features::VERSION_1);
    bench.status(status::FEATURES_OK);
    // Not a power of two; past QUEUE_NUM_MAX; a misaligned used ring.
    bench.queue(6);
    assert_eq!(bench.read(QUEUE_READY), 0);
    bench.queue(512);
    assert_eq!(bench.read(QUEUE_READY), 0);
    bench.write(QUEUE_NUM, 8);
    bench.write(QUEUE_USED_LOW, USED as u32 + 2);
    bench.write(QUEUE_READY, 1);
    assert_eq!(bench.read(QUEUE_READY), 0);
    bench.queue(8);
    assert_eq!(bench.read(QUEUE_READY), 1);
    assert_eq!(bench.read(QUEUE_DESC_HIGH), (DESC >> 32) as u32);
    let (events, _) = bench.device.take_events();
    let refused = events
        .iter()
        .filter(|event| matches!(event, Event::QueueRefused { .. }))
        .count();
    assert_eq!(refused, 4);
    assert!(events.contains(&Event::Queue {
        index: 0,
        size: 8,
        desc: DESC,
        avail: AVAIL,
        used: USED
    }));
    // A ready queue's places hold: changing one stops the device.
    bench.write(QUEUE_NUM, 4);
    assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
    assert_eq!(bench.read(QUEUE_READY), 0, "the device forgot its queue");
}

#[test]
fn entropy_fills_each_posted_buffer_and_raises_the_line() {
    let mut bench = Bench::new(counting(1));
    bench.start(features::VERSION_1, 8);
    // DRIVER_OK: the first step checks the areas, with nothing to fill.
    assert!(bench.device.due());
    bench.step();
    assert!(!bench.device.due());
    assert_ne!(bench.line.level, Some(true), "the line is low");
    // Two 64-byte buffers, then a notification.
    bench.post(8, 0, 64, 1);
    bench.post(8, 1, 64, 2);
    bench.write(QUEUE_NOTIFY, 0);
    assert!(bench.device.due());
    bench.step();
    assert_eq!(bench.used_index(), 2);
    assert_eq!(bench.used(0, 8), (0, 64));
    assert_eq!(bench.used(1, 8), (1, 64));
    let mut first = [0u8; 64];
    bench.ram.read(BUFFERS, &mut first);
    let counted: Vec<u8> = (1..=64).collect();
    assert_eq!(first[..], counted[..]);
    let mut second = [0u8; 1];
    bench.ram.read(BUFFERS + 0x1000, &mut second);
    assert_eq!(second[0], 65, "the second buffer gets the next bytes");
    // No EVENT_IDX and no NO_INTERRUPT: the line rises until acknowledged.
    assert_eq!(bench.line.level, Some(true));
    assert_eq!(bench.read(INTERRUPT_STATUS), INT_VRING as u32);
    bench.write(INTERRUPT_ACK, INT_VRING as u32);
    assert_eq!(bench.line.level, Some(false));
    assert_eq!(bench.read(INTERRUPT_STATUS), 0);
    let stats = bench.device.stats();
    assert_eq!(
        (stats.notifies, stats.interrupts, stats.chains, stats.bytes),
        (1, 1, 2, 128)
    );
    // A large request gets CHAIN_BYTES; the first notification was logged once.
    bench.post(8, 2, 64 * 1024, 3);
    bench.write(QUEUE_NOTIFY, 0);
    bench.step();
    assert_eq!(bench.used(2, 8), (2, CHAIN_BYTES as u32));
    let (events, _) = bench.device.take_events();
    let notified = events
        .iter()
        .filter(|event| matches!(event, Event::Notified { index: 0 }))
        .count();
    assert_eq!(notified, 1);
}

#[test]
fn event_idx_suppresses_interrupts_and_asks_for_the_next_kick() {
    let mut bench = Bench::new(counting(0));
    bench.start(features::VERSION_1 | features::EVENT_IDX, 8);
    bench.step();
    // used_event far ahead: no interrupt for the first fill.
    bench.ram.write(AVAIL + 4 + 2 * 8, &0x8000u16.to_le_bytes());
    bench.post(8, 0, 32, 1);
    bench.write(QUEUE_NOTIFY, 0);
    bench.step();
    assert_eq!(bench.used_index(), 1);
    assert_ne!(bench.line.level, Some(true), "the line is low");
    // The device asks for a kick at the next request: avail_event = 1.
    let mut event = [0u8; 2];
    bench.ram.read(USED + 4 + 8 * 8, &mut event);
    assert_eq!(u16::from_le_bytes(event), 1);
    // used_event 1: the next completion interrupts.
    bench.ram.write(AVAIL + 4 + 2 * 8, &1u16.to_le_bytes());
    bench.post(8, 1, 32, 2);
    bench.write(QUEUE_NOTIFY, 0);
    bench.step();
    assert_eq!(bench.line.level, Some(true));
}

#[test]
fn a_bad_ring_stops_the_device_and_asks_for_a_reset() {
    let notice: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let mut bench = Bench::new(counting(0));
    bench.device = Mmio::new(Box::new(Rng::new(counting(0))), IRQ, Some(notice));
    bench.start(features::VERSION_1, 8);
    assert!(notice.swap(false, Ordering::AcqRel), "the start was logged");
    bench.step();
    // A readable buffer: the device may not write it.
    let descriptor = [
        BUFFERS.to_le_bytes().as_slice(),
        &64u32.to_le_bytes(),
        &[0; 4],
    ]
    .concat();
    bench.ram.write(DESC, &descriptor);
    bench.ram.write(AVAIL + 2, &1u16.to_le_bytes());
    bench.write(QUEUE_NOTIFY, 0);
    bench.step();
    assert_eq!(
        bench.device.error(),
        Some("an entropy request the device may not write")
    );
    assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
    assert_eq!(bench.read(INTERRUPT_STATUS), INT_CONFIG as u32);
    assert_eq!(bench.line.level, Some(true));
    assert!(!bench.device.due(), "a stopped device does no work");
    assert!(notice.load(Ordering::Acquire));
    // The driver cannot clear the error but by a reset.
    bench.write(STATUS, status::ACKNOWLEDGE as u32);
    assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
    bench.write(STATUS, 0);
    assert_eq!(bench.read(STATUS), 0);
    assert_eq!(bench.line.level, Some(false));
    let (events, _) = bench.device.take_events();
    assert!(events.contains(&Event::Reset));

    // Rings outside RAM stop the device at the first step.
    let mut bench = Bench::new(counting(0));
    bench.write(STATUS, 0);
    bench.status(status::ACKNOWLEDGE | status::DRIVER);
    bench.features(features::VERSION_1);
    bench.status(status::FEATURES_OK);
    bench.write(QUEUE_SEL, 0);
    bench.write(QUEUE_NUM, 8);
    bench.write(QUEUE_DESC_LOW, 0x1000);
    bench.write(QUEUE_AVAIL_LOW, AVAIL as u32);
    bench.write(QUEUE_AVAIL_HIGH, (AVAIL >> 32) as u32);
    bench.write(QUEUE_USED_LOW, USED as u32);
    bench.write(QUEUE_USED_HIGH, (USED >> 32) as u32);
    bench.write(QUEUE_READY, 1);
    bench.status(status::DRIVER_OK);
    bench.step();
    assert_eq!(bench.device.error(), Some("a ring or buffer is not RAM"));

    // A failing source stops the device: no buffer gets bytes that are not random.
    let mut bench = Bench::new(Box::new(|_: &mut [u8]| false));
    bench.start(features::VERSION_1, 8);
    bench.post(8, 0, 64, 1);
    bench.write(QUEUE_NOTIFY, 0);
    bench.step();
    assert_eq!(
        bench.device.error(),
        Some("the host's random source failed")
    );
    assert_eq!(bench.used_index(), 0);
}

#[test]
fn driver_ok_without_features_ok_stops_the_device() {
    let mut bench = Bench::new(counting(0));
    bench.status(status::ACKNOWLEDGE | status::DRIVER | status::DRIVER_OK);
    assert_eq!(bench.device.error(), Some("DRIVER_OK before FEATURES_OK"));
    assert!(!bench.device.running());
}

#[test]
fn a_split_queue_takes_three_aligned_areas() {
    assert!(Queue::split(DESC, AVAIL, USED, 8, 256).is_ok());
    assert!(Queue::split(DESC + 8, AVAIL, USED, 8, 256).is_err());
    assert!(Queue::split(DESC, AVAIL + 1, USED, 8, 256).is_err());
    assert!(Queue::split(DESC, AVAIL, USED + 2, 8, 256).is_err());
    assert!(Queue::split(DESC, AVAIL, USED, 0, 256).is_err());
    assert!(Queue::split(DESC, AVAIL, USED, 12, 256).is_err());
    assert!(Queue::split(DESC, AVAIL, USED, 512, 256).is_err());
    assert!(Queue::split(0, AVAIL, USED, 8, 256).is_err());
    assert!(Queue::split(DESC, u64::MAX - 1, USED, 8, 256).is_err());
    // The areas are where they were put: a chain at DESC, published at AVAIL,
    // used at USED.
    let mut ram = Ram::new(RAM, 0x2_0000);
    let mut queue = Queue::split(DESC, AVAIL, USED, 4, 256).unwrap();
    queue.check(&mut ram).unwrap();
    let descriptor = [
        BUFFERS.to_le_bytes().as_slice(),
        &16u32.to_le_bytes(),
        &ring::DESC_F_WRITE.to_le_bytes(),
        &[0; 2],
    ]
    .concat();
    ram.write(DESC + 16 * 3, &descriptor);
    ram.write(AVAIL + 4, &3u16.to_le_bytes());
    ram.write(AVAIL + 2, &1u16.to_le_bytes());
    let chain = queue.pop(&mut ram).unwrap().unwrap();
    assert_eq!(chain.head, 3);
    assert_eq!(chain.bytes(), 16);
    queue.push_used(&mut ram, chain.head, 16).unwrap();
    assert!(queue.publish(&mut ram, false).unwrap());
    let mut used = [0u8; 12];
    ram.read(USED, &mut used);
    assert_eq!(used, [0, 0, 1, 0, 3, 0, 0, 0, 16, 0, 0, 0]);
    // An area that is not RAM fails the check.
    let queue = Queue::split(DESC, AVAIL, RAM + 0x2_0000, 4, 256).unwrap();
    assert!(queue.check(&mut ram).is_err());
}
