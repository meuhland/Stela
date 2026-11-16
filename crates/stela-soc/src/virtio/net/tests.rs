//! The network device through the virtio-mmio register page, as the kext
//! drives it, on RAM a test owns: the configuration space, frames each way
//! with their headers, merged receive buffers, the link and resets.
use super::*;
use crate::bus::{Access, MmioDevice, Width};
use crate::virtio::mmio::{Event, Mmio};
use alloc::{boxed::Box, vec, vec::Vec};
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::{AicInput, InterruptLatch, Wire};
use stela_contracts::virtio::{features as transport, mmio::*, ring, status};

const RAM: u64 = 0x8_0000_0000;
const IRQ: AicInput = AicInput::new(0, 0x2a4);
const MAC: [u8; 6] = [0x02, 0x53, 1, 2, 3, 4];
const OTHER: [u8; 6] = [0xbc, 0x24, 0x11, 0xbc, 0xf8, 0x0b];
/// Each queue's areas: descriptors, then the available ring at +0x1000 and
/// the used ring at +0x2000, as the kext lays them out in one page.
const QUEUES: [u64; 2] = [RAM, RAM + 0x4000];
const BUFFERS: u64 = RAM + 0x1_0000;
const SIZE: u16 = 8;
const HEADER: usize = net::HEADER_BYTES as usize;

/// One queue as the driver sees it.
struct Ring {
    base: u64,
    avail: u16,
    used: u16,
    /// The next descriptor a chain takes.
    next: u16,
}

impl Ring {
    fn desc(&self) -> u64 {
        self.base
    }
    fn avail_area(&self) -> u64 {
        self.base + 0x1000
    }
    fn used_area(&self) -> u64 {
        self.base + 0x2000
    }
}

struct Bench {
    device: Mmio,
    ram: Ram,
    latch: InterruptLatch,
    rings: [Ring; 2],
}

impl Bench {
    fn new(offloads: u64) -> Self {
        Self {
            device: Mmio::new(Box::new(VirtioNet::new(MAC, offloads)), IRQ, None),
            ram: Ram::new(RAM, 0x4_0000),
            latch: InterruptLatch::new(),
            rings: QUEUES.map(|base| Ring {
                base,
                avail: 0,
                used: 0,
                next: 0,
            }),
        }
    }

    fn net(&mut self) -> &mut VirtioNet {
        self.device.device_mut().net().unwrap()
    }

    fn read_width(&mut self, register: u64, width: Width) -> Option<u64> {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.device.read(register, width, &mut access).ok()
    }

    fn read(&mut self, register: u64) -> u32 {
        self.read_width(register, Width::Word).unwrap() as u32
    }

    fn write(&mut self, register: u64, value: u32) {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.device
            .write(register, Width::Word, u64::from(value), &mut access)
            .unwrap();
    }

    fn step(&mut self) {
        let mut access = Access::new(0, 0, &mut self.latch);
        self.device.step(&mut self.ram, &mut access);
    }

    fn line(&self) -> bool {
        self.latch.is_asserted(Wire::Aic(IRQ))
    }

    fn acknowledge(&mut self) -> u32 {
        let status = self.read(INTERRUPT_STATUS);
        self.write(INTERRUPT_ACK, status);
        status
    }

    fn offered(&mut self) -> u64 {
        let mut offered = 0;
        for select in 0..2 {
            self.write(DEVICE_FEATURES_SEL, select);
            offered |= u64::from(self.read(DEVICE_FEATURES)) << (32 * select);
        }
        offered
    }

