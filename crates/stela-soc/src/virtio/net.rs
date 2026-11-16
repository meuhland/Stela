//! The network device (virtio 1.2, 5.1; `virtio_net.h`): a receive queue
//! (0) and a transmit queue (1), every buffer after a `virtio_net_hdr_v1`,
//! and a configuration space with the station address and the link state.
//! Its frames move through StelaNet's engine ([`Engine`]), so it completes
//! partial transmit checksums, vouches for received ones and coalesces
//! receive segments exactly as StelaNet does, when the driver accepts the
//! bits the boot offers.
//!
//! It is the guest's second network interface, beside StelaNet (or the USB
//! Ethernet adapter), with a station address of its own
//! ([`station_address`]): the board's network calls hand it the frames for
//! that address and every broadcast and multicast frame
//! (`machine::virtio::route`), and the host link sends what it transmits.
//! `STATUS` follows the host adapter's carrier, a change raising
//! `INT_CONFIG`. There is no control queue (`CTRL_VQ`): the device takes
//! frames to its address, broadcast and every multicast group, and a driver
//! has no promiscuous mode or multicast list to set.
use super::queue::Queue;
use super::{Served, VirtioDevice, ring_error};
use crate::stela_net::engine::{Engine, Stats};
use crate::usb::ecm::Counters;
use alloc::{vec, vec::Vec};
use stela_contracts::{
    dma::Memory,
    virtio::{
        ids,
        net::{self, features},
    },
};

/// The most entries each queue takes, as StelaNet's rings.
pub const QUEUE_MAX: u16 = stela_contracts::net::window::QUEUE_MAX as u16;
/// The offloads a boot may add to what the device always offers:
/// StelaNet's (`stela_contracts::net::features::CHECKSUMS` and
/// `COALESCING`), which number their bits as virtio-net does.
pub const OFFLOADS: u64 = features::CSUM
    | features::GUEST_CSUM
    | features::GUEST_TSO4
    | features::GUEST_TSO6
    | features::MRG_RXBUF;

const _: () = assert!(
    stela_contracts::net::features::CHECKSUMS | stela_contracts::net::features::COALESCING
        == OFFLOADS
);

/// A locally administered unicast station address for the board with
/// `ecid`, `02:53` ("S") then four bytes mixed from it, never `avoid` (the
/// guest's other interface's).
pub fn station_address(ecid: u64, avoid: Option<[u8; 6]>) -> [u8; 6] {
    let mut mixed = ecid ^ 0x5374_656c_6156_494f;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    let [a, b, c, d, ..] = mixed.to_le_bytes();
    let mut mac = [0x02, 0x53, a, b, c, d];
    if avoid == Some(mac) {
        mac[5] ^= 1;
    }
    mac
}

pub struct VirtioNet {
    engine: Engine,
    /// Its own feature bits: [`features::OFFERED`] and the boot's offloads.
    offered: u64,
    /// `STATUS_LINK_UP`: the host adapter's carrier, as the host's service
    /// last reported it.
    link: bool,
    /// The link changed since the driver last heard: the next step raises
    /// `INT_CONFIG`.
    link_changed: bool,
    /// The driver set `DRIVER_OK` and has not reset the device since.
    running: bool,
}

impl VirtioNet {
    /// A device with station address `mac`, offering `offloads` (bits of
    /// [`OFFLOADS`]; others are not offered) on top of what it always does.
    pub fn new(mac: [u8; 6], offloads: u64) -> Self {
        Self {
            engine: Engine::new(mac),
            offered: features::OFFERED | offloads & OFFLOADS,
            link: true,
            link_changed: false,
            running: false,
        }
    }

    pub fn mac(&self) -> [u8; 6] {
        self.engine.mac()
    }

    /// Whether the driver runs the device.
    pub fn up(&self) -> bool {
        self.running
    }

    /// The host adapter's carrier: `STATUS` follows it, and a change raises
    /// `INT_CONFIG` once the driver runs the device.
    pub fn set_link(&mut self, up: bool) {
        if self.link != up {
            self.link = up;
            self.link_changed = self.running;
        }
    }

    pub fn link(&self) -> bool {
        self.link
    }

    /// Whether the guest's receive queue has room for a frame from the host.
    pub fn has_room(&self) -> bool {
        self.engine.has_room()
    }

    /// A frame from the host for the guest, dropped as StelaNet's are.
    pub fn deliver_owned(&mut self, frame: Vec<u8>) {
        self.engine.deliver_owned(frame, self.running);
    }

