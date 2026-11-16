//! Virtio devices on the virtio-mmio version 2 transport
//! (`stela_contracts::virtio`), each one register page at
//! `/arm-io/stela-virtio<n>` and one AIC input, which Stela's own StelaVirtIO
//! kext drives.
//!
//! [`mmio::Mmio`] is the transport: the register page, the status and
//! feature negotiation, the queues' places, notifications and the level
//! interrupt. Behind it a [`VirtioDevice`] moves buffers on its queues in the
//! board's DMA step, as StelaNet's frames move: the guest's notifications
//! and the device's own [`VirtioDevice::due`] work, or its timed work at
//! [`VirtioDevice::deadline`] (the sound device's pacing), bring the step
//! about. The
//! queues are split virtqueues ([`queue`]), shared with StelaNet.
pub mod console;
pub mod mmio;
pub mod queue;
pub mod rng;
pub mod snd;
#[cfg(test)]
mod tests;

use alloc::{string::String, vec::Vec};
use queue::{Queue, RingError};
use stela_contracts::dma::Memory;

/// What a device's step did on its queues.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Served {
    /// Chains the device used.
    pub chains: u64,
    /// Bytes it wrote into them.
    pub bytes: u64,
    /// The driver asked to hear of a used chain: the transport raises
    /// `INT_VRING`.
    pub interrupt: bool,
}

/// A device type behind the transport: its identity, its queues and the
/// work it does on them. Its configuration space, if any, sits at the
/// page's `CONFIG`.
pub trait VirtioDevice {
    /// Its name in the boot log and in refusals, such as `virtio-rng`.
    fn name(&self) -> &'static str;
    /// Its device type (`stela_contracts::virtio::ids`).
    fn id(&self) -> u32;
    /// Its own feature bits; the transport adds `features::TRANSPORT`.
    fn features(&self) -> u64 {
        0
    }
    /// The most entries each of its queues takes, by index.
    fn queue_max(&self) -> &'static [u16];
    /// `bytes` of its configuration space from `offset`, `None` past it.
    fn config_read(&self, _offset: u64, _bytes: u64) -> Option<u64> {
        None
    }
    /// A driver's write to its configuration space; `false` refuses it.
    fn config_write(&mut self, _offset: u64, _bytes: u64, _value: u64) -> bool {
        false
    }
    /// The driver accepted `features` and set `DRIVER_OK`.
    fn activate(&mut self, _features: u64) {}
    /// The driver reset the device: it forgets what it was doing.
    fn reset(&mut self) {}
    /// Whether it has work without a notification.
    fn due(&self) -> bool {
        false
    }
    /// The guest counter tick its next timed work is due at, such as the
    /// sound device's next completion, `None` for none.
    fn deadline(&self) -> Option<u64> {
        None
    }
    /// Notes of its own for the boot log since the last call, logged as
    /// they are, such as the sound stream's state changes and level.
    fn take_notes(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Whether it has notes for [`Self::take_notes`].
    fn has_notes(&self) -> bool {
        false
    }
    /// Lines of its own for the boot log since the last call, such as the
    /// console's output.
    fn take_lines(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Whether it has lines for [`Self::take_lines`].
    fn has_lines(&self) -> bool {
        false
    }
    /// Counters of its own for the boot log's report, by name.
    fn counters(&self) -> Vec<(&'static str, u64)> {
        Vec::new()
    }
    /// Its DMA step on `queues` (`None` where a queue is not ready), the
    /// ones in `notified` (bit per index) told of new buffers, with
    /// `VIRTIO_F_EVENT_IDX` as negotiated, at counter tick `now`. An error
    /// stops the device.
    fn step(
        &mut self,
        queues: &mut [Option<Queue>],
        notified: u64,
        memory: &mut dyn Memory,
        event_idx: bool,
        now: u64,
    ) -> Result<Served, &'static str>;
}

/// Why a ring stopped the device, for its error.
pub fn ring_error(error: RingError) -> &'static str {
    match error {
        RingError::Memory => "a ring or buffer is not RAM",
        RingError::Malformed(why) => why,
    }
}

/// Every queue index `notified` names, lowest first.
pub fn notified_queues(notified: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |index| notified & 1 << index != 0)
}

/// The queues of a device, as the transport keeps them while they run.
pub type Queues = Vec<Option<Queue>>;
