//! The modeled port's upstream side (`--apcie`): an endpoint's DMA, routed
//! by requester ID through the port's RID2SID table to a DART stream, and its
//! MSIs, posted writes to the port's doorbell that become AIC inputs.
//!
//! RID2SID (`port+0x828 + 4 * i`, Linux `PORT_RID2SID`): the driver writes
//! entry `i` from its `_ridMapping[i]` (`AppleT811xPCIePort::
//! _updateRIDToSIDMappings`, 0xfffffe0009fa43d0): the requester ID in
//! [15:0], the SID in [19:16] and VALID in bit 31, and REQUIREs
//! `(sid != 0) || !valid` (SID 0 is the shared SID of bridges).
//! `_mapRIDToSID` (0xfffffe00094578dc) fills an entry when a device nub is
//! published: its SID is the static `reg` of the nub's mapper, or one
//! `function-dart_request_sid` assigns (`AppleT8110DART::
//! _dartAssignDynamicSID`, 0xfffffe0009f3e3f8: the first free SID below
//! `sid-count`, only when the DART's `dart-options` allows dynamic SIDs; no
//! DART register is touched), for which `NewIODARTMapper` then builds the
//! stream's tables, which SPTM programs (TCR, TTBR, TLB) like any other
//! stream. So a requester's DMA here is: the entry whose RID matches gives the
//! SID, and the port's DART translates the I/O address for that stream
//! (`Iommu::translate`, which also latches and signals a DART fault). A
//! requester without a valid entry is refused, nothing is transferred, and
//! no port status is raised (which `INTSTAT` bit reports it is not
//! recovered, and `handleInterrupt` panics on an unmasked bit it does not
//! handle).
//!
//! MSI (`AppleT811xPCIePort::configMSIRange`, 0xfffffe0009fa0e08):
//! `MSICFG` (`port+0x124`) = `log2(#msi-vectors) << 4 | 1`, so 0x31 for the
//! eight vectors J473AP gives each port; `MSIBASE` (`port+0x128`) = the
//! port's `msi-vector-base`, as 16 bits, with the same base mod 32 in
//! [20:16] (port 1 0x00080008, port 2 0x00100010); `MSIADDR` (`port+0x168`)
//! = the complex's `msi-address & ~0xf` (0xfffff000). The vector numbers
//! are the complex's: `ApplePCIEMSIController::allocateInterruptVectors`
//! (0xfffffe0009437924) takes them from the port's range (an
//! `IORangeAllocator` over `msi-vector-base` .. + `#msi-vectors`) and
//! REQUIREs `firstVector < _numVectors` (the complex's `#msi-vectors`), and
//! `IOPCIMessagedInterruptController::allocateDeviceInterrupts`
//! (0xfffffe000af17264) programs the endpoint's MSI data with that vector
//! itself (its own vector base, +0xa0, is never set by
//! `ApplePCIEMSIController::init` and stays 0), from
//! `AppleEmbeddedPCIE::callPlatformFunction(GetMessagedInterruptAddress)`'s
//! address (`msi-address`) and data (the vector). The AIC input of vector V
//! is `msi-vector-offset + V`: `addDeviceInterruptProperties`
//! (0xfffffe00094376bc) gives the nub the specifier `V + _firstVector`
//! (`init`'s `msi-vector-offset`) and `registerInterrupt` passes it to the
//! AIC. So the data already includes the port's base: a port-2 MSI carries
//! 16..23, not 0..7. The model accepts a write of data D to the doorbell
//! when MSI is enabled and `MSIBASE` <= D < `MSIBASE` + `1 << L2MSINUM`, and
//! pulses AIC input `msi-vector-offset + D` (an edge input of the AIC). Any
//! other data is dropped and logged (`isMSIDataMiscompareInterrupt` names
//! the hardware's report; its bit is not recovered).
use super::link::Endpoint;
use alloc::vec::Vec;
use stela_contracts::bus::Access;
use stela_contracts::dma::{DmaError, Iommu, Memory, Port};
use stela_contracts::interrupt::{AicInput, Wire};

/// `PORT_RID2SID`'s first entry and how many the model reads (Linux
/// `MAX_RID2SID`).
pub const RID2SID: u64 = 0x828;
pub const RID2SID_ENTRIES: usize = 64;
const RID2SID_VALID: u32 = 1 << 31;
/// `PORT_MSICFG`, `PORT_MSIBASE`, `PORT_MSIADDR`.
pub const MSICFG: u64 = 0x124;
pub const MSIBASE: u64 = 0x128;
pub const MSIADDR: u64 = 0x168;

/// The SID the RID2SID `entries` give requester `rid`, if one is valid.
pub fn sid_for(entries: &[u32], rid: u16) -> Option<usize> {
    entries
        .iter()
        .find(|&&entry| entry & RID2SID_VALID != 0 && entry as u16 == rid)
        .map(|&entry| (entry >> 16 & 0xf) as usize)
}

