//! The block device (virtio 1.2, 5.2): one request queue in front of a host
//! [`BlockDevice`], through the block layer's [`Submission`]s as ANS's
//! namespaces use it (`ans::host`).
//!
//! Each request is one chain: a 16-byte header the device reads (type,
//! reserved, 512-byte sector), the data (readable for `OUT`, writable for
//! `IN`), and a status byte the device writes last. `IN`, `OUT`, `FLUSH` and
//! `GET_ID` are served; every other type is answered `UNSUPP`. A request the
//! disk refuses, one out of range or not aligned to the block size, a write
//! to a read-only device, or one past [`REQUEST_MAX`] is answered `IOERR`. A
//! chain with no room for the header or the status byte breaks the format,
//! and stops the device as a malformed ring does.
//!
//! The device offers `BLK_SIZE`, `SEG_MAX`, `SIZE_MAX` and `FLUSH`, and `RO`
//! for a read-only disk; never `DISCARD` or `WRITE_ZEROES`. A request starts
//! where the device takes it from the ring and completes once the disk has
//! completed it, in whatever order: a disk that runs nothing queued (the RAM
//! disk, [`ram`]) completes it in the same step. A disk that runs requests
//! queued keeps the device due while they run, and each step looks for
//! their completions.
//!
//! What backs the device is the boot's choice, and safety comes first:
//!
//! - The boot's one backing is [`ram`], a zero-filled RAM disk of the
//!   host's memory: it opens no host disk and keeps nothing.
//! - [`Blk::new`] refuses any writable disk that is not the engine's own
//!   memory, whoever asks: a host disk could only ever back the device
//!   read-only.
//! - A read-only host disk is not wired into the boot yet. Its rule is
//!   [`refusal`]: never a disk Stela holds for itself (the guest's system
//!   disk behind ANS, the disk the PC booted from, the store's) or gives the
//!   guest another way (`--usb-storage`, `--external-disks`).
use super::queue::{Chain, Queue};
use super::{Served, VirtioDevice, ring_error};
use alloc::{boxed::Box, format, string::String, vec, vec::Vec};
use stela_contracts::{
    dma::Memory,
    virtio::{
        blk::{config, features, request},
        ids,
    },
};
use stela_drivers::block::{BlockDevice, Finished, Request, Submission, Transport, Volatile};

/// The most entries the request queue takes.
pub const QUEUE_MAX: u16 = 256;
/// The most data buffers a request has, as `seg_max` says: a queue's worth
/// less the header and the status.
pub const SEG_MAX: u32 = QUEUE_MAX as u32 - 2;
/// The most bytes one buffer has, as `size_max` says.
pub const SIZE_MAX: u32 = 1 << 20;
/// The most data one request moves: a step's work under the devices' lock
/// stays bounded. Larger requests are answered `IOERR`.
pub const REQUEST_MAX: u64 = 4 << 20;
/// Requests one step takes from the ring at most.
const STEP_REQUESTS: usize = 32;
/// Requests the disk runs at once at most.
const RUNNING_MAX: usize = 64;
/// The serial a RAM disk answers `GET_ID` with: 20 bytes, all of `ID_BYTES`.
pub const RAM_SERIAL: &str = "stela-virtio-blk-ram";
/// The RAM disk's size by default, and at most, in MiB: what the guest
/// writes takes the engine's heap, a few GiB shared with everything else.
pub const RAM_DEFAULT_MIB: u64 = 64;
pub const RAM_MOST_MIB: u64 = 256;

/// What the device did, for the boot log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Requests completed, whatever their status.
    pub served: u64,
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub ids: u64,
    /// Requests of a type the device does not take.
    pub unsupported: u64,
    /// Requests answered `IOERR`.
    pub errors: u64,
    /// Data read from the disk into the guest, and written from it.
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// What a request asks of the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Flush,
}

/// Where a request's parts lie in its chain: the bytes the device reads
/// (the header, then a write's data) and writes (a read's data, then the
/// status byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    readable: u64,
    writable: u64,
}

impl Shape {
    fn of(chain: &Chain) -> Self {
        let sum = |write: bool| {
            chain
                .buffers
                .iter()
                .filter(|buffer| buffer.write == write)
                .map(|buffer| u64::from(buffer.bytes))
                .sum()
        };
        Self {
            readable: sum(false),
            writable: sum(true),
        }
    }

    /// The status byte: the chain's last.
    fn status(&self) -> u64 {
        self.readable + self.writable - 1
    }
}

/// A request on the disk.
struct Running {
    chain: Chain,
    shape: Shape,
    kind: Kind,
    /// A write's data bytes, for the counts.
    bytes: u64,
    submission: Submission,
}

