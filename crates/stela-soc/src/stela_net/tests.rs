//! A driver written against the ring layout drives the device over test
//! RAM: rings, wraparound, chained descriptors, notification suppression,
//! malformed rings and the queues' drop rules.
extern crate std;

use super::queue::{Queue, RingError, need_event};
use super::*;
use alloc::vec;
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::InterruptLatch;
use stela_contracts::net::ring::{self, DESC_F_INDIRECT, DESC_F_NEXT, DESC_F_WRITE};

const RAM: u64 = 0x8_0000_0000;
const RX: u64 = RAM;
const TX: u64 = RAM + 0x1_0000;
const BUFFERS: u64 = RAM + 0x2_0000;
const MAC_ADDRESS: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
const IRQ: AicInput = AicInput::new(0, 0x2a0);

/// The driver side of one queue: a descriptor table it fills, an available
/// ring it publishes and a used ring it reads.
struct Driver {
    base: u64,
    size: u16,
    avail: u16,
    used: u16,
    next_descriptor: u16,
}

impl Driver {
    fn new(base: u64, size: u16) -> Self {
        Self {
            base,
            size,
            avail: 0,
            used: 0,
            next_descriptor: 0,
        }
    }

    fn avail(&self) -> u64 {
        self.base + ring::avail(u64::from(self.size))
    }

    fn used_ring(&self) -> u64 {
        self.base + ring::used(u64::from(self.size))
    }

    fn descriptor(
        &mut self,
        ram: &mut Ram,
        address: u64,
        bytes: u32,
        flags: u16,
        next: u16,
    ) -> u16 {
        let index = self.next_descriptor % self.size;
        self.next_descriptor = self.next_descriptor.wrapping_add(1);
        let mut entry = [0u8; 16];
        entry[..8].copy_from_slice(&address.to_le_bytes());
        entry[8..12].copy_from_slice(&bytes.to_le_bytes());
        entry[12..14].copy_from_slice(&flags.to_le_bytes());
        entry[14..].copy_from_slice(&next.to_le_bytes());
        assert!(ram.write(self.base + 16 * u64::from(index), &entry));
        index
    }

    /// Writes a chain over `buffers` (address, bytes) and makes it
    /// available, unpublished.
    fn chain(&mut self, ram: &mut Ram, buffers: &[(u64, u32)], write: bool) -> u16 {
        let first = self.next_descriptor % self.size;
        for (index, &(address, bytes)) in buffers.iter().enumerate() {
            let last = index + 1 == buffers.len();
            let next = (first + index as u16 + 1) % self.size;
            let mut flags = if last { 0 } else { DESC_F_NEXT };
            if write {
                flags |= DESC_F_WRITE;
            }
            self.descriptor(ram, address, bytes, flags, if last { 0 } else { next });
        }
        self.offer(ram, first);
        first
    }

    fn offer(&mut self, ram: &mut Ram, head: u16) {
        let slot = u64::from(self.avail % self.size);
        assert!(ram.write(self.avail() + 4 + 2 * slot, &head.to_le_bytes()));
        self.avail = self.avail.wrapping_add(1);
    }

    fn publish(&self, ram: &mut Ram) {
        assert!(ram.write(self.avail() + 2, &self.avail.to_le_bytes()));
    }

    fn set_used_event(&self, ram: &mut Ram, event: u16) {
        let at = self.avail() + 4 + 2 * u64::from(self.size);
        assert!(ram.write(at, &event.to_le_bytes()));
    }

    fn avail_event(&self, ram: &mut Ram) -> u16 {
        let mut bytes = [0; 2];
        assert!(ram.read(self.used_ring() + 4 + 8 * u64::from(self.size), &mut bytes));
        u16::from_le_bytes(bytes)
    }

    /// Whether publishing from `old` to the current index needs a kick.
    fn needs_kick(&self, ram: &mut Ram, old: u16) -> bool {
        need_event(self.avail_event(ram), self.avail, old)
    }

