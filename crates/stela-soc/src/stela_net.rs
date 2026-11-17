//! StelaNet: the paravirtual network adapter at `/arm-io/stela-net0`
//! (`stela_contracts::net`), the guest's network in place of the USB
//! Ethernet adapter when the boot asks for it.
//!
//! The register page only configures and kicks; frames move in the board's
//! DMA step ([`Net::step`]), which the device's own register writes and its
//! [`Net::due`] work bring about. What moves them is the [`engine`], which
//! the virtio-mmio network device (`crate::virtio::net`) runs too: transmit
//! chains the guest published are copied out of guest RAM into the outbound
//! queue the host's service takes ([`Net::take`]), and frames the service
//! delivers ([`Net::deliver_owned`]) are copied into receive buffers the
//! guest posted. Both queues bound memory and drop as the USB adapter's do.
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
pub mod engine;
pub mod offload;
/// The device side of its virtqueues, shared with the virtio devices.
pub use crate::virtio::queue;
#[cfg(test)]
mod tests;

pub use engine::{QUEUE_FRAMES, Stats};

use super::bus::{Access, Direction, MmioDevice, MmioError, Width, refuse};
use crate::usb::ecm::Counters;
use alloc::vec::Vec;
use engine::{Engine, Signal};
use queue::{Queue, RingError};
use stela_contracts::{
    bus::AdvancePolicy,
    dma::Memory,
    interrupt::{AicInput, Wire},
    net::{features, window::*},
};

const NAME: &str = "stela-net";

pub struct Net {
    irq: AicInput,
    /// `FEATURES`: [`features::OFFERED`], with [`features::CHECKSUMS`] and
    /// [`features::COALESCING`] as the boot offers them.
    offered: u64,
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
    /// The guest kicked (`KICK_` bits) since the last step.
    kicked: u64,
    /// Why the device last stopped itself.
    error: Option<&'static str>,
    /// `LINK` bit 0: the host adapter's carrier, as the host's service last
    /// reported it ([`Net::set_link`]).
    link: bool,
    /// The link changed since the guest last heard: the next step raises
    /// `CAUSE_LINK`.
    link_changed: bool,
    /// Times the line rose, and `KICK` writes.
    interrupts: u64,
    kicks: u64,
    /// The rings' work and the frames' queues.
    engine: Engine,
}

impl Net {
    pub fn new(mac: [u8; 6], irq: AicInput) -> Self {
        Self::with_features(mac, irq, features::OFFERED)
    }

    /// A device offering `offered`, which must hold [`features::REQUIRED`].
    pub fn with_features(mac: [u8; 6], irq: AicInput, offered: u64) -> Self {
        Self {
            irq,
            offered: offered | features::REQUIRED,
            control: 0,
            rx_ring: 0,
            rx_size: 0,
            tx_ring: 0,
            tx_size: 0,
            rx: None,
            tx: None,
            checked: false,
            cause: 0,
            kicked: 0,
            error: None,
            link: true,
            link_changed: false,
            interrupts: 0,
            kicks: 0,
            engine: Engine::new(mac),
        }
    }

    /// What the device did for the guest so far.
    pub fn stats(&self) -> Stats {
        Stats {
            interrupts: self.interrupts,
            kicks: self.kicks,
            ..self.engine.stats()
        }
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
        self.engine.guest_address()
    }

    /// Whether the guest runs the device: its link, as the host sees it.
    pub fn up(&self) -> bool {
        self.rx.is_some()
    }

    pub fn counters(&self) -> Counters {
        self.engine.counters()
    }