/// The port's MSI configuration as the driver programs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiWindow {
    /// The first vector, in the complex's numbering.
    pub base: u32,
    pub count: u32,
    /// The doorbell address, 16-byte aligned.
    pub doorbell: u64,
}

impl MsiWindow {
    /// The window `MSICFG`, `MSIBASE` and `MSIADDR` describe, when MSI is
    /// enabled and a doorbell is set.
    pub fn of(msicfg: u32, msibase: u32, msiaddr: u32) -> Option<Self> {
        (msicfg & 1 != 0 && msiaddr & !0xf != 0).then(|| Self {
            base: msibase & 0xffff,
            count: 1 << (msicfg >> 4 & 7),
            doorbell: u64::from(msiaddr & !0xf),
        })
    }

    /// Whether a posted write to `address` is a write to this doorbell.
    pub fn is_doorbell(&self, address: u64) -> bool {
        address & !0xf == self.doorbell
    }

    /// The complex's vector an MSI with `data` raises, if it is one of the
    /// port's.
    pub fn vector(&self, data: u32) -> Option<u32> {
        let data = data & 0xffff;
        (data >= self.base && data - self.base < self.count).then_some(data)
    }
}

/// Why an upstream transaction did nothing (or only part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The endpoint or the root port has bus mastering off.
    BusMaster,
    /// No valid RID2SID entry names the requester.
    NoSid(u16),
    /// The port has no DART to translate with.
    NoIommu,
    /// The DART faulted or the target is not RAM; `completed` bytes moved.
    Dma { error: DmaError, completed: usize },
    /// A doorbell write whose data is not one of the port's vectors, or an
    /// MSI with no AIC vectors known.
    Msi(u32),
}

/// What an upstream write did, when it was an MSI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signalled {
    pub vector: u32,
    pub input: Option<AicInput>,
}

/// The modeled port's upstream path for one step of its endpoint: the RID2SID
/// table and MSI window as the port's registers hold them, the port's DART
/// (or none), guest memory, and the step's access, for faults and MSIs.
pub struct Upstream<'a, 'b> {
    pub rid2sid: Vec<u32>,
    pub msi: Option<MsiWindow>,
    /// The AIC input of vector 0 (`msi-vector-offset`).
    pub msi_offset: Option<u32>,
    /// Whether the root port forwards the endpoint's requests (its command
    /// register's bus master enable).
    pub forwarding: bool,
    pub iommu: Option<&'a mut dyn Iommu>,
    pub memory: &'a mut dyn Memory,
    pub access: &'a mut Access<'b>,
    /// The MSIs this step signalled, in order.
    pub signalled: Vec<Signalled>,
}

impl<'b> Upstream<'_, 'b> {
    fn port(&mut self, rid: u16) -> Result<Port<'_, 'b>, Refused> {
        if !self.forwarding {
            return Err(Refused::BusMaster);
        }
        let stream = sid_for(&self.rid2sid, rid).ok_or(Refused::NoSid(rid))?;
        let iommu = self.iommu.as_deref_mut().ok_or(Refused::NoIommu)?;
        Ok(Port {
            iommu,
            stream,
            memory: &mut *self.memory,
            access: &mut *self.access,
        })
    }

    /// The guest physical address requester `rid`'s DMA reaches at `iova`,
    /// for the bytes up to `last` in the same DART page: the API an endpoint
    /// model translates by. A DART fault is the DART's (latched and
    /// signalled by it).
    pub fn translate(
        &mut self,
        rid: u16,
        iova: u64,
        last: u64,
        write: bool,
    ) -> Result<u64, Refused> {
        let port = self.port(rid)?;
        port.iommu
            .translate(port.memory, port.stream, iova, last, write, port.access)
            .map_err(|error| Refused::Dma {
                error,
                completed: 0,
            })
    }

    /// Requester `rid` reads `buffer.len()` bytes at `iova`.
    pub fn read(&mut self, rid: u16, iova: u64, buffer: &mut [u8]) -> Result<(), Refused> {
        self.port(rid)?
            .read_progress(iova, buffer)
            .map_err(|fault| Refused::Dma {
                error: fault.error,
                completed: fault.completed,
            })
    }

    /// Requester `rid` writes `data` at `iova`; a write to the port's
    /// doorbell is an MSI, which goes to the AIC and not to memory.
    pub fn write(&mut self, rid: u16, iova: u64, data: &[u8]) -> Result<(), Refused> {
        if let Some(window) = self.msi
            && window.is_doorbell(iova)
        {
            if !self.forwarding {
                return Err(Refused::BusMaster);
            }
            let mut word = [0u8; 4];
            let bytes = data.len().min(4);
            word[..bytes].copy_from_slice(&data[..bytes]);
            let value = u32::from_le_bytes(word);
            let vector = window.vector(value).ok_or(Refused::Msi(value))?;
            let input = self
                .msi_offset
                .and_then(|offset| u16::try_from(offset + vector).ok())
                .map(|irq| AicInput::new(0, irq));
            self.signalled.push(Signalled { vector, input });
            let Some(input) = input else {
                return Err(Refused::Msi(value));
            };
            // A posted write: a rise the AIC latches, then the fall.
            self.access.interrupts.set_level(Wire::Aic(input), true);
            self.access.interrupts.set_level(Wire::Aic(input), false);
            return Ok(());
        }
        self.port(rid)?
            .write_progress(iova, data)
            .map_err(|fault| Refused::Dma {
                error: fault.error,
                completed: fault.completed,
            })
    }
}