    /// The used entries since the last call, as (id, len).
    fn reap(&mut self, ram: &mut Ram) -> Vec<(u32, u32)> {
        let mut index = [0; 2];
        assert!(ram.read(self.used_ring() + 2, &mut index));
        let index = u16::from_le_bytes(index);
        let mut entries = Vec::new();
        while self.used != index {
            let slot = u64::from(self.used % self.size);
            let mut element = [0; 8];
            assert!(ram.read(self.used_ring() + 4 + 8 * slot, &mut element));
            entries.push((
                u32::from_le_bytes(element[..4].try_into().unwrap()),
                u32::from_le_bytes(element[4..].try_into().unwrap()),
            ));
            self.used = self.used.wrapping_add(1);
        }
        entries
    }
}

fn frame(to: [u8; 6], length: usize, fill: u8) -> Vec<u8> {
    let mut frame = vec![fill; length];
    frame[..6].copy_from_slice(&to);
    frame[6..12].copy_from_slice(&[0x02, 0, 0, 0, 0, 9]);
    frame
}

struct Rig {
    ram: Ram,
    latch: InterruptLatch,
    net: Net,
    rx: Driver,
    tx: Driver,
}

impl Rig {
    fn new(size: u16, accept: u64) -> Self {
        Self::offering(size, features::OFFERED, accept)
    }

    fn offering(size: u16, offered: u64, accept: u64) -> Self {
        let mut rig = Self {
            ram: Ram::new(RAM, 0x10_0000),
            latch: InterruptLatch::new(),
            net: Net::with_features(MAC_ADDRESS, IRQ, offered),
            rx: Driver::new(RX, size),
            tx: Driver::new(TX, size),
        };
        for (register, value) in [
            (ACCEPT, accept),
            (RX_RING, RX),
            (RX_SIZE, u64::from(size)),
            (TX_RING, TX),
            (TX_SIZE, u64::from(size)),
            (ENABLE, 1),
        ] {
            rig.write(register, value).unwrap();
        }
        assert!(rig.net.up());
        rig
    }

    fn write(&mut self, offset: u64, value: u64) -> Result<(), MmioError> {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.net.write(offset, Width::Double, value, &mut access)
    }

    fn read(&mut self, offset: u64) -> u64 {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.net.read(offset, Width::Double, &mut access).unwrap()
    }

    fn step(&mut self) {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.net.step(&mut self.ram, &mut access);
    }

    fn line(&self) -> bool {
        self.latch.is_asserted(Wire::Aic(IRQ))
    }

    /// Posts `count` receive buffers of `bytes` each, from buffer slot
    /// `first` on.
    fn post(&mut self, first: u64, count: u64, bytes: u32) {
        for slot in first..first + count {
            self.rx
                .chain(&mut self.ram, &[(BUFFERS + slot * 0x800, bytes)], true);
        }
        self.rx.publish(&mut self.ram);
    }

    fn send(&mut self, frame: &[u8], slot: u64) -> u16 {
        self.send_with(frame, slot, [0; 12])
    }

    fn send_with(&mut self, frame: &[u8], slot: u64, header: [u8; 12]) -> u16 {
        let address = BUFFERS + 0x8_0000 + slot * 0x800;
        let mut buffer = header.to_vec();
        buffer.extend_from_slice(frame);
        assert!(self.ram.write(address, &buffer));
        let head = self
            .tx
            .chain(&mut self.ram, &[(address, buffer.len() as u32)], false);
        self.tx.publish(&mut self.ram);
        head
    }
}