/// What became of a request when the device took it.
enum Outcome {
    /// Answered at once: its status and the bytes written into the chain.
    Done { status: u8, written: u32 },
    /// For the disk: what it asks, a write's bytes, its submission.
    Start {
        kind: Kind,
        bytes: u64,
        submission: Box<Submission>,
    },
}

pub struct Blk {
    disk: Box<dyn BlockDevice>,
    /// The disk's block size in bytes and its capacity in 512-byte sectors.
    block: u32,
    sectors: u64,
    read_only: bool,
    /// What `GET_ID` answers: the disk's serial, NUL-padded.
    id: [u8; request::ID_BYTES as usize],
    /// The features the driver accepted.
    accepted: u64,
    running: Vec<Running>,
    /// The last step left requests on the ring for the next.
    pending: bool,
    counts: Counts,
}

/// A zero-filled RAM disk of `mib` MiB from the host's heap, which takes
/// memory only as the guest writes it, up to its size, and keeps nothing
/// past power-off; its serial is [`RAM_SERIAL`].
pub fn ram(mib: u64) -> Result<Blk, String> {
    if mib == 0 || mib > RAM_MOST_MIB {
        return Err(format!(
            "a virtio-blk RAM disk is 1 to {RAM_MOST_MIB} MiB, not {mib}"
        ));
    }
    let bytes = mib << 20;
    let disk = Volatile::new(RAM_SERIAL, bytes, bytes as usize)
        .map_err(|error| format!("a virtio-blk RAM disk of {mib} MiB: {error}"))?;
    Blk::new(Box::new(disk)).map_err(String::from)
}

/// A disk the boot holds for one of its own roles, or gives the guest
/// another way, by its serial: the guest's system disk behind ANS, the
/// disk the PC booted from, a `--usb-storage` disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held<'a> {
    pub serial: &'a str,
    /// Its role, for the refusal: "the disk the PC booted from".
    pub role: &'a str,
}