/// The endpoint's DMA self-test (BAR0 registers from [`SELFTEST`]), the
/// smallest exercise of the whole upstream path a guest can start: write an
/// I/O address and a length, then a command; the endpoint DMAs through its
/// RID's DART stream and, when its MSI is enabled, raises it.
///
/// | BAR0 offset | register |
/// |---|---|
/// | 0xf00, 0xf04 | I/O address, low and high words |
/// | 0xf08 | length in bytes (0 means 64; at most [`SELFTEST_MAX`]) |
/// | 0xf0c | command: 1 writes the pattern at the address, 2 reads the address into BAR0 from 0 (at most 0xf00 bytes) |
/// | 0xf10 | status (read-only): 1 busy, 2 done, 4 refused, 8 MSI sent; [31:16] runs completed |
/// | 0xf14 | pattern seed: byte i of the pattern is `seed ^ i` (initially 0xa5) |
/// | 0xf18 | what refused the last run (0 none, 1 bus master off, 2 no SID, 3 no DART, 4 DART fault or not RAM, 5 MSI) |
/// | 0xf1c | bytes the last run moved |
pub const SELFTEST: usize = 0xf00;
pub const SELFTEST_MAX: u32 = 0x4000;
pub const SELFTEST_RAM: usize = SELFTEST;
pub const COMMAND_WRITE: u32 = 1;
pub const COMMAND_READ: u32 = 2;
pub const STATUS_BUSY: u32 = 1;
pub const STATUS_DONE: u32 = 2;
pub const STATUS_REFUSED: u32 = 4;
pub const STATUS_MSI: u32 = 8;

/// The pattern's byte `index` for `seed`.
pub fn pattern(seed: u32, index: usize) -> u8 {
    (seed as u8) ^ (index as u8)
}

impl Refused {
    /// The self-test's code for this refusal (`0xf18`).
    pub fn code(&self) -> u32 {
        match self {
            Self::BusMaster => 1,
            Self::NoSid(_) => 2,
            Self::NoIommu => 3,
            Self::Dma { .. } => 4,
            Self::Msi(_) => 5,
        }
    }
}

/// Run the endpoint's pending self-test as requester `rid`.
pub fn run_selftest(
    endpoint: &mut Endpoint,
    rid: u16,
    upstream: &mut Upstream<'_, '_>,
) -> Result<(), Refused> {
    let Some(test) = endpoint.take_selftest() else {
        return Ok(());
    };
    let result = match test.command {
        COMMAND_WRITE => {
            let data: Vec<u8> = (0..test.length as usize)
                .map(|index| pattern(test.seed, index))
                .collect();
            upstream.write(rid, test.iova, &data).map(|()| test.length)
        }
        _ => {
            let mut data = alloc::vec![0u8; (test.length as usize).min(SELFTEST_RAM)];
            let moved = upstream
                .read(rid, test.iova, &mut data)
                .map(|()| data.len() as u32);
            if moved.is_ok() {
                for (at, byte) in data.iter().enumerate() {
                    endpoint.write_bar(at, 1, u64::from(*byte));
                }
            }
            moved
        }
    };
    let msi = match &result {
        Ok(_) => endpoint
            .msi_message()
            .map(|(address, data)| upstream.write(rid, address, &data.to_le_bytes())),
        Err(_) => None,
    };
    let moved = match &result {
        Ok(bytes) => *bytes,
        Err(Refused::Dma { completed, .. }) => *completed as u32,
        Err(_) => 0,
    };
    let refusal = result
        .as_ref()
        .err()
        .or(msi.as_ref().and_then(|msi| msi.as_ref().err()));
    endpoint.finish_selftest(
        moved,
        refusal.map_or(0, Refused::code),
        matches!(msi, Some(Ok(()))),
    );
    match refusal {
        Some(refused) => Err(refused.clone()),
        None => Ok(()),
    }
}
