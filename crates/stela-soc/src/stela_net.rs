//! StelaNet: the paravirtual network adapter at `/arm-io/stela-net0`
//! (`stela_contracts::net`), the guest's network in place of the USB
//! Ethernet adapter when the boot asks for it.
//!
//! The register page only configures and kicks; frames move in the board's
//! DMA step ([`Net::step`]), which the device's own register writes and its
//! [`Net::due`] work bring about. Transmit chains the guest published are
//! copied out of guest RAM into the outbound queue the host's service takes
//! ([`Net::take`]), and frames the service delivers ([`Net::deliver_owned`])
//! are copied into receive buffers the guest posted. Both queues bound
//! memory and drop as the USB adapter's do.
//!
//! The device raises its AIC input, a level, when the guest asked to hear of
//! used entries (`VIRTIO_F_EVENT_IDX`), and holds it until the guest
//! acknowledges every cause. A ring that breaks the format stops the device
//! with `CAUSE_ERROR` instead of stopping the machine.
pub mod offload;
pub mod queue;
#[cfg(test)]
mod tests;

use super::bus::{Access, Direction, MmioDevice, MmioError, Width, refuse};
use crate::usb::ecm::Counters;
use alloc::{collections::VecDeque, vec::Vec};
use queue::{Queue, RingError};
use stela_contracts::{
    bus::AdvancePolicy,
    dma::Memory,
    interrupt::{AicInput, Wire},
    net::{features, window::*},
};

const NAME: &str = "stela-net";
/// Frames each direction holds, as the NCM adapter's queues do.
pub const QUEUE_FRAMES: usize = 256;
/// Frames one step moves each way at most, bounding its work under the
/// devices' lock.
const STEP_FRAMES: usize = 256;
/// The filter a device starts with: what a driver that sets none expects.
const DEFAULT_FILTER: u64 = FILTER_DIRECTED | FILTER_BROADCAST | FILTER_ALL_MULTICAST;

pub struct Net {
    irq: AicInput,
    mac: [u8; 6],
    /// `FEATURES`: [`features::OFFERED`], with [`features::CHECKSUMS`] when
    /// the boot offers them.
    offered: u64,
    accepted: u64,
    control: u64,
    rx_ring: u64,
    rx_size: u64,
    tx_ring: u64,
    tx_size: u64,
    /// The rings while the device runs.
    rx: Option<Queue>,
    tx: Option<Queue>,
    /// Whether the running rings were checked to be RAM.
    checked: bool,
    cause: u64,
    filter: u64,
    /// Frames for the guest, and from it.
    inbound: VecDeque<Vec<u8>>,
    outbound: VecDeque<Vec<u8>>,
    /// Storage the host link gave back, for the next transmit copy.
    spare: Vec<Vec<u8>>,
    /// The guest kicked (`KICK_` bits) since the last step.
    kicked: u64,
    /// The last step found no receive buffer; a receive kick clears it.
    starved: bool,
    /// The last step left transmit chains for want of outbound room.
    blocked: bool,
    /// Outbound frames the host's service has not been asked to take.
    service_due: bool,
    counters: Counters,
    rx_drops: u64,
    tx_errors: u64,
    /// Why the device last stopped itself.
    error: Option<&'static str>,
}

impl Net {
    pub fn new(mac: [u8; 6], irq: AicInput) -> Self {
        Self::with_features(mac, irq, features::OFFERED)
    }

    /// A device offering `offered`, which must hold [`features::REQUIRED`].
    pub fn with_features(mac: [u8; 6], irq: AicInput, offered: u64) -> Self {
        Self {
            irq,
            mac,
            offered: offered | features::REQUIRED,
            accepted: 0,
            control: 0,
            rx_ring: 0,
            rx_size: 0,
            tx_ring: 0,
            tx_size: 0,
            rx: None,
            tx: None,
            checked: false,
            cause: 0,
            filter: DEFAULT_FILTER,
            inbound: VecDeque::new(),
            outbound: VecDeque::new(),
            spare: Vec::new(),
            kicked: 0,
            starved: false,
            blocked: false,
            service_due: false,
            counters: Counters::default(),
            rx_drops: 0,
            tx_errors: 0,
            error: None,
        }
    }

    /// Whether the guest runs the device: its link, as the host sees it.
    pub fn up(&self) -> bool {
        self.rx.is_some()
    }

    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// Why the device last stopped itself, for the boot log.
    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    fn event_idx(&self) -> bool {
        self.accepted & features::EVENT_IDX != 0
    }