/// Why the disk `serial` may not back a virtio-blk device read-only, or
/// `None` when it may: it must be named, must not be one the boot holds
/// (`held`), and must report itself read-only to the device.
pub fn refusal(serial: &str, read_only: bool, held: &[Held<'_>]) -> Option<String> {
    if serial.trim().is_empty() {
        return Some("a disk with no serial cannot be named, so none is attached".into());
    }
    if let Some(held) = held.iter().find(|held| held.serial == serial) {
        return Some(format!(
            "{serial} is {}: virtio-blk never attaches a disk Stela holds",
            held.role
        ));
    }
    if !read_only {
        return Some(format!(
            "{serial} is not read-only behind its view: virtio-blk attaches real disks only read-only"
        ));
    }
    None
}

impl Blk {
    /// The device in front of `disk`. A disk that is not the engine's own
    /// memory must be read-only: a writable real disk is refused, whoever
    /// asks. So is a block size that is not a power of two from 512 bytes
    /// to 64 KiB, or no blocks.
    pub fn new(disk: Box<dyn BlockDevice>) -> Result<Self, &'static str> {
        let identity = disk.identity();
        if identity.transport != Transport::Ram && !identity.read_only {
            return Err("virtio-blk attaches a host disk only read-only; this one is writable");
        }
        let block = identity.block_size;
        if !block.is_power_of_two() || !(512..=0x1_0000).contains(&block) {
            return Err(
                "a virtio-blk disk's block size is a power of two from 512 bytes to 64 KiB",
            );
        }
        let sectors = identity
            .blocks
            .checked_mul(u64::from(block) / request::SECTOR_BYTES)
            .filter(|&sectors| sectors != 0 && sectors.checked_mul(request::SECTOR_BYTES).is_some())
            .ok_or("a virtio-blk disk has no blocks, or more bytes than 64 bits count")?;
        let mut id = [0; request::ID_BYTES as usize];
        let serial = identity.serial.as_bytes();
        let length = serial.len().min(id.len());
        id[..length].copy_from_slice(&serial[..length]);
        Ok(Self {
            block,
            sectors,
            read_only: identity.read_only,
            id,
            disk,
            accepted: 0,
            running: Vec::new(),
            pending: false,
            counts: Counts::default(),
        })
    }

    /// The device's capacity in bytes.
    pub fn bytes(&self) -> u64 {
        self.sectors * request::SECTOR_BYTES
    }

    pub fn block_size(&self) -> u32 {
        self.block
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The configuration space, `config::BYTES` long: capacity, size_max,
    /// seg_max and blk_size; every field not offered reads 0.
    fn config(&self) -> [u8; config::BYTES as usize] {
        let mut space = [0; config::BYTES as usize];
        let mut put = |offset: u64, bytes: &[u8]| {
            space[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        };
        put(config::CAPACITY, &self.sectors.to_le_bytes());
        put(config::SIZE_MAX, &SIZE_MAX.to_le_bytes());
        put(config::SEG_MAX, &SEG_MAX.to_le_bytes());
        put(config::BLK_SIZE, &self.block.to_le_bytes());
        space
    }

    /// The first block of a transfer of `bytes` at `sector`, or `None` for
    /// one the disk cannot take: not whole blocks, past the end or past
    /// [`REQUEST_MAX`].
    fn blocks(&self, sector: u64, bytes: u64) -> Option<u64> {
        let block = u64::from(self.block);
        let start = sector.checked_mul(request::SECTOR_BYTES)?;
        let end = start.checked_add(bytes)?;
        (start.is_multiple_of(block)
            && bytes.is_multiple_of(block)
            && bytes <= REQUEST_MAX
            && end <= self.bytes())
        .then_some(start / block)
    }

    /// Takes the request `chain` makes: answered at once, or for the disk.
    fn begin(&mut self, chain: &Chain, memory: &mut dyn Memory) -> Result<Outcome, &'static str> {
        let shape = Shape::of(chain);
        if shape.readable < request::HEADER_BYTES {
            return Err("a block request with no room for its header");
        }
        if shape.writable == 0 {
            return Err("a block request with no status byte");
        }
        let mut header = [0u8; request::HEADER_BYTES as usize];
        chain.read(memory, 0, &mut header).map_err(ring_error)?;
        let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let ioerr = Outcome::Done {
            status: request::S_IOERR as u8,
            written: 1,
        };
        let (kind, request, bytes) = match u64::from(kind) {
            request::T_IN => {
                let bytes = shape.writable - 1;
                let Some(lba) = self
                    .blocks(sector, bytes)
                    .filter(|_| shape.readable == request::HEADER_BYTES)
                else {
                    return Ok(ioerr);
                };
                let request = Request::Read {
                    lba,
                    bytes: bytes as usize,
                };
                (Kind::Read, request, 0)
            }
            request::T_OUT => {
                let bytes = shape.readable - request::HEADER_BYTES;
                let Some(lba) = self
                    .blocks(sector, bytes)
                    .filter(|_| shape.writable == 1 && !self.read_only)
                else {
                    return Ok(ioerr);
                };
                let mut data = vec![0; bytes as usize];
                chain
                    .read(memory, request::HEADER_BYTES, &mut data)
                    .map_err(ring_error)?;
                let request = Request::Write {
                    lba,
                    data,
                    durable: false,
                };
                (Kind::Write, request, bytes)
            }
            request::T_FLUSH => {
                if self.accepted & features::FLUSH == 0 {
                    return Ok(self.unsupported());
                }
                if shape.readable != request::HEADER_BYTES || shape.writable != 1 {
                    return Ok(ioerr);
                }
                (Kind::Flush, Request::Flush, 0)
            }
            request::T_GET_ID => {
                let bytes = (shape.writable - 1).min(request::ID_BYTES) as usize;
                chain
                    .write(memory, shape.readable, &self.id[..bytes])
                    .map_err(ring_error)?;
                self.counts.ids += 1;
                return Ok(Outcome::Done {
                    status: request::S_OK as u8,
                    written: shape.writable as u32,
                });
            }
            _ => return Ok(self.unsupported()),
        };
        Ok(match Submission::new(request, self.disk.identity()) {
            Ok(submission) => Outcome::Start {
                kind,
                bytes,
                submission: Box::new(submission),
            },
            Err(_) => ioerr,
        })
    }

    fn unsupported(&mut self) -> Outcome {
        self.counts.unsupported += 1;
        Outcome::Done {
            status: request::S_UNSUPP as u8,
            written: 1,
        }
    }

    /// The disk completed `running`: a read's data into its chain, then
    /// its status.
    fn settle(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        running: Running,
        finished: Finished,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        let Running {
            chain,
            shape,
            kind,
            bytes,
            ..
        } = running;
        let (status, written) = match (kind, finished) {
            (Kind::Read, Ok(Some(data))) if data.len() as u64 == shape.writable - 1 => {
                chain
                    .write(memory, shape.readable, &data)
                    .map_err(ring_error)?;
                self.counts.reads += 1;
                self.counts.bytes_in += data.len() as u64;
                (request::S_OK, shape.writable as u32)
            }
            (Kind::Write, Ok(_)) => {
                self.counts.writes += 1;
                self.counts.bytes_out += bytes;
                (request::S_OK, 1)
            }
            (Kind::Flush, Ok(_)) => {
                self.counts.flushes += 1;
                (request::S_OK, 1)
            }
            // A failed transfer, or a read the disk answered short.
            _ => (request::S_IOERR, 1),
        };
        self.answer(queue, memory, &chain, shape, status as u8, written, served)
    }

    /// Writes `status` as `chain`'s last byte and puts it on the used ring
    /// with `written` bytes.
    #[allow(clippy::too_many_arguments)]
    fn answer(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        chain: &Chain,
        shape: Shape,
        status: u8,
        written: u32,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        chain
            .write(memory, shape.status(), &[status])
            .map_err(ring_error)?;
        queue
            .push_used(memory, chain.head, written)
            .map_err(ring_error)?;
        self.counts.served += 1;
        if u64::from(status) == request::S_IOERR {
            self.counts.errors += 1;
        }
        served.chains += 1;
        served.bytes += u64::from(written);
        Ok(())
    }

    /// Looks for what the disk completed, and answers it.
    fn harvest(
        &mut self,
        queue: &mut Queue,
        memory: &mut dyn Memory,
        served: &mut Served,
    ) -> Result<(), &'static str> {
        let mut index = 0;
        while index < self.running.len() {
            let Some(finished) = self.running[index].submission.poll(&mut *self.disk) else {
                index += 1;
                continue;
            };
            let running = self.running.swap_remove(index);
            self.settle(queue, memory, running, finished, served)?;
        }
        Ok(())
    }

    fn serve(
        &mut self,
        queue: &mut Queue,
        notified: u64,
        memory: &mut dyn Memory,
        event_idx: bool,
    ) -> Result<Served, &'static str> {
        let mut served = Served::default();
        self.harvest(queue, memory, &mut served)?;
        let mut taken = 0;
        let look =
            notified & 1 != 0 || self.pending || queue.pending(memory).map_err(ring_error)? != 0;
        while look && taken < STEP_REQUESTS && self.running.len() < RUNNING_MAX {
            let Some(chain) = queue.pop(memory).map_err(ring_error)? else {
                break;
            };
            taken += 1;
            let shape = Shape::of(&chain);
            match self.begin(&chain, memory)? {
                Outcome::Done { status, written } => {
                    self.answer(queue, memory, &chain, shape, status, written, &mut served)?;
                }
                Outcome::Start {
                    kind,
                    bytes,
                    mut submission,
                } => {
                    let finished = submission.poll(&mut *self.disk);
                    let running = Running {
                        chain,
                        shape,
                        kind,
                        bytes,
                        submission: *submission,
                    };
                    match finished {
                        Some(finished) => {
                            self.settle(queue, memory, running, finished, &mut served)?
                        }
                        None => self.running.push(running),
                    }
                }
            }
        }
        self.pending = queue.pending(memory).map_err(ring_error)? != 0
            && (taken == STEP_REQUESTS || self.running.len() >= RUNNING_MAX);
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

impl VirtioDevice for Blk {
    fn name(&self) -> &'static str {
        "virtio-blk"
    }

    fn id(&self) -> u32 {
        ids::BLOCK as u32
    }

    fn features(&self) -> u64 {
        let read_only = if self.read_only { features::RO } else { 0 };
        features::SIZE_MAX | features::SEG_MAX | features::BLK_SIZE | features::FLUSH | read_only
    }

    fn queue_max(&self) -> &'static [u16] {
        &[QUEUE_MAX]
    }

    /// Any aligned access of 1, 2, 4 or 8 bytes within the structure.
    fn config_read(&self, offset: u64, bytes: u64) -> Option<u64> {
        if !matches!(bytes, 1 | 2 | 4 | 8) || !offset.is_multiple_of(bytes) {
            return None;
        }
        let end = offset.checked_add(bytes)?;
        let space = self.config();
        let field = space.get(offset as usize..end as usize)?;
        let mut value = [0u8; 8];
        value[..field.len()].copy_from_slice(field);
        Some(u64::from_le_bytes(value))
    }

    fn activate(&mut self, features: u64) {
        self.accepted = features;
    }

    /// The driver reset the device: requests the disk still runs are
    /// forgotten, their results never reaching the guest.
    fn reset(&mut self) {
        self.running.clear();
        self.pending = false;
        self.accepted = 0;
    }

    fn due(&self) -> bool {
        self.pending || !self.running.is_empty()
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        let counts = self.counts;
        vec![
            ("requests", counts.served),
            ("reads", counts.reads),
            ("writes", counts.writes),
            ("flushes", counts.flushes),
            ("ids", counts.ids),
            ("unsupported", counts.unsupported),
            ("bytes_in", counts.bytes_in),
            ("bytes_out", counts.bytes_out),
            ("errors", counts.errors),
            ("running", self.running.len() as u64),
        ]
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
        self.serve(queue, notified, memory, event_idx)
    }
}

#[cfg(test)]
mod tests;