    /// The driver's start: reset, `accepted`, both queues of SIZE entries,
    /// DRIVER_OK, and the first step that checks the queues.
    fn start(&mut self, accepted: u64) {
        self.write(STATUS, 0);
        self.write(STATUS, (status::ACKNOWLEDGE | status::DRIVER) as u32);
        for select in 0..2 {
            self.write(DRIVER_FEATURES_SEL, select);
            self.write(DRIVER_FEATURES, (accepted >> (32 * select)) as u32);
        }
        let now = self.read(STATUS);
        self.write(STATUS, now | status::FEATURES_OK as u32);
        assert_ne!(self.read(STATUS) & status::FEATURES_OK as u32, 0);
        for (index, base) in QUEUES.into_iter().enumerate() {
            // Fresh rings, as a driver makes them after a reset.
            assert!(self.ram.write(base, &[0; 0x3000]));
            self.rings[index] = Ring {
                base,
                avail: 0,
                used: 0,
                next: 0,
            };
            self.write(QUEUE_SEL, index as u32);
            self.write(QUEUE_NUM, u32::from(SIZE));
            for (low, address) in [
                (QUEUE_DESC_LOW, base),
                (QUEUE_AVAIL_LOW, base + 0x1000),
                (QUEUE_USED_LOW, base + 0x2000),
            ] {
                self.write(low, address as u32);
                self.write(low + 4, (address >> 32) as u32);
            }
            self.write(QUEUE_READY, 1);
            assert_eq!(self.read(QUEUE_READY), 1);
            // Interrupts for every used entry.
            let ring = &self.rings[index];
            let event = ring.avail_area() + 4 + 2 * u64::from(SIZE);
            assert!(self.ram.write(event, &0u16.to_le_bytes()));
        }
        let now = self.read(STATUS);
        self.write(STATUS, now | status::DRIVER_OK as u32);
        self.step();
    }

    /// Makes one chain of `buffers` (address, bytes) available on `queue`,
    /// readable or `write`able, published, and notifies the device.
    fn offer(&mut self, queue: usize, buffers: &[(u64, u32)], write: bool) -> u16 {
        let ring = &mut self.rings[queue];
        // Each chain takes the next descriptors; the tests use fewer than
        // the ring has before the device returns them.
        let first = ring.next % SIZE;
        ring.next = ring.next.wrapping_add(buffers.len() as u16);
        let mut descriptor = first;
        for (index, &(address, bytes)) in buffers.iter().enumerate() {
            let last = index + 1 == buffers.len();
            let next = (descriptor + 1) % SIZE;
            let mut flags = if last { 0 } else { ring::DESC_F_NEXT };
            if write {
                flags |= ring::DESC_F_WRITE;
            }
            let entry = [
                address.to_le_bytes().as_slice(),
                &bytes.to_le_bytes(),
                &flags.to_le_bytes(),
                &(if last { 0 } else { next }).to_le_bytes(),
            ]
            .concat();
            assert!(
                self.ram
                    .write(ring.desc() + 16 * u64::from(descriptor), &entry)
            );
            descriptor = next;
        }
        let slot = u64::from(ring.avail % SIZE);
        assert!(
            self.ram
                .write(ring.avail_area() + 4 + 2 * slot, &first.to_le_bytes())
        );
        ring.avail = ring.avail.wrapping_add(1);
        assert!(
            self.ram
                .write(ring.avail_area() + 2, &ring.avail.to_le_bytes())
        );
        self.write(QUEUE_NOTIFY, queue as u32);
        first
    }

    /// The used entries of `queue` since the last call, as (id, len).
    fn reap(&mut self, queue: usize) -> Vec<(u32, u32)> {
        let ring = &mut self.rings[queue];
        let mut index = [0; 2];
        assert!(self.ram.read(ring.used_area() + 2, &mut index));
        let index = u16::from_le_bytes(index);
        let mut entries = Vec::new();
        while ring.used != index {
            let slot = u64::from(ring.used % SIZE);
            let mut element = [0; 8];
            assert!(self.ram.read(ring.used_area() + 4 + 8 * slot, &mut element));
            entries.push((
                u32::from_le_bytes(element[..4].try_into().unwrap()),
                u32::from_le_bytes(element[4..].try_into().unwrap()),
            ));
            ring.used = ring.used.wrapping_add(1);
        }
        entries
    }