#[test]
fn the_register_page_identifies_the_device() {
    let mut rig = Rig::new(8, features::OFFERED);
    assert_eq!(rig.read(VERSION), 0x534e_4554_0000_0001);
    assert_eq!(rig.read(FEATURES), features::OFFERED);
    assert_eq!(rig.read(MAC).to_le_bytes()[..6], MAC_ADDRESS);
    assert_eq!(rig.read(LINK), 1000 << 32 | 1);
    assert_eq!(rig.read(ENABLE), 1);
    let mut access = Access::new(0, 0, &mut rig.latch);
    assert!(rig.net.read(VERSION, Width::Word, &mut access).is_err());
    assert!(rig.net.read(0x3ff8, Width::Double, &mut access).is_err());
    // A running device's rings stay where they are.
    assert!(rig.write(RX_RING, 0).is_err());
}

#[test]
fn a_transmitted_frame_reaches_the_outbound_queue_and_completes() {
    let mut rig = Rig::new(8, features::OFFERED);
    // The driver reclaims on its next send: no completion interrupts.
    rig.tx.set_used_event(&mut rig.ram, 0x8000);
    let sent = frame([0xff; 6], 60, 0xa5);
    let head = rig.send(&sent, 0);
    rig.write(KICK, KICK_TX).unwrap();
    assert!(rig.net.due());
    rig.step();
    assert_eq!(rig.net.take(), Some(sent));
    assert!(rig.net.take_service_due());
    assert!(!rig.net.take_service_due(), "asked once");
    assert_eq!(rig.tx.reap(&mut rig.ram), [(u32::from(head), 0)]);
    assert_eq!(rig.read(TX_FRAMES), 1);
    assert!(!rig.line());
    assert!(!rig.net.due());
    // A driver short of descriptors asks to hear of the next completion.
    rig.tx.set_used_event(&mut rig.ram, 1);
    rig.send(&frame([0xff; 6], 60, 0x5a), 1);
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert!(rig.line());
    assert_eq!(rig.read(CAUSE), CAUSE_TX);
}

#[test]
fn a_chained_transmit_frame_is_gathered_across_its_buffers() {
    let mut rig = Rig::new(8, features::OFFERED);
    let sent = frame(MAC_ADDRESS, 1514, 0x3c);
    let mut bytes = vec![0u8; HEADER_BYTES as usize];
    bytes.extend_from_slice(&sent);
    // Header alone, then the frame split unevenly over three buffers.
    let pieces = [12usize, 100, 1000, 414];
    let mut buffers = Vec::new();
    let mut offset = 0;
    for (index, &length) in pieces.iter().enumerate() {
        let address = BUFFERS + index as u64 * 0x1000 + 3;
        assert!(rig.ram.write(address, &bytes[offset..offset + length]));
        buffers.push((address, length as u32));
        offset += length;
    }
    rig.tx.chain(&mut rig.ram, &buffers, false);
    rig.tx.publish(&mut rig.ram);
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert_eq!(rig.net.take(), Some(sent));
}

#[test]
fn a_received_frame_lands_after_its_header_and_interrupts() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.post(0, 2, 2048);
    rig.rx.set_used_event(&mut rig.ram, 0);
    let delivered = frame(MAC_ADDRESS, 98, 0x77);
    rig.net.deliver_owned(delivered.clone());
    assert!(rig.net.due());
    rig.step();
    assert!(rig.line());
    assert_eq!(rig.read(CAUSE), CAUSE_RX);
    let used = rig.rx.reap(&mut rig.ram);
    assert_eq!(used, [(0, 12 + 98)]);
    let mut landed = vec![0u8; 12 + 98];
    assert!(rig.ram.read(BUFFERS, &mut landed));
    assert_eq!(&landed[..10], &[0; 10]);
    assert_eq!(
        u16::from_le_bytes([landed[10], landed[11]]),
        1,
        "num_buffers"
    );
    assert_eq!(&landed[12..], &delivered[..]);
    rig.write(ACK, CAUSE_RX).unwrap();
    assert!(!rig.line());
    assert_eq!(rig.read(RX_FRAMES), 1);
}