    /// Whether the guest's receive queue has room for a frame from the host.
    pub fn has_room(&self) -> bool {
        self.inbound.len() < QUEUE_FRAMES
    }

    /// A frame from the host for the guest: dropped by size, by the filter,
    /// while the device is stopped, or without room, as the USB adapter
    /// drops them.
    pub fn deliver_owned(&mut self, frame: Vec<u8>) {
        if !(FRAME_MIN as usize..=FRAME_MAX as usize).contains(&frame.len()) {
            self.counters.oversized += 1;
            self.rx_drops += 1;
            return;
        }
        if !self.up() || !self.passes(&frame) {
            self.counters.filtered += 1;
            self.rx_drops += 1;
            return;
        }
        if !self.has_room() {
            self.counters.overflowed += 1;
            self.rx_drops += 1;
            return;
        }
        self.inbound.push_back(frame);
    }

    fn passes(&self, frame: &[u8]) -> bool {
        let destination: [u8; 6] = frame[..6].try_into().unwrap();
        let filter = self.filter;
        filter & FILTER_PROMISCUOUS != 0
            || if destination == [0xff; 6] {
                filter & FILTER_BROADCAST != 0
            } else if destination[0] & 1 != 0 {
                // No multicast list yet: the kext passes every group.
                filter & FILTER_ALL_MULTICAST != 0
            } else {
                filter & FILTER_DIRECTED != 0 && destination == self.mac
            }
    }