    fn bytes(&mut self, address: u64, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        assert!(self.ram.read(address, &mut bytes));
        bytes
    }
}

fn frame(to: [u8; 6], length: usize, fill: u8) -> Vec<u8> {
    let mut frame = vec![fill; length];
    frame[..6].copy_from_slice(&to);
    frame[6..12].copy_from_slice(&OTHER);
    frame
}

const ACCEPTED: u64 =
    transport::VERSION_1 | transport::EVENT_IDX | features::MAC | features::STATUS;

#[test]
fn the_page_identifies_a_network_device_with_its_address_and_link() {
    let mut bench = Bench::new(0);
    assert_eq!(bench.read(DEVICE_ID), ids::NET as u32);
    assert_eq!(
        bench.offered(),
        features::MAC | features::STATUS | transport::TRANSPORT
    );
    let offloads =
        stela_contracts::net::features::CHECKSUMS | stela_contracts::net::features::COALESCING;
    assert_eq!(
        Bench::new(offloads | features::HOST_TSO4).offered(),
        features::OFFERED | OFFLOADS | transport::TRANSPORT,
        "every StelaNet offload, and no host segmentation"
    );
    for queue in 0..2 {
        bench.write(QUEUE_SEL, queue);
        assert_eq!(bench.read(QUEUE_NUM_MAX), u32::from(QUEUE_MAX));
    }
    bench.write(QUEUE_SEL, 2);
    assert_eq!(bench.read(QUEUE_NUM_MAX), 0, "no control queue");
    // The configuration space: the address byte by byte, the link, one pair.
    for (index, byte) in MAC.into_iter().enumerate() {
        assert_eq!(
            bench.read_width(CONFIG + index as u64, Width::Byte),
            Some(u64::from(byte))
        );
    }
    assert_eq!(
        bench.read_width(CONFIG + net::CONFIG_STATUS, Width::Half),
        Some(net::STATUS_LINK_UP)
    );
    assert_eq!(
        bench.read_width(CONFIG + net::CONFIG_MAX_VIRTQUEUE_PAIRS, Width::Half),
        Some(1)
    );
    assert_eq!(
        bench.read_width(CONFIG, Width::Word),
        Some(u64::from(u32::from_le_bytes(MAC[..4].try_into().unwrap())))
    );
    assert_eq!(bench.read_width(CONFIG + 8, Width::Word), None, "past it");
    assert_eq!(bench.read_width(CONFIG + 10, Width::Byte), None);
    // Read only.
    let mut access = Access::new(0, 0, &mut bench.latch);
    assert!(
        bench
            .device
            .write(CONFIG, Width::Byte, 0, &mut access)
            .is_err()
    );
}

#[test]
fn a_transmitted_frame_goes_to_the_host_and_its_chain_completes() {
    let mut bench = Bench::new(0);
    bench.start(ACCEPTED);
    assert!(!bench.line());
    let sent = frame(OTHER, 64, 0x5a);
    // The header and the frame in two readable buffers.
    assert!(bench.ram.write(BUFFERS, &[0; HEADER]));
    assert!(bench.ram.write(BUFFERS + 0x100, &sent));
    let head = bench.offer(
        1,
        &[
            (BUFFERS, HEADER as u32),
            (BUFFERS + 0x100, sent.len() as u32),
        ],
        false,
    );
    assert!(bench.device.due());
    bench.step();
    assert!(bench.device.take_service_due());
    assert_eq!(bench.net().take().as_deref(), Some(sent.as_slice()));
    assert_eq!(bench.reap(1), [(u32::from(head), 0)]);
    assert!(bench.line(), "the driver asked for every used entry");
    assert_eq!(bench.acknowledge(), INT_VRING as u32);
    assert!(!bench.line());
    assert_eq!(bench.net().counters().to_host, 1);

    // Host segmentation was never offered: such a frame completes unsent.
    let mut header = [0u8; HEADER];
    header[net::HDR_GSO_TYPE as usize] = 1;
    assert!(bench.ram.write(BUFFERS + 0x200, &header));
    assert!(bench.ram.write(BUFFERS + 0x200 + HEADER as u64, &sent));
    bench.offer(1, &[(BUFFERS + 0x200, (HEADER + sent.len()) as u32)], false);
    bench.step();
    assert_eq!(bench.net().take(), None);
    assert_eq!(bench.net().counters().malformed, 1);
    assert_eq!(bench.reap(1).len(), 1);
}