#[test]
fn a_received_frame_scatters_over_a_chain_of_buffers() {
    let mut rig = Rig::new(8, features::OFFERED);
    let buffers = [
        (BUFFERS, 12),
        (BUFFERS + 0x1000 + 1, 500),
        (BUFFERS + 0x2000, 1100),
    ];
    rig.rx.chain(&mut rig.ram, &buffers, true);
    rig.rx.publish(&mut rig.ram);
    let delivered = frame(MAC_ADDRESS, 1514, 0x19);
    rig.net.deliver_owned(delivered.clone());
    rig.step();
    assert_eq!(rig.rx.reap(&mut rig.ram), [(0, 12 + 1514)]);
    let mut first = vec![0u8; 500];
    assert!(rig.ram.read(BUFFERS + 0x1000 + 1, &mut first));
    assert_eq!(first, delivered[..500]);
    let mut rest = vec![0u8; 1014];
    assert!(rig.ram.read(BUFFERS + 0x2000, &mut rest));
    assert_eq!(rest, delivered[500..]);
}

#[test]
fn the_rings_wrap_their_slots_both_ways() {
    let mut rig = Rig::new(4, features::OFFERED);
    // Ten laps of a four-entry ring, both ways, past several slot wraps.
    let mut received = 0u64;
    for lap in 0..40u64 {
        rig.post(lap % 4, 1, 2048);
        let delivered = frame(MAC_ADDRESS, 64, lap as u8);
        rig.net.deliver_owned(delivered.clone());
        let sent = frame([0xff; 6], 64, !(lap as u8));
        rig.send(&sent, lap % 4);
        rig.write(KICK, KICK_TX | KICK_RX).unwrap();
        rig.step();
        assert_eq!(rig.net.take(), Some(sent));
        let used = rig.rx.reap(&mut rig.ram);
        assert_eq!(used.len(), 1);
        received += 1;
        assert_eq!(rig.tx.reap(&mut rig.ram).len(), 1);
    }
    assert_eq!(rig.read(RX_FRAMES), received);
}

#[test]
fn indices_cross_the_u16_wrap() {
    // Batches of three through a four-entry ring until both indices have
    // wrapped past 0xffff: every chain comes back once, in order.
    let mut ram = Ram::new(RAM, 0x1_0000);
    let mut queue = Queue::new(RX, 4).unwrap();
    let mut driver = Driver::new(RX, 4);
    for round in 0..21_850u32 {
        let heads: Vec<u16> = (0..3)
            .map(|slot| driver.chain(&mut ram, &[(BUFFERS + slot * 0x100, 64)], true))
            .collect();
        driver.publish(&mut ram);
        assert_eq!(queue.pending(&mut ram).unwrap(), 3);
        for &head in &heads {
            let chain = queue.pop(&mut ram).unwrap().unwrap();
            assert_eq!(chain.head, head);
            queue.push_used(&mut ram, chain.head, round).unwrap();
        }
        assert_eq!(queue.pop(&mut ram).unwrap(), None);
        queue.publish(&mut ram, false).unwrap();
        let reaped: Vec<(u32, u32)> = heads.iter().map(|&head| (u32::from(head), round)).collect();
        assert_eq!(driver.reap(&mut ram), reaped);
    }
    assert!(driver.avail < 0x100, "the available index wrapped");
}

#[test]
fn event_indices_suppress_interrupts_until_the_driver_asks() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.post(0, 4, 2048);
    // The driver wants to hear once the used index passes 2.
    rig.rx.set_used_event(&mut rig.ram, 2);
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 1));
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 2));
    rig.step();
    assert!(!rig.line(), "used index 2 does not pass used_event 2");
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 3));
    rig.step();
    assert!(rig.line(), "used index 3 passes it");
    assert_eq!(rig.read(CAUSE), CAUSE_RX);
}