    /// Why the device last stopped itself, for the boot log.
    pub fn error(&self) -> Option<&'static str> {
        self.error
    }

    /// Whether the guest's receive queue has room for a frame from the host.
    pub fn has_room(&self) -> bool {
        self.engine.has_room()
    }

    /// A frame from the host for the guest: dropped by size, by the filter,
    /// while the device is stopped, or without room, as the USB adapter
    /// drops them.
    pub fn deliver_owned(&mut self, frame: Vec<u8>) {
        let up = self.up();
        self.engine.deliver_owned(frame, up);
    }

    /// The next frame the guest sent, for the host network.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        self.engine.take()
    }

    /// Storage the host link finished with.
    pub fn recycle(&mut self, frame: Vec<u8>) {
        self.engine.recycle(frame);
    }

    /// Whether the guest sent frames since the host's service was last
    /// asked for; asking clears it.
    pub fn take_service_due(&mut self) -> bool {
        self.engine.take_service_due()
    }

    /// Whether the DMA step has work: a kick, rings not yet checked, a link
    /// change to raise, frames for posted buffers, or transmit chains left
    /// for want of room that now has some.
    pub fn due(&self) -> bool {
        self.up()
            && (self.kicked != 0 || !self.checked || self.link_changed || self.engine.pending())
    }

    /// Stops the device: the rings are forgotten, so nothing reaches guest
    /// memory after this, and frames for the guest go.
    fn stop(&mut self) {
        self.rx = None;
        self.tx = None;
        self.checked = false;
        self.kicked = 0;
        self.engine.stop();
    }

    /// Stops the device for `why` and raises `CAUSE_ERROR`.
    fn fail(&mut self, why: &'static str, access: &mut Access<'_>) {
        self.stop();
        self.error = Some(why);
        self.raise(CAUSE_ERROR, access);
    }

    fn raise(&mut self, cause: u64, access: &mut Access<'_>) {
        raise(
            &mut self.cause,
            &mut self.interrupts,
            self.irq,
            cause,
            access,
        );
    }

    fn enable(&mut self, access: &mut Access<'_>) {
        if self.up() {
            return;
        }
        let accepted = self.engine.accepted();
        if accepted & !self.offered != 0 || accepted & features::REQUIRED != features::REQUIRED {
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
            let (cause, interrupts, irq) = (&mut self.cause, &mut self.interrupts, self.irq);
            self.engine.transfer(
                &mut rx,
                &mut tx,
                kicked & KICK_RX != 0,
                memory,
                &mut |signal| {
                    let bit = match signal {
                        Signal::Sent => CAUSE_TX,
                        Signal::Received => CAUSE_RX,
                    };
                    raise(cause, interrupts, irq, bit, access);
                },
            )
        })();
        // A failure stops the device: its rings stay forgotten.
        if result.is_ok() {
            self.rx = Some(rx);
            self.tx = Some(tx);
        }
        result
    }
}

/// Raises `bit` in `cause` and the line, counting the interrupt when it was
/// low.
fn raise(cause: &mut u64, interrupts: &mut u64, irq: AicInput, bit: u64, access: &mut Access<'_>) {
    if *cause == 0 {
        *interrupts += 1;
    }
    *cause |= bit;
    access.interrupts.set_level(Wire::Aic(irq), true);
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
        mac[..6].copy_from_slice(&self.engine.mac());
        Ok(match offset {
            VERSION => MAGIC << 32 | LAYOUT_VERSION,
            FEATURES => self.offered,
            ACCEPT => self.engine.accepted(),
            MAC => u64::from_le_bytes(mac),
            LINK => LINK_MBPS << 32 | u64::from(self.link),
            CONTROL => self.control,
            RX_RING => self.rx_ring,
            RX_SIZE => self.rx_size,
            TX_RING => self.tx_ring,
            TX_SIZE => self.tx_size,
            ENABLE => self.up().into(),
            CAUSE => self.cause,
            FILTER => self.engine.filter(),
            RX_FRAMES => self.engine.counters().to_guest,
            TX_FRAMES => self.engine.counters().to_host,
            RX_DROPS => self.engine.rx_drops(),
            TX_ERRORS => self.engine.tx_errors(),
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
            ACCEPT => self.engine.set_accepted(value),
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
                self.kicks += 1;
                self.kicked |= value & (KICK_TX | KICK_RX);
            }
            ACK => {
                self.cause &= !value;
                if self.cause == 0 {
                    access.interrupts.set_level(Wire::Aic(self.irq), false);
                }
            }
            FILTER => self.engine.set_filter(value),
            _ => return Err(refused("no such register")),
        }
        Ok(())
    }
}