#[test]
fn a_partial_transmit_checksum_is_completed_when_csum_was_accepted() {
    let mut bench = Bench::new(OFFLOADS);
    bench.start(ACCEPTED | features::CSUM);
    // A UDP datagram over IPv4 whose checksum field holds the pseudo
    // header's sum, as a stack that leaves it partial writes it.
    let mut sent = frame(OTHER, 14 + 20 + 8 + 4, 0);
    sent[12..14].copy_from_slice(&[0x08, 0x00]);
    sent[14] = 0x45;
    sent[23] = 17;
    sent[14 + 20 + 4..14 + 20 + 6].copy_from_slice(&12u16.to_be_bytes());
    sent[14 + 20 + 8..].copy_from_slice(&[1, 2, 3, 4]);
    let mut header = [0u8; HEADER];
    header[0] = net::HDR_F_NEEDS_CSUM as u8;
    header[6..8].copy_from_slice(&34u16.to_le_bytes());
    header[8..10].copy_from_slice(&6u16.to_le_bytes());
    assert!(bench.ram.write(BUFFERS, &header));
    assert!(bench.ram.write(BUFFERS + HEADER as u64, &sent));
    bench.offer(1, &[(BUFFERS, (HEADER + sent.len()) as u32)], false);
    bench.step();
    let taken = bench.net().take().unwrap();
    assert_ne!(taken[40..42], [0, 0], "the checksum was written");
    assert_eq!(taken[..40], sent[..40]);
}

#[test]
fn a_received_frame_lands_after_its_header_and_interrupts() {
    let mut bench = Bench::new(0);
    // Not running yet: dropped.
    bench.net().deliver_owned(frame(MAC, 60, 1));
    assert_eq!(bench.net().counters().filtered, 1);
    bench.start(ACCEPTED);
    let head = bench.offer(0, &[(BUFFERS, 2048)], true);
    bench.step();
    assert_eq!(bench.reap(0), []);
    let received = frame(MAC, 60, 0x33);
    bench.net().deliver_owned(received.clone());
    // Another station's unicast is not this device's.
    bench.net().deliver_owned(frame(OTHER, 60, 0x44));
    assert!(bench.device.due());
    bench.step();
    assert_eq!(bench.reap(0), [(u32::from(head), (HEADER + 60) as u32)]);
    let header = bench.bytes(BUFFERS, HEADER);
    assert_eq!(header[..10], [0; 10]);
    assert_eq!(header[10..], 1u16.to_le_bytes(), "num_buffers");
    assert_eq!(bench.bytes(BUFFERS + HEADER as u64, 60), received);
    assert!(bench.line());
    assert_eq!(bench.acknowledge(), INT_VRING as u32);
    let counters = bench.net().counters();
    assert_eq!((counters.to_guest, counters.filtered), (1, 2));
    // Broadcast passes.
    bench.offer(0, &[(BUFFERS + 0x1000, 2048)], true);
    bench.net().deliver_owned(frame([0xff; 6], 60, 0x55));
    bench.step();
    assert_eq!(bench.reap(0).len(), 1);
}

