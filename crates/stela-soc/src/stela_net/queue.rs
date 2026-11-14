//! The device side of one split virtqueue (virtio 1.2, 2.7) in guest RAM, as
//! `stela_contracts::net::ring` lays it out: chains taken from the available
//! ring, completions put on the used ring, and `VIRTIO_F_EVENT_IDX`
//! notification suppression both ways.
//!
//! Every access goes through [`Memory`] by guest physical address. A ring
//! whose indices or descriptors break the format is the driver's bug: the
//! queue answers [`RingError::Malformed`] and the device stops, rather than
//! guess.
use alloc::vec::Vec;
use stela_contracts::dma::Memory;
use stela_contracts::net::{
    ring::{self, DESC_F_INDIRECT, DESC_F_NEXT, DESC_F_WRITE},
    window::QUEUE_MAX,
};

/// Why a ring could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// The ring or a buffer is not RAM.
    Memory,
    /// The ring breaks the format.
    Malformed(&'static str),
}

/// One buffer of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buffer {
    pub address: u64,
    pub bytes: u32,
    /// The device writes it (`DESC_F_WRITE`).
    pub write: bool,
}

/// A descriptor chain the driver made available.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chain {
    /// The head descriptor's index, which the used ring names.
    pub head: u16,
    pub buffers: Vec<Buffer>,
}

impl Chain {
    /// The bytes of every buffer.
    pub fn bytes(&self) -> u64 {
        self.buffers.iter().map(|b| u64::from(b.bytes)).sum()
    }

    /// Whether every buffer is the device's to write, or every one the
    /// device's to read.
    pub fn all(&self, write: bool) -> bool {
        self.buffers.iter().all(|b| b.write == write)
    }

    /// Reads `out.len()` bytes of the chain from byte `skip` on.
    pub fn read(
        &self,
        memory: &mut dyn Memory,
        skip: u64,
        out: &mut [u8],
    ) -> Result<(), RingError> {
        self.each(skip, out.len(), |address, range| {
            memory
                .read(address, &mut out[range])
                .then_some(())
                .ok_or(RingError::Memory)
        })
    }

    /// Writes `data` into the chain from byte `skip` on.
    pub fn write(&self, memory: &mut dyn Memory, skip: u64, data: &[u8]) -> Result<(), RingError> {
        self.each(skip, data.len(), |address, range| {
            memory
                .write(address, &data[range])
                .then_some(())
                .ok_or(RingError::Memory)
        })
    }

    /// Calls `transfer` with each buffer address and the byte range of the
    /// caller's data that lands there, for `bytes` bytes from `skip` on.
    fn each(
        &self,
        mut skip: u64,
        bytes: usize,
        mut transfer: impl FnMut(u64, core::ops::Range<usize>) -> Result<(), RingError>,
    ) -> Result<(), RingError> {
        let mut done = 0;
        for buffer in &self.buffers {
            if done == bytes {
                break;
            }
            let length = u64::from(buffer.bytes);
            if skip >= length {
                skip -= length;
                continue;
            }
            let take = ((length - skip) as usize).min(bytes - done);
            transfer(buffer.address + skip, done..done + take)?;
            done += take;
            skip = 0;
        }
        if done == bytes {
            Ok(())
        } else {
            Err(RingError::Malformed("a transfer past the chain's buffers"))
        }
    }
}

/// Whether a driver that last asked to hear at `event` wants to hear of the
/// index moving from `old` to `new` (virtio 2.7.10, `vring_need_event`).
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// One virtqueue: where it is and how far the device has gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    base: u64,
    size: u16,
    /// The next available entry the device takes.
    last_avail: u16,
    /// The used index after the entries pushed.
    used: u16,
    /// The used index the device last published.
    shown: u16,
}