#[test]
fn without_event_indices_the_avail_flag_suppresses_interrupts() {
    let mut rig = Rig::new(8, features::VERSION_1 | features::MAC);
    rig.post(0, 2, 2048);
    assert!(rig.ram.write(
        RX + ring::avail(8),
        &ring::AVAIL_F_NO_INTERRUPT.to_le_bytes()
    ));
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 1));
    rig.step();
    assert!(!rig.line());
    assert!(rig.ram.write(RX + ring::avail(8), &0u16.to_le_bytes()));
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 2));
    rig.step();
    assert!(rig.line());
}

#[test]
fn the_device_asks_for_a_kick_only_when_it_needs_one() {
    let mut rig = Rig::new(8, features::OFFERED);
    // Transmit: once drained, the next publish needs a kick.
    let old = rig.tx.avail;
    rig.send(&frame([0xff; 6], 64, 1), 0);
    assert!(
        rig.tx.needs_kick(&mut rig.ram, old),
        "a fresh ring asks for the first"
    );
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    let old = rig.tx.avail;
    rig.send(&frame([0xff; 6], 64, 2), 1);
    assert!(rig.tx.needs_kick(&mut rig.ram, old));
    // A second publish before the device ran needs none.
    let old = rig.tx.avail;
    rig.send(&frame([0xff; 6], 64, 3), 2);
    assert!(!rig.tx.needs_kick(&mut rig.ram, old));
    // Receive: buffers posted while the device has some need no kick...
    rig.post(0, 2, 2048);
    rig.write(KICK, KICK_TX | KICK_RX).unwrap();
    rig.step();
    let old = rig.rx.avail;
    rig.post(2, 1, 2048);
    assert!(!rig.rx.needs_kick(&mut rig.ram, old));
    // ...but once a frame found none, the next post kicks.
    for fill in 0..4 {
        rig.net.deliver_owned(frame(MAC_ADDRESS, 64, fill));
    }
    rig.step();
    assert_eq!(rig.rx.reap(&mut rig.ram).len(), 3);
    assert!(!rig.net.due(), "starved until a receive kick");
    let old = rig.rx.avail;
    rig.post(3, 1, 2048);
    assert!(rig.rx.needs_kick(&mut rig.ram, old));
    rig.write(KICK, KICK_RX).unwrap();
    assert!(rig.net.due());
    rig.step();
    assert_eq!(rig.rx.reap(&mut rig.ram).len(), 1);
}

#[test]
fn the_receive_filter_and_queue_drop_as_the_usb_adapter_does() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.net.deliver_owned(frame([0x02, 9, 9, 9, 9, 9], 64, 0));
    assert_eq!(rig.net.counters().filtered, 1, "another station's frame");
    rig.net.deliver_owned(vec![0; 13]);
    rig.net.deliver_owned(vec![0; 1515]);
    assert_eq!(rig.net.counters().oversized, 2);
    rig.write(FILTER, FILTER_PROMISCUOUS).unwrap();
    rig.net.deliver_owned(frame([0x02, 9, 9, 9, 9, 9], 64, 0));
    assert_eq!(rig.net.counters().filtered, 1);
    for _ in 0..QUEUE_FRAMES {
        rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 0));
    }
    assert!(!rig.net.has_room());
    assert_eq!(rig.net.counters().overflowed, 1);
    assert_eq!(rig.read(RX_DROPS), 4);
    // A stopped device takes nothing and forgets what it held.
    rig.write(ENABLE, 0).unwrap();
    assert!(rig.net.has_room());
    rig.net.deliver_owned(frame(MAC_ADDRESS, 64, 0));
    assert_eq!(rig.net.counters().filtered, 2);
}

#[test]
fn a_frame_too_long_for_the_buffer_is_dropped_and_the_buffer_kept() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.post(0, 1, 128);
    rig.net.deliver_owned(frame(MAC_ADDRESS, 1000, 1));
    rig.net.deliver_owned(frame(MAC_ADDRESS, 100, 2));
    rig.step();
    assert_eq!(rig.rx.reap(&mut rig.ram), [(0, 112)]);
    assert_eq!(rig.net.counters().oversized, 1);
}