    /// The next frame the guest sent, for the host network.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        self.outbound.pop_front()
    }

    /// Storage the host link finished with.
    pub fn recycle(&mut self, mut frame: Vec<u8>) {
        if self.spare.len() < 8 && frame.capacity() <= 2 * FRAME_MAX as usize {
            frame.clear();
            self.spare.push(frame);
        }
    }

    /// Whether the guest sent frames since the host's service was last
    /// asked for; asking clears it.
    pub fn take_service_due(&mut self) -> bool {
        core::mem::take(&mut self.service_due)
    }

    /// Whether the DMA step has work: a kick, rings not yet checked, frames
    /// for posted buffers, or transmit chains left for want of room that
    /// now has some.
    pub fn due(&self) -> bool {
        self.up()
            && (self.kicked != 0
                || !self.checked
                || !self.inbound.is_empty() && !self.starved
                || self.blocked && self.outbound.len() < QUEUE_FRAMES)
    }

    /// Stops the device: the rings are forgotten, so nothing reaches guest
    /// memory after this, and frames for the guest go.
    fn stop(&mut self) {
        self.rx = None;
        self.tx = None;
        self.checked = false;
        self.kicked = 0;
        self.starved = false;
        self.blocked = false;
        self.inbound.clear();
    }

    /// Stops the device for `why` and raises `CAUSE_ERROR`.
    fn fail(&mut self, why: &'static str, access: &mut Access<'_>) {
        self.stop();
        self.error = Some(why);
        self.raise(CAUSE_ERROR, access);
    }

    fn raise(&mut self, cause: u64, access: &mut Access<'_>) {
        self.cause |= cause;
        access.interrupts.set_level(Wire::Aic(self.irq), true);
    }

    fn enable(&mut self, access: &mut Access<'_>) {
        if self.up() {
            return;
        }
        if self.accepted & !self.offered != 0
            || self.accepted & features::REQUIRED != features::REQUIRED
        {
            return self.fail("ACCEPT is not a subset of FEATURES with VERSION_1", access);
        }
        match (
            Queue::new(self.rx_ring, self.rx_size),
            Queue::new(self.tx_ring, self.tx_size),
        ) {
            (Ok(rx), Ok(tx)) => {
                self.rx = Some(rx);
                self.tx = Some(tx);
                self.error = None;
            }
            (Err(why), _) | (_, Err(why)) => self.fail(why, access),
        }
    }

    /// The DMA step: moves frames each way and raises the interrupt the
    /// guest asked for. A ring the guest broke stops the device.
    pub fn step(&mut self, memory: &mut dyn Memory, access: &mut Access<'_>) {
        if !self.up() {
            return;
        }
        if let Err(error) = self.transfer(memory, access) {
            let why = match error {
                RingError::Memory => "a ring or buffer is not RAM",
                RingError::Malformed(why) => why,
            };
            self.fail(why, access);
        }
    }

    fn transfer(
        &mut self,
        memory: &mut dyn Memory,
        access: &mut Access<'_>,
    ) -> Result<(), RingError> {
        let (Some(mut rx), Some(mut tx)) = (self.rx.take(), self.tx.take()) else {
            return Ok(());
        };
        let result = (|| {
            if !self.checked {
                rx.check(memory)?;
                tx.check(memory)?;
                self.checked = true;
            }
            let kicked = core::mem::take(&mut self.kicked);
            if kicked & KICK_RX != 0 {
                self.starved = false;
            }
            self.transmit(&mut tx, memory, access)?;
            self.receive(&mut rx, memory, access)
        })();
        // A failure stops the device: its rings stay forgotten.
        if result.is_ok() {
            self.rx = Some(rx);
            self.tx = Some(tx);
        }
        result
    }

    /// Copies the transmit chains the guest published into the outbound
    /// queue while it has room, completing each, then asks for the next
    /// kick: at once when every chain was taken, otherwise none until
    /// [`Self::due`] finds room.
    fn transmit(
        &mut self,
        tx: &mut Queue,
        memory: &mut dyn Memory,
        access: &mut Access<'_>,
    ) -> Result<(), RingError> {
        let event_idx = self.event_idx();
        let mut moved = 0;
        self.blocked = false;
        while moved < STEP_FRAMES {
            if self.outbound.len() >= QUEUE_FRAMES {
                self.blocked = tx.pending(memory)? != 0;
                break;
            }
            let Some(chain) = tx.pop(memory)? else {
                break;
            };
            moved += 1;
            let bytes = chain.bytes();
            let frame = bytes.saturating_sub(HEADER_BYTES);
            if !chain.all(false) || !(FRAME_MIN..=FRAME_MAX).contains(&frame) {
                // The chain goes back unsent; the guest reclaims it.
                self.tx_errors += 1;
                self.counters.malformed += 1;
            } else {
                let mut header = [0u8; HEADER_BYTES as usize];
                chain.read(memory, 0, &mut header)?;
                let header = offload::Header::parse(&header);
                let mut data = self.spare.pop().unwrap_or_default();
                data.resize(frame as usize, 0);
                chain.read(memory, HEADER_BYTES, &mut data)?;
                // A partial checksum is completed only when CSUM was
                // accepted; a driver that marks one otherwise breaks the
                // contract and its frame goes as it is.
                let checksummed = header.flags & offload::NEEDS_CSUM == 0
                    || self.accepted & features::CSUM == 0
                    || offload::complete(&mut data, header.csum_start, header.csum_offset)
                        .is_some();
                if header.gso_type != offload::GSO_NONE || !checksummed {
                    self.tx_errors += 1;
                    self.counters.malformed += 1;
                    self.recycle(data);
                } else {
                    self.outbound.push_back(data);
                    self.counters.to_host += 1;
                    self.service_due = true;
                }
            }
            tx.push_used(memory, chain.head, 0)?;
        }
        if event_idx {
            if self.blocked || moved == STEP_FRAMES {
                tx.no_kick(memory)?;
            } else {
                tx.want_kick(memory)?;
            }
        }
        if moved == STEP_FRAMES {
            // The rest go at the next step, which a kick need not bring.
            self.blocked = tx.pending(memory)? != 0;
        }
        if tx.publish(memory, event_idx)? {
            self.raise(CAUSE_TX, access);
        }
        Ok(())
    }

    /// Copies frames for the guest into the receive buffers it posted, each
    /// after a `virtio_net_hdr_v1` naming one buffer. A frame that fits no
    /// posted buffer is dropped and the buffer kept.
    fn receive(
        &mut self,
        rx: &mut Queue,
        memory: &mut dyn Memory,
        access: &mut Access<'_>,
    ) -> Result<(), RingError> {
        let event_idx = self.event_idx();
        let mut moved = 0;
        while moved < STEP_FRAMES {
            let Some(frame) = self.inbound.pop_front() else {
                break;
            };
            let Some(chain) = rx.pop(memory)? else {
                self.inbound.push_front(frame);
                self.starved = true;
                break;
            };
            if !chain.all(true) {
                return Err(RingError::Malformed(
                    "a receive buffer the device may not write",
                ));
            }
            let length = HEADER_BYTES + frame.len() as u64;
            if chain.bytes() < length {
                rx.unpop();
                self.counters.oversized += 1;
                self.rx_drops += 1;
                self.recycle(frame);
                continue;
            }
            let mut header = [0u8; HEADER_BYTES as usize];
            if self.accepted & features::GUEST_CSUM != 0 && offload::verified(&frame) {
                header[0] = offload::DATA_VALID;
            }
            // num_buffers: one buffer holds the frame.
            header[10..12].copy_from_slice(&1u16.to_le_bytes());
            chain.write(memory, 0, &header)?;
            chain.write(memory, HEADER_BYTES, &frame)?;
            rx.push_used(memory, chain.head, length as u32)?;
            self.counters.to_guest += 1;
            self.recycle(frame);
            moved += 1;
        }
        if event_idx {
            if self.starved {
                rx.want_kick(memory)?;
            } else {
                rx.no_kick(memory)?;
            }
        }
        if rx.publish(memory, event_idx)? {
            self.raise(CAUSE_RX, access);
        }
        Ok(())
    }
}