#[test]
fn a_frame_spans_merged_receive_buffers() {
    let mut bench = Bench::new(OFFLOADS);
    bench.start(ACCEPTED | features::MRG_RXBUF);
    let buffers: Vec<u64> = (0..4).map(|slot| BUFFERS + 0x1000 * slot).collect();
    for &buffer in &buffers {
        bench.offer(0, &[(buffer, 512)], true);
    }
    let received = frame(MAC, 1514, 0x77);
    bench.net().deliver_owned(received.clone());
    bench.step();
    let used = bench.reap(0);
    assert_eq!(used.len(), 3, "12 + 1514 bytes in 512-byte buffers");
    assert_eq!(
        used.iter().map(|&(_, length)| length).collect::<Vec<_>>(),
        [512, 512, 502]
    );
    let header = bench.bytes(buffers[0], HEADER);
    assert_eq!(header[10..], 3u16.to_le_bytes(), "num_buffers");
    let mut landed = bench.bytes(buffers[0] + HEADER as u64, 512 - HEADER);
    landed.extend(bench.bytes(buffers[1], 512));
    landed.extend(bench.bytes(buffers[2], 502));
    assert_eq!(landed, received);
}

#[test]
fn the_link_follows_the_host_and_a_change_raises_a_configuration_interrupt() {
    let mut bench = Bench::new(0);
    // Before the driver runs the device, a change raises nothing.
    bench.net().set_link(false);
    assert_eq!(
        bench.read_width(CONFIG + net::CONFIG_STATUS, Width::Half),
        Some(0)
    );
    bench.start(ACCEPTED);
    assert!(!bench.line());
    let generation = bench.read(CONFIG_GENERATION);
    bench.net().set_link(true);
    assert!(bench.device.due());
    bench.step();
    assert!(bench.line());
    assert_eq!(bench.acknowledge(), INT_CONFIG as u32);
    assert_ne!(bench.read(CONFIG_GENERATION), generation);
    assert_eq!(
        bench.read_width(CONFIG + net::CONFIG_STATUS, Width::Half),
        Some(net::STATUS_LINK_UP)
    );
    assert!(!bench.device.due());
}

#[test]
fn a_reset_stops_the_device_and_drops_what_waited_for_the_guest() {
    let mut bench = Bench::new(0);
    bench.start(ACCEPTED);
    bench.net().deliver_owned(frame(MAC, 60, 1));
    assert!(bench.net().up());
    bench.write(STATUS, 0);
    assert!(!bench.net().up());
    let (events, _) = bench.device.take_events();
    assert!(events.contains(&Event::Reset));
    // Started again, the frame from before is gone.
    bench.start(ACCEPTED);
    assert!(!bench.device.due());
    bench.offer(0, &[(BUFFERS, 2048)], true);
    bench.step();
    assert_eq!(bench.reap(0), []);
    // A receive buffer the device may not write is a broken ring: the
    // device stops and the driver hears it must reset it.
    bench.offer(0, &[(BUFFERS + 0x1000, 2048)], false);
    bench.net().deliver_owned(frame(MAC, 60, 2));
    bench.net().deliver_owned(frame(MAC, 60, 3));
    bench.step();
    bench.step();
    assert_ne!(bench.read(STATUS) & status::DEVICE_NEEDS_RESET as u32, 0);
    assert!(bench.device.error().is_some());
    assert_ne!(bench.acknowledge() & INT_CONFIG as u32, 0);
}

#[test]
fn the_summary_names_the_address_link_and_counts() {
    let bench = Bench::new(0);
    let summary = bench.device.summary().unwrap();
    assert!(
        summary.starts_with("mac=02:53:01:02:03:04 link=up to_host=0"),
        "{summary}"
    );
}

#[test]
fn a_station_address_is_local_unicast_stable_and_never_the_other_interfaces() {
    let one = station_address(0x0011_2233_4455_6677, None);
    assert_eq!(one, station_address(0x0011_2233_4455_6677, None));
    assert_eq!(one[..2], [0x02, 0x53]);
    assert_eq!(one[0] & 3, 2, "locally administered unicast");
    assert_ne!(one, station_address(0x0011_2233_4455_6678, None));
    let other = station_address(0x0011_2233_4455_6677, Some(one));
    assert_ne!(other, one);
    assert_eq!(other[..5], one[..5]);
}