    /// The next frame the guest sent, for the host network.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        self.engine.take()
    }

    /// Storage the host link finished with.
    pub fn recycle(&mut self, frame: Vec<u8>) {
        self.engine.recycle(frame);
    }

    pub fn counters(&self) -> Counters {
        self.engine.counters()
    }

    /// The signals and frames; the transport counts interrupts and
    /// notifications (`mmio::Stats`).
    pub fn stats(&self) -> Stats {
        self.engine.stats()
    }

    /// The IPv4 address the guest last announced in an ARP frame it sent.
    pub fn guest_address(&self) -> Option<[u8; 4]> {
        self.engine.guest_address()
    }

    /// The configuration space's bytes.
    fn config(&self) -> [u8; net::CONFIG_BYTES as usize] {
        let mut config = [0u8; net::CONFIG_BYTES as usize];
        let mac = net::CONFIG_MAC as usize;
        config[mac..mac + 6].copy_from_slice(&self.engine.mac());
        let status = if self.link { net::STATUS_LINK_UP } else { 0 } as u16;
        let at = net::CONFIG_STATUS as usize;
        config[at..at + 2].copy_from_slice(&status.to_le_bytes());
        let at = net::CONFIG_MAX_VIRTQUEUE_PAIRS as usize;
        config[at..at + 2].copy_from_slice(&1u16.to_le_bytes());
        config
    }
}

impl VirtioDevice for VirtioNet {
    fn name(&self) -> &'static str {
        "virtio-net"
    }

    fn id(&self) -> u32 {
        ids::NET as u32
    }

    fn features(&self) -> u64 {
        self.offered
    }

    fn queue_max(&self) -> &'static [u16] {
        &[QUEUE_MAX, QUEUE_MAX]
    }

    fn config_read(&self, offset: u64, bytes: u64) -> Option<u64> {
        let end = offset.checked_add(bytes)?;
        if !matches!(bytes, 1 | 2 | 4 | 8) || end > net::CONFIG_BYTES {
            return None;
        }
        let mut value = [0u8; 8];
        value[..bytes as usize].copy_from_slice(&self.config()[offset as usize..end as usize]);
        Some(u64::from_le_bytes(value))
    }

    fn activate(&mut self, features: u64) {
        self.engine.set_accepted(features);
        self.running = true;
        // The driver reads STATUS as it starts the device.
        self.link_changed = false;
    }

    fn reset(&mut self) {
        self.running = false;
        self.link_changed = false;
        self.engine.stop();
        self.engine.set_accepted(0);
    }

    fn due(&self) -> bool {
        self.running && (self.link_changed || self.engine.pending())
    }

    fn take_service_due(&mut self) -> bool {
        self.engine.take_service_due()
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        let counters = self.engine.counters();
        let stats = self.engine.stats();
        // The guest's IPv4 address from its ARP frames, as a number:
        // 0xc0a80105 is 192.168.1.5.
        let address = self.engine.guest_address().map_or(0, u32::from_be_bytes);
        vec![
            ("link_up", u64::from(self.link)),
            ("to_host", counters.to_host),
            ("to_guest", counters.to_guest),
            ("oversized", counters.oversized),
            ("filtered", counters.filtered),
            ("overflowed", counters.overflowed),
            ("malformed", counters.malformed),
            ("rx_frames", stats.rx_frames),
            ("coalesced", stats.coalesced),
            ("ipv4", u64::from(address)),
        ]
    }

    fn net(&mut self) -> Option<&mut VirtioNet> {
        Some(self)
    }

    fn net_ref(&self) -> Option<&VirtioNet> {
        Some(self)
    }

    fn step(
        &mut self,
        queues: &mut [Option<Queue>],
        notified: u64,
        memory: &mut dyn Memory,
        _event_idx: bool,
        _now: u64,
    ) -> Result<Served, &'static str> {
        let mut served = Served {
            config: core::mem::take(&mut self.link_changed),
            ..Served::default()
        };
        let [Some(rx), Some(tx)] = queues else {
            return Ok(served);
        };
        let before = self.engine.counters();
        let mut interrupt = false;
        self.engine
            .transfer(
                rx,
                tx,
                notified & 1 << net::RX_QUEUE != 0,
                memory,
                &mut |_| interrupt = true,
            )
            .map_err(ring_error)?;
        let after = self.engine.counters();
        served.interrupt = interrupt;
        served.chains = after.to_guest - before.to_guest + after.to_host - before.to_host;
        Ok(served)
    }
}

#[cfg(test)]
mod tests;