#[test]
fn malformed_transmit_chains_complete_unsent() {
    let mut rig = Rig::new(8, features::OFFERED);
    // Too short to hold an Ethernet header, then a writable buffer.
    rig.send(&[0u8; 4], 0);
    rig.tx.chain(&mut rig.ram, &[(BUFFERS, 100)], true);
    rig.tx.publish(&mut rig.ram);
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert_eq!(rig.net.take(), None);
    assert_eq!(rig.tx.reap(&mut rig.ram).len(), 2);
    assert_eq!(rig.read(TX_ERRORS), 2);
    assert!(rig.net.up());
}

#[test]
fn a_broken_ring_stops_the_device_with_an_error_cause() {
    let mut rig = Rig::new(8, features::OFFERED);
    // An indirect descriptor.
    let head = rig
        .tx
        .descriptor(&mut rig.ram, BUFFERS, 16, DESC_F_INDIRECT, 0);
    rig.tx.offer(&mut rig.ram, head);
    rig.tx.publish(&mut rig.ram);
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert!(!rig.net.up());
    assert_eq!(rig.read(ENABLE), 0);
    assert_eq!(rig.read(CAUSE), CAUSE_ERROR);
    assert!(rig.line());
    assert!(rig.net.error().is_some());

    // A chain that loops, a next past the table, an index a lap ahead.
    for case in 0..3 {
        let mut rig = Rig::new(4, features::OFFERED);
        match case {
            0 => {
                rig.tx.descriptor(&mut rig.ram, BUFFERS, 64, DESC_F_NEXT, 1);
                rig.tx.descriptor(&mut rig.ram, BUFFERS, 64, DESC_F_NEXT, 0);
                rig.tx.offer(&mut rig.ram, 0);
            }
            1 => {
                rig.tx.descriptor(&mut rig.ram, BUFFERS, 64, DESC_F_NEXT, 9);
                rig.tx.offer(&mut rig.ram, 0);
            }
            _ => rig.tx.avail = 5,
        }
        rig.tx.publish(&mut rig.ram);
        rig.write(KICK, KICK_TX).unwrap();
        rig.step();
        assert!(!rig.net.up(), "case {case}");
    }
}

#[test]
fn rings_outside_ram_or_badly_sized_are_refused() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.write(ENABLE, 0).unwrap();
    rig.write(TX_RING, 0x1000).unwrap();
    rig.write(ENABLE, 1).unwrap();
    assert!(rig.net.up());
    assert!(rig.net.due(), "the rings are checked at the first step");
    rig.step();
    assert!(!rig.net.up());
    assert_eq!(rig.read(CAUSE) & CAUSE_ERROR, CAUSE_ERROR);
    rig.write(ACK, CAUSE_ERROR).unwrap();
    assert!(!rig.line());
    for (register, value) in [(RX_SIZE, 6), (RX_SIZE, 2048), (RX_RING, RX + 8)] {
        let mut rig = Rig::new(8, features::OFFERED);
        rig.write(ENABLE, 0).unwrap();
        rig.write(register, value).unwrap();
        rig.write(ENABLE, 1).unwrap();
        assert!(!rig.net.up(), "{register:#x}={value:#x}");
    }
    let mut rig = Rig::new(8, features::OFFERED);
    rig.write(ENABLE, 0).unwrap();
    rig.write(ACCEPT, features::MAC).unwrap();
    rig.write(ENABLE, 1).unwrap();
    assert!(!rig.net.up(), "VERSION_1 is required");
}

#[test]
fn a_full_outbound_queue_leaves_chains_until_the_host_takes_frames() {
    let mut rig = Rig::new(512, features::OFFERED);
    for slot in 0..(QUEUE_FRAMES as u64 + 2) {
        rig.send(&frame([0xff; 6], 64, slot as u8), slot % 64);
    }
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert_eq!(rig.tx.reap(&mut rig.ram).len(), QUEUE_FRAMES);
    assert!(!rig.net.due(), "no room");
    rig.net.take().unwrap();
    assert!(rig.net.due(), "room again, no kick needed");
    rig.step();
    assert_eq!(rig.tx.reap(&mut rig.ram).len(), 1);
}

