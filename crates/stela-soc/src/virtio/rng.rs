//! The entropy device (virtio 1.2, 5.4): one request queue whose writable
//! buffers the device fills from the host's random source. No features and
//! no configuration space.
//!
//! The source is the caller's, as the SEP model's entropy is
//! (`sep::Entropy`): `stela.efi` gives the boot's seeded cryptographic
//! stream. A source that fails stops the device rather than give the guest
//! bytes that are not random.
use super::queue::Queue;
use super::{Served, VirtioDevice, ring_error};
use alloc::{boxed::Box, vec, vec::Vec};
use stela_contracts::{dma::Memory, virtio::ids};

/// The most entries the request queue takes.
pub const QUEUE_MAX: u16 = 256;
/// The bytes one chain gets at most: a driver asking for more gets this.
pub const CHAIN_BYTES: usize = 4096;
/// Chains one step fills at most, bounding its work under the devices' lock.
const STEP_CHAINS: usize = 64;

/// The host's random bytes: fills the buffer, or answers `false`.
pub type Source = Box<dyn FnMut(&mut [u8]) -> bool>;

pub struct Rng {
    source: Source,
    /// The last step left chains for the next.
    pending: bool,
    buffer: Vec<u8>,
}

impl Rng {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            pending: false,
            buffer: vec![0; CHAIN_BYTES],
        }
    }

    fn fill(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        event_idx: bool,
    ) -> Result<Served, &'static str> {
        let mut served = Served::default();
        let mut filled = 0;
        while filled < STEP_CHAINS {
            let Some(chain) = queue.pop(memory).map_err(ring_error)? else {
                break;
            };
            if !chain.all(true) {
                return Err("an entropy request the device may not write");
            }
            let bytes = (chain.bytes() as usize).min(CHAIN_BYTES);
            let random = &mut self.buffer[..bytes];
            if !(self.source)(random) {
                return Err("the host's random source failed");
            }
            chain.write(memory, 0, random).map_err(ring_error)?;
            random.fill(0);
            queue
                .push_used(memory, chain.head, bytes as u32)
                .map_err(ring_error)?;
            served.chains += 1;
            served.bytes += bytes as u64;
            filled += 1;
        }
        self.pending = filled == STEP_CHAINS && queue.pending(memory).map_err(ring_error)? != 0;
        if event_idx {
            // A kick at the next request, unless the next step comes anyway.
            if self.pending {
                queue.no_kick(memory).map_err(ring_error)?;
            } else {
                queue.want_kick(memory).map_err(ring_error)?;
            }
        }
        served.interrupt = queue.publish(memory, event_idx).map_err(ring_error)?;
        Ok(served)
    }
}

impl VirtioDevice for Rng {
    fn name(&self) -> &'static str {
        "virtio-rng"
    }

    fn id(&self) -> u32 {
        ids::RNG as u32
    }

    fn queue_max(&self) -> &'static [u16] {
        &[QUEUE_MAX]
    }

    fn reset(&mut self) {
        self.pending = false;
    }

    fn due(&self) -> bool {
        self.pending
    }

    fn step(
        &mut self,
        queues: &mut [Option<Queue>],
        notified: u64,
        memory: &mut dyn Memory,
        event_idx: bool,
        _now: u64,
    ) -> Result<Served, &'static str> {
        let Some(queue) = queues.first_mut().and_then(Option::as_mut) else {
            return Ok(Served::default());
        };
        if notified & 1 == 0 && !self.pending {
            // Checked, not told of a request: nothing to do yet, but a
            // driver that published before DRIVER_OK is served now.
            if queue.pending(memory).map_err(ring_error)? == 0 {
                return Ok(Served::default());
            }
        }
        self.fill(queue, memory, event_idx)
    }
}
