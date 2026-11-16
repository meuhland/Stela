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
//!
//! With `MRG_RXBUF` a frame takes as many posted receive buffers as it
//! needs, `num_buffers` in its header naming them; with `GUEST_TSO4`/`6` as
//! well, in-order TCP segments of one flow that wait together are joined
//! into one frame ([`offload::Coalesced`]), so the guest's stack handles one
//! segment where the wire carried dozens.
pub mod offload;
/// The device side of its virtqueues, shared with the virtio devices.
pub use crate::virtio::queue;
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
    /// `FEATURES`: [`features::OFFERED`], with [`features::CHECKSUMS`] and
    /// [`features::COALESCING`] as the boot offers them.
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
    /// `LINK` bit 0: the host adapter's carrier, as the host's service last
    /// reported it ([`Net::set_link`]).
    link: bool,
    /// The link changed since the guest last heard: the next step raises
    /// `CAUSE_LINK`.
    link_changed: bool,
    /// The IPv4 address the guest last claimed as an ARP sender, for the
    /// boot log: how a run finds a guest whose address DHCP chose.
    address: Option<[u8; 4]>,
    /// A frame for the guest, its header made, that found too few posted
    /// buffers: it goes before the inbound queue once the guest posts more.
    held: Option<Held>,
    /// Storage for the next coalesced frame, from the last one placed.
    coalesce_spare: Option<Vec<u8>>,
    /// The chains a merged frame is placed in, kept for the next.
    chains: Vec<queue::Chain>,
    stats: Stats,
}

/// A frame waiting for receive buffers, as [`Net::receive`] left it.
struct Held {
    frame: Vec<u8>,
    header: [u8; HEADER_BYTES as usize],
    /// The frames from the host it carries.
    segments: u32,
}