impl MmioDevice for Net {
    fn advance_policy(&self) -> AdvancePolicy {
        AdvancePolicy::Passive
    }

    fn name(&self) -> &'static str {
        NAME
    }

    fn register_name(&self, offset: u64) -> Option<&'static str> {
        Some(match offset {
            VERSION => "VERSION",
            FEATURES => "FEATURES",
            ACCEPT => "ACCEPT",
            MAC => "MAC",
            LINK => "LINK",
            CONTROL => "CONTROL",
            RX_RING => "RX_RING",
            RX_SIZE => "RX_SIZE",
            TX_RING => "TX_RING",
            TX_SIZE => "TX_SIZE",
            ENABLE => "ENABLE",
            KICK => "KICK",
            CAUSE => "CAUSE",
            ACK => "ACK",
            FILTER => "FILTER",
            RX_FRAMES => "RX_FRAMES",
            TX_FRAMES => "TX_FRAMES",
            RX_DROPS => "RX_DROPS",
            TX_ERRORS => "TX_ERRORS",
            _ => return None,
        })
    }

    fn read(&mut self, offset: u64, width: Width, _: &mut Access<'_>) -> Result<u64, MmioError> {
        if width != Width::Double {
            return Err(refuse(
                NAME,
                offset,
                width,
                Direction::Read,
                "registers are 64-bit",
            ));
        }
        let mut mac = [0u8; 8];
        mac[..6].copy_from_slice(&self.mac);
        Ok(match offset {
            VERSION => MAGIC << 32 | LAYOUT_VERSION,
            FEATURES => self.offered,
            ACCEPT => self.accepted,
            MAC => u64::from_le_bytes(mac),
            LINK => LINK_MBPS << 32 | 1,
            CONTROL => self.control,
            RX_RING => self.rx_ring,
            RX_SIZE => self.rx_size,
            TX_RING => self.tx_ring,
            TX_SIZE => self.tx_size,
            ENABLE => self.up().into(),
            CAUSE => self.cause,
            FILTER => self.filter,
            RX_FRAMES => self.counters.to_guest,
            TX_FRAMES => self.counters.to_host,
            RX_DROPS => self.rx_drops,
            TX_ERRORS => self.tx_errors,
            _ => {
                return Err(refuse(
                    NAME,
                    offset,
                    width,
                    Direction::Read,
                    "no such register",
                ));
            }
        })
    }

    fn write(
        &mut self,
        offset: u64,
        width: Width,
        value: u64,
        access: &mut Access<'_>,
    ) -> Result<(), MmioError> {
        let refused = |reason| refuse(NAME, offset, width, Direction::Write, reason);
        if width != Width::Double {
            return Err(refused("registers are 64-bit"));
        }
        // The rings' places hold while the device runs.
        let configuring = matches!(
            offset,
            ACCEPT | CONTROL | RX_RING | RX_SIZE | TX_RING | TX_SIZE
        );
        if configuring && self.up() {
            return Err(refused("the device runs: stop it first"));
        }
        match offset {
            ACCEPT => self.accepted = value,
            CONTROL => self.control = value,
            RX_RING => self.rx_ring = value,
            RX_SIZE => self.rx_size = value,
            TX_RING => self.tx_ring = value,
            TX_SIZE => self.tx_size = value,
            ENABLE => match value {
                0 => self.stop(),
                1 => self.enable(access),
                _ => return Err(refused("ENABLE takes 0 or 1")),
            },
            KICK => self.kicked |= value & (KICK_TX | KICK_RX),
            ACK => {
                self.cause &= !value;
                if self.cause == 0 {
                    access.interrupts.set_level(Wire::Aic(self.irq), false);
                }
            }
            FILTER => self.filter = value,
            _ => return Err(refused("no such register")),
        }
        Ok(())
    }
}