#[test]
fn need_event_matches_the_virtio_rule() {
    assert!(need_event(0, 1, 0));
    assert!(!need_event(1, 1, 0));
    assert!(need_event(5, 7, 3));
    assert!(!need_event(7, 7, 3));
    // Across the u16 wrap.
    assert!(need_event(0xffff, 1, 0xfffe));
    assert!(!need_event(1, 1, 0xfffe));
}

#[test]
fn chains_refuse_bad_shapes() {
    let mut ram = Ram::new(RAM, 0x1_0000);
    let mut queue = Queue::new(RX, 4).unwrap();
    let mut driver = Driver::new(RX, 4);
    // Writable then readable.
    driver.descriptor(&mut ram, BUFFERS, 8, DESC_F_NEXT | DESC_F_WRITE, 1);
    driver.descriptor(&mut ram, BUFFERS, 8, 0, 0);
    driver.offer(&mut ram, 0);
    driver.publish(&mut ram);
    assert!(matches!(queue.pop(&mut ram), Err(RingError::Malformed(_))));
    assert!(Queue::new(0, 4).is_err());
    assert!(Queue::new(RX, 0).is_err());
    assert!(Queue::new(0xffff_ffff_ffff_fff0, 4).is_err());
}

/// A header marking a partial checksum from `start`, its field at `offset`.
fn partial(start: u16, offset: u16) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0] = offload::NEEDS_CSUM;
    header[6..8].copy_from_slice(&start.to_le_bytes());
    header[8..10].copy_from_slice(&offset.to_le_bytes());
    header
}

#[test]
fn checksum_offloads_are_offered_only_when_the_boot_asks() {
    let mut rig = Rig::new(8, features::OFFERED);
    assert_eq!(rig.read(FEATURES) & features::CHECKSUMS, 0);
    rig.write(ENABLE, 0).unwrap();
    rig.write(ACCEPT, features::OFFERED | features::CSUM)
        .unwrap();
    rig.write(ENABLE, 1).unwrap();
    assert!(!rig.net.up(), "CSUM was not offered");
    let all = features::OFFERED | features::CHECKSUMS;
    let mut rig = Rig::offering(8, all, all);
    assert_eq!(rig.read(FEATURES), all);
    assert!(rig.net.up());
}

#[test]
fn a_partial_transmit_checksum_is_completed() {
    let all = features::OFFERED | features::CHECKSUMS;
    let mut rig = Rig::offering(8, all, all);
    let mut good = offload::tests::tcp4(b"partial checksum");
    good[..6].copy_from_slice(&[0xff; 6]);
    assert_ne!(good[50..52], [0, 0]);
    // The stack leaves the pseudo-header sum in the field.
    let mut pseudo = good[26..34].to_vec();
    pseudo.extend_from_slice(&[0, 6, 0, 20 + 16]);
    let mut sent = good.clone();
    sent[50..52].copy_from_slice(&(!offload::tests::reference(&pseudo)).to_be_bytes());
    rig.send_with(&sent, 0, partial(34, 16));
    // A field past the frame, and a segmentation request, are refused.
    rig.send_with(&sent, 1, partial(34, 4000));
    let mut segmented = [0u8; 12];
    segmented[1] = 1;
    rig.send_with(&sent, 2, segmented);
    rig.write(KICK, KICK_TX).unwrap();
    rig.step();
    assert_eq!(rig.net.take(), Some(good));
    assert_eq!(rig.net.take(), None);
    assert_eq!(rig.read(TX_ERRORS), 2);
    assert_eq!(rig.tx.reap(&mut rig.ram).len(), 3);
}