impl Queue {
    /// A queue of `size` entries in the block at `base`. Sizes are powers of
    /// two up to `QUEUE_MAX`; the block is 16-byte aligned, as the
    /// descriptor table must be.
    pub fn new(base: u64, size: u64) -> Result<Self, &'static str> {
        if size == 0 || !size.is_power_of_two() || size > QUEUE_MAX {
            return Err("a queue's size is not a power of two up to QUEUE_MAX");
        }
        if base == 0 || !base.is_multiple_of(16) {
            return Err("a queue's block is not 16-byte aligned");
        }
        base.checked_add(ring::bytes(size))
            .ok_or("a queue's block wraps the address space")?;
        Ok(Self {
            base,
            size: size as u16,
            last_avail: 0,
            used: 0,
            shown: 0,
        })
    }

    pub fn size(&self) -> u16 {
        self.size
    }

    /// Checks the whole block is RAM the device may write.
    pub fn check(&self, memory: &mut dyn Memory) -> Result<(), RingError> {
        memory
            .check_range(self.base, ring::bytes(u64::from(self.size)) as usize, true)
            .map_err(|_| RingError::Memory)
    }

    fn avail(&self) -> u64 {
        self.base + ring::avail(u64::from(self.size))
    }

    fn used_ring(&self) -> u64 {
        self.base + ring::used(u64::from(self.size))
    }

    fn read16(memory: &mut dyn Memory, address: u64) -> Result<u16, RingError> {
        let mut bytes = [0; 2];
        memory
            .read(address, &mut bytes)
            .then_some(u16::from_le_bytes(bytes))
            .ok_or(RingError::Memory)
    }

    fn write16(memory: &mut dyn Memory, address: u64, value: u16) -> Result<(), RingError> {
        memory
            .write(address, &value.to_le_bytes())
            .then_some(())
            .ok_or(RingError::Memory)
    }

    /// The available ring's `flags`.
    pub fn avail_flags(&self, memory: &mut dyn Memory) -> Result<u16, RingError> {
        Self::read16(memory, self.avail())
    }

    /// The available ring's `used_event`: the used index the driver wants
    /// an interrupt past.
    pub fn used_event(&self, memory: &mut dyn Memory) -> Result<u16, RingError> {
        Self::read16(memory, self.avail() + 4 + 2 * u64::from(self.size))
    }

    /// Sets the used ring's `avail_event`: the driver kicks once its
    /// available index passes `event`.
    pub fn set_avail_event(&self, memory: &mut dyn Memory, event: u16) -> Result<(), RingError> {
        Self::write16(
            memory,
            self.used_ring() + 4 + 8 * u64::from(self.size),
            event,
        )
    }

    /// Asks for a kick at the next chain the driver publishes.
    pub fn want_kick(&self, memory: &mut dyn Memory) -> Result<(), RingError> {
        self.set_avail_event(memory, self.last_avail)
    }

    /// Asks for no kick until the device next asks.
    pub fn no_kick(&self, memory: &mut dyn Memory) -> Result<(), RingError> {
        self.set_avail_event(memory, self.last_avail.wrapping_add(0x8000))
    }

    /// Chains the driver made available that the device has not taken.
    pub fn pending(&self, memory: &mut dyn Memory) -> Result<u16, RingError> {
        let index = Self::read16(memory, self.avail() + 2)?;
        let pending = index.wrapping_sub(self.last_avail);
        if pending > self.size {
            return Err(RingError::Malformed(
                "the available index is more than a ring ahead",
            ));
        }
        Ok(pending)
    }

    /// Takes the next available chain, `None` when there is none.
    pub fn pop(&mut self, memory: &mut dyn Memory) -> Result<Option<Chain>, RingError> {
        if self.pending(memory)? == 0 {
            return Ok(None);
        }
        let slot = u64::from(self.last_avail % self.size);
        let head = Self::read16(memory, self.avail() + 4 + 2 * slot)?;
        let chain = self.chain(memory, head)?;
        self.last_avail = self.last_avail.wrapping_add(1);
        Ok(Some(chain))
    }

    /// Gives back the chain [`Self::pop`] took last, untouched.
    pub fn unpop(&mut self) {
        self.last_avail = self.last_avail.wrapping_sub(1);
    }

    /// Walks the chain at descriptor `head`: at most a ring of descriptors,
    /// every index in the table, readable buffers before writable ones, no
    /// indirect tables.
    fn chain(&self, memory: &mut dyn Memory, head: u16) -> Result<Chain, RingError> {
        let mut chain = Chain {
            head,
            buffers: Vec::new(),
        };
        let mut index = head;
        loop {
            if index >= self.size {
                return Err(RingError::Malformed("a descriptor index past the table"));
            }
            if chain.buffers.len() == usize::from(self.size) {
                return Err(RingError::Malformed("a chain loops"));
            }
            let mut descriptor = [0u8; ring::DESCRIPTOR_BYTES as usize];
            memory
                .read(
                    self.base + ring::DESCRIPTOR_BYTES * u64::from(index),
                    &mut descriptor,
                )
                .then_some(())
                .ok_or(RingError::Memory)?;
            let address = u64::from_le_bytes(descriptor[0..8].try_into().unwrap());
            let bytes = u32::from_le_bytes(descriptor[8..12].try_into().unwrap());
            let flags = u16::from_le_bytes(descriptor[12..14].try_into().unwrap());
            let next = u16::from_le_bytes(descriptor[14..16].try_into().unwrap());
            if flags & DESC_F_INDIRECT != 0 {
                return Err(RingError::Malformed("an indirect descriptor, not offered"));
            }
            let write = flags & DESC_F_WRITE != 0;
            if !write && chain.buffers.last().is_some_and(|b| b.write) {
                return Err(RingError::Malformed(
                    "a readable buffer after a writable one",
                ));
            }
            if address.checked_add(u64::from(bytes)).is_none() {
                return Err(RingError::Malformed("a buffer wraps the address space"));
            }
            chain.buffers.push(Buffer {
                address,
                bytes,
                write,
            });
            if flags & DESC_F_NEXT == 0 {
                return Ok(chain);
            }
            index = next;
        }
    }

    /// Puts chain `head` on the used ring with `bytes` written, unpublished
    /// until [`Self::publish`].
    pub fn push_used(
        &mut self,
        memory: &mut dyn Memory,
        head: u16,
        bytes: u32,
    ) -> Result<(), RingError> {
        let slot = u64::from(self.used % self.size);
        let mut element = [0u8; 8];
        element[..4].copy_from_slice(&u32::from(head).to_le_bytes());
        element[4..].copy_from_slice(&bytes.to_le_bytes());
        memory
            .write(self.used_ring() + 4 + 8 * slot, &element)
            .then_some(())
            .ok_or(RingError::Memory)?;
        self.used = self.used.wrapping_add(1);
        Ok(())
    }

    /// Publishes the used entries pushed since the last publish, after
    /// them, and answers whether the driver wants an interrupt for them:
    /// with `event_idx` when they pass its `used_event`, otherwise unless
    /// its available ring's flags say not to.
    pub fn publish(&mut self, memory: &mut dyn Memory, event_idx: bool) -> Result<bool, RingError> {
        let old = self.shown;
        if old == self.used {
            return Ok(false);
        }
        Self::write16(memory, self.used_ring() + 2, self.used)?;
        self.shown = self.used;
        Ok(if event_idx {
            need_event(self.used_event(memory)?, self.used, old)
        } else {
            self.avail_flags(memory)? & ring::AVAIL_F_NO_INTERRUPT == 0
        })
    }
}