/// What the device did for the guest, for the boot log: how often the
/// guest was interrupted and kicked for how many frames, and how many
/// frames receive coalescing spared its stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Times the line rose: interrupts the guest took.
    pub interrupts: u64,
    /// Receive publishes the guest asked to hear of (`CAUSE_RX`).
    pub rx_signals: u64,
    /// Transmit completions the guest asked to hear of (`CAUSE_TX`).
    pub tx_signals: u64,
    /// `KICK` writes: each a guest MMIO store.
    pub kicks: u64,
    /// Frames the guest received, each perhaps several from the host.
    pub rx_frames: u64,
    /// Frames that joined more than one TCP segment, and the segments
    /// they joined.
    pub coalesced: u64,
    pub coalesced_segments: u64,
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
            link: true,
            link_changed: false,
            address: None,
            held: None,
            coalesce_spare: None,
            chains: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// What the device did for the guest so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// The host adapter's carrier: `LINK` follows it, and a running device
    /// raises `CAUSE_LINK` at its next step when it changes.
    pub fn set_link(&mut self, up: bool) {
        if self.link != up {
            self.link = up;
            self.link_changed = true;
        }
    }

    /// The IPv4 address the guest last announced in an ARP frame it sent.
    pub fn guest_address(&self) -> Option<[u8; 4]> {
        self.address
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
        } else if frame.capacity() >= offload::COALESCED_FRAME_MAX && self.coalesce_spare.is_none()
        {
            self.coalesce_spare = Some(frame);
        }
    }

    /// Whether the guest sent frames since the host's service was last
    /// asked for; asking clears it.
    pub fn take_service_due(&mut self) -> bool {
        core::mem::take(&mut self.service_due)
    }

    /// Whether the DMA step has work: a kick, rings not yet checked, a link
    /// change to raise, frames for posted buffers, or transmit chains left
    /// for want of room that now has some.
    pub fn due(&self) -> bool {
        self.up()
            && (self.kicked != 0
                || !self.checked
                || self.link_changed
                || (self.held.is_some() || !self.inbound.is_empty()) && !self.starved
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
        self.held = None;
    }

    /// Stops the device for `why` and raises `CAUSE_ERROR`.
    fn fail(&mut self, why: &'static str, access: &mut Access<'_>) {
        self.stop();
        self.error = Some(why);
        self.raise(CAUSE_ERROR, access);
    }

    fn raise(&mut self, cause: u64, access: &mut Access<'_>) {
        if self.cause == 0 {
            self.stats.interrupts += 1;
        }
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
                // The driver reads LINK as it starts the device.
                self.link_changed = false;
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
        if core::mem::take(&mut self.link_changed) {
            self.raise(CAUSE_LINK, access);
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
                    if let Some(address) = arp_sender(&data, &self.mac) {
                        self.address = Some(address);
                    }
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
            self.stats.tx_signals += 1;
            self.raise(CAUSE_TX, access);
        }
        Ok(())
    }

    /// Copies frames for the guest into the receive buffers it posted, each
    /// after a `virtio_net_hdr_v1`: one buffer per frame, or with
    /// `MRG_RXBUF` as many as it needs. A frame that fits no posted buffer,
    /// or not the whole ring's, is dropped and the buffers kept; one that
    /// finds too few posted waits for the guest's next receive kick.
    fn receive(
        &mut self,
        rx: &mut Queue,
        memory: &mut dyn Memory,
        access: &mut Access<'_>,
    ) -> Result<(), RingError> {
        let event_idx = self.event_idx();
        let merged = self.accepted & features::MRG_RXBUF != 0;
        let mut moved = 0;
        while moved < STEP_FRAMES {
            let next = match self.held.take() {
                Some(held) => held,
                None => match self.inbound.pop_front() {
                    Some(frame) => self.prepare(frame),
                    None => break,
                },
            };
            let placed = if merged {
                self.place_merged(rx, memory, next.header, &next.frame)?
            } else {
                Self::place_single(rx, memory, &next.header, &next.frame)?
            };
            let segments = u64::from(next.segments);
            match placed {
                Placed::Written => {
                    self.counters.to_guest += segments;
                    self.stats.rx_frames += 1;
                    if segments > 1 {
                        self.stats.coalesced += 1;
                        self.stats.coalesced_segments += segments;
                    }
                    moved += 1;
                }
                Placed::NoRoom => {
                    self.held = Some(next);
                    self.starved = true;
                    break;
                }
                Placed::TooLong => {
                    self.counters.oversized += segments;
                    self.rx_drops += segments;
                }
            }
            self.recycle(next.frame);
        }
        if event_idx {
            if self.starved {
                // Buffers posted since the ring was last read, too few or
                // none, are not enough; the next ones are announced. Any
                // posted before the driver could see that are looked at
                // again without a kick.
                if rx.want_kick_past_seen(memory)? {
                    self.starved = false;
                }
            } else {
                rx.no_kick(memory)?;
            }
        }
        if rx.publish(memory, event_idx)? {
            self.stats.rx_signals += 1;
            self.raise(CAUSE_RX, access);
        }
        Ok(())
    }

    /// Whether the guest takes coalesced TCP segments over `ethertype`:
    /// with their checksums vouched for and room to span buffers.
    fn coalesces(&self, ethertype: [u8; 2]) -> bool {
        let family = match u16::from_be_bytes(ethertype) {
            0x0800 => features::GUEST_TSO4,
            0x86dd => features::GUEST_TSO6,
            _ => return false,
        };
        let needed = family | features::GUEST_CSUM | features::MRG_RXBUF;
        self.accepted & needed == needed
    }

    /// The header for `frame`, which the inbound queue gave up: its
    /// checksums vouched for when the host verified them and, when the
    /// guest takes coalesced segments, the frame joined with the inbound
    /// frames that continue its TCP flow.
    fn prepare(&mut self, frame: Vec<u8>) -> Held {
        let mut header = [0u8; HEADER_BYTES as usize];
        // num_buffers: one, unless placing the frame says otherwise.
        header[10..12].copy_from_slice(&1u16.to_le_bytes());
        let alone = |frame, header| Held {
            frame,
            header,
            segments: 1,
        };
        if self.accepted & features::GUEST_CSUM == 0 || !offload::verified(&frame) {
            return alone(frame, header);
        }
        header[0] = offload::DATA_VALID;
        if !self.coalesces([frame[12], frame[13]]) {
            return alone(frame, header);
        }
        let run = match offload::Coalesced::start(frame) {
            Ok(run) => run,
            Err(frame) => return alone(frame, header),
        };
        // Only a run the next frame continues is copied into storage that
        // holds a whole run.
        let continued = |run: &offload::Coalesced, next: Option<&Vec<u8>>| {
            next.is_some_and(|next| run.takes(next) && offload::verified(next))
        };
        if !continued(&run, self.inbound.front()) {
            return alone(run.finish().0, header);
        }
        let (first, _) = run.finish();
        let mut storage = self.coalesce_spare.take().unwrap_or_default();
        storage.clear();
        storage.reserve(offload::COALESCED_FRAME_MAX);
        storage.extend_from_slice(&first);
        self.recycle(first);
        let Ok(mut run) = offload::Coalesced::start(storage) else {
            unreachable!("the segment started a run before");
        };
        // The first continuation is verified already.
        let mut verified = true;
        while let Some(next) = self.inbound.front() {
            if !verified && !continued(&run, Some(next)) {
                break;
            }
            verified = false;
            let next = self.inbound.pop_front().unwrap();
            run.append(&next);
            self.recycle(next);
        }
        let segments = run.segments();
        let (frame, gso) = run.finish();
        if let Some(gso) = gso {
            header[1] = gso.gso_type;
            header[2..4].copy_from_slice(&gso.hdr_len.to_le_bytes());
            header[4..6].copy_from_slice(&gso.gso_size.to_le_bytes());
        }
        Held {
            frame,
            header,
            segments,
        }
    }

    /// Writes the frame after its header into the next posted chain.
    fn place_single(
        rx: &mut Queue,
        memory: &mut dyn Memory,
        header: &[u8; HEADER_BYTES as usize],
        frame: &[u8],
    ) -> Result<Placed, RingError> {
        let Some(chain) = rx.pop(memory)? else {
            return Ok(Placed::NoRoom);
        };
        if !chain.all(true) {
            return Err(RingError::Malformed(
                "a receive buffer the device may not write",
            ));
        }
        let length = HEADER_BYTES + frame.len() as u64;
        if chain.bytes() < length {
            rx.unpop(1);
            return Ok(Placed::TooLong);
        }
        chain.write(memory, 0, header)?;
        chain.write(memory, HEADER_BYTES, frame)?;
        rx.push_used(memory, chain.head, length as u32)?;
        Ok(Placed::Written)
    }

    /// Writes the header and the frame over as many posted chains as they
    /// fill (`MRG_RXBUF`), `num_buffers` naming them, each with a used
    /// entry of the bytes it got; the chains all go back while too few are
    /// posted.
    fn place_merged(
        &mut self,
        rx: &mut Queue,
        memory: &mut dyn Memory,
        mut header: [u8; HEADER_BYTES as usize],
        frame: &[u8],
    ) -> Result<Placed, RingError> {
        let length = HEADER_BYTES as usize + frame.len();
        let mut chains = core::mem::take(&mut self.chains);
        chains.clear();
        let mut room = 0;
        while room < length {
            let Some(chain) = rx.pop(memory)? else {
                break;
            };
            if !chain.all(true) {
                return Err(RingError::Malformed(
                    "a receive buffer the device may not write",
                ));
            }
            room += chain.bytes() as usize;
            chains.push(chain);
        }
        let placed = if room < length {
            rx.unpop(chains.len() as u16);
            if chains.len() == usize::from(rx.size()) {
                Placed::TooLong
            } else {
                Placed::NoRoom
            }
        } else if chains[0].bytes() < HEADER_BYTES {
            return Err(RingError::Malformed(
                "a receive buffer shorter than the header",
            ));
        } else {
            header[10..12].copy_from_slice(&(chains.len() as u16).to_le_bytes());
            let mut offset = 0;
            for chain in &chains {
                let take = (chain.bytes() as usize).min(length - offset);
                let mut written = 0;
                if offset == 0 {
                    chain.write(memory, 0, &header)?;
                    written = header.len();
                }
                let start = offset + written - header.len();
                let end = offset + take - header.len();
                chain.write(memory, written as u64, &frame[start..end])?;
                rx.push_used(memory, chain.head, take as u32)?;
                offset += take;
            }
            Placed::Written
        };
        self.chains = chains;
        Ok(placed)
    }
}

/// What became of a frame [`Net::receive`] tried to place.
enum Placed {
    Written,
    /// Too few buffers are posted: the frame waits.
    NoRoom,
    /// No posted buffer, or not the whole ring's, holds it: dropped.
    TooLong,
}

/// The sender's IPv4 address of an Ethernet ARP frame from `mac` (request,
/// reply or announcement), but not a probe's 0.0.0.0.
fn arp_sender(frame: &[u8], mac: &[u8; 6]) -> Option<[u8; 4]> {
    // Ethernet header, then htype 1, ptype IPv4, hlen 6, plen 4, an op,
    // the sender's hardware and protocol addresses.
    const ARP: [u8; 8] = [0x08, 0x06, 0, 1, 0x08, 0x00, 6, 4];
    if frame.len() < 42 || frame[12..20] != ARP || frame[22..28] != *mac {
        return None;
    }
    let address: [u8; 4] = frame[28..32].try_into().unwrap();
    (address != [0; 4]).then_some(address)
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
            LINK => LINK_MBPS << 32 | u64::from(self.link),
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
            KICK => {
                self.stats.kicks += 1;
                self.kicked |= value & (KICK_TX | KICK_RX);
            }
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