#[test]
fn received_checksums_the_host_verified_are_marked_valid() {
    let all = features::OFFERED | features::CHECKSUMS;
    let header_flags = |accept: u64, frame: &[u8]| {
        let mut rig = Rig::offering(8, all, accept);
        rig.post(0, 1, 2048);
        rig.net.deliver_owned(frame.to_vec());
        rig.step();
        assert_eq!(rig.rx.reap(&mut rig.ram).len(), 1);
        let mut flags = [0u8];
        assert!(rig.ram.read(BUFFERS, &mut flags));
        flags[0]
    };
    let mut good = offload::tests::tcp4(b"verified on the host");
    good[..6].copy_from_slice(&MAC_ADDRESS);
    assert_eq!(header_flags(all, &good), offload::DATA_VALID);
    assert_eq!(header_flags(features::OFFERED, &good), 0, "not accepted");
    let mut bad = good.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert_eq!(header_flags(all, &bad), 0, "left for the guest to judge");
}

#[test]
fn a_link_change_reaches_link_and_raises_its_cause_once() {
    let mut rig = Rig::new(8, features::OFFERED);
    rig.step();
    assert!(!rig.net.due());
    rig.net.set_link(true);
    assert!(!rig.net.due(), "no change");
    rig.net.set_link(false);
    assert_eq!(rig.read(LINK), 1000 << 32);
    assert!(rig.net.due());
    rig.step();
    assert!(rig.line());
    assert_eq!(rig.read(CAUSE), CAUSE_LINK);
    rig.write(ACK, CAUSE_LINK).unwrap();
    assert!(!rig.line());
    assert!(!rig.net.due());
    // A stopped device only keeps LINK; the driver reads it as it starts.
    rig.write(ENABLE, 0).unwrap();
    rig.net.set_link(true);
    assert!(!rig.net.due());
    rig.write(ENABLE, 1).unwrap();
    assert_eq!(rig.read(LINK), 1000 << 32 | 1);
    rig.step();
    assert_eq!(rig.read(CAUSE), 0);
    assert!(!rig.line());
}

/// An Ethernet ARP frame from `sender` claiming `address`.
fn arp(sender: [u8; 6], address: [u8; 4], op: u8) -> Vec<u8> {
    let mut frame = vec![0u8; 42];
    frame[..6].copy_from_slice(&[0xff; 6]);
    frame[6..12].copy_from_slice(&sender);
    frame[12..22].copy_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0x00, 6, 4, 0, op]);
    frame[22..28].copy_from_slice(&sender);
    frame[28..32].copy_from_slice(&address);
    frame[38..42].copy_from_slice(&[10, 0, 69, 1]);
    frame
}

#[test]
fn the_guest_address_comes_from_arp_frames_it_sends() {
    let mut rig = Rig::new(8, features::OFFERED);
    let sent = |rig: &mut Rig, frame: Vec<u8>, slot| {
        rig.send(&frame, slot);
        rig.write(KICK, KICK_TX).unwrap();
        rig.step();
        assert_eq!(rig.net.take(), Some(frame));
    };
    // A probe claims no address, and another station's frame is not the
    // guest's.
    sent(&mut rig, arp(MAC_ADDRESS, [0; 4], 1), 0);
    sent(&mut rig, arp([0x02, 9, 9, 9, 9, 9], [10, 0, 69, 9], 1), 1);
    assert_eq!(rig.net.guest_address(), None);
    sent(&mut rig, arp(MAC_ADDRESS, [10, 0, 69, 74], 1), 2);
    assert_eq!(rig.net.guest_address(), Some([10, 0, 69, 74]));
    sent(&mut rig, arp(MAC_ADDRESS, [10, 0, 69, 80], 2), 3);
    assert_eq!(rig.net.guest_address(), Some([10, 0, 69, 80]));
    sent(&mut rig, frame([0xff; 6], 60, 0x08), 4);
    assert_eq!(rig.net.guest_address(), Some([10, 0, 69, 80]));
}
