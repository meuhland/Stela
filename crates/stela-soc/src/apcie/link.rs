//! The root-complex model's one port (`--apcie`): link training that
//! completes, and a minimal endpoint behind the port.
//!
//! What the driver checks on the way to link-up (26B123 J473AP kernelcache):
//!
//! - `AppleEmbeddedPCIEPort::waitForLinkUpGated` (0xfffffe0009454dd0) sleeps
//!   until `PORT_INTSTAT` (`port+0x100`) bit 12 (`LINK_UP`) is set and the
//!   port's state is link-up; only `handleInterrupt` (0xfffffe0009451e9c),
//!   the action of the port's interrupt (the complex's `interrupts[N]`),
//!   calls `handleLinkUp` (0xfffffe0009462114), which sets that state. So
//!   the link comes up with `LINK_UP` set *and* the port's AIC input raised;
//!   `handleInterrupt` clears the bit (write one to clear) and panics on any
//!   unmasked status bit it does not handle, so the model sets no other.
//! - `getLinkUp` (0xfffffe0009fa4188) is `port+0x208` (`PORT_LINKSTS`) bit
//!   0, which `_waitForLinkUp`, `configure` and the link-training timer read.
//! - `handleLinkUp` panics unless the root port's `LNKSTA` width (bits 9:4)
//!   is 1 (`link width is %u, expected %u`), and `setLinkSpeed`
//!   (0xfffffe0009462868) panics after 100 ms unless `LNKSTA`'s speed
//!   reaches the port's desired speed (`maximum-link-speed`, written to
//!   `LNKCTL2`): the link trains at `LNKCTL2`'s target, bounded by `LNKCAP`.
//!
//! The link comes up [`TRAINING_TICKS`] after LTSSM start (`port+0x80` bit
//! 0), after `enableGated` has set the port's link-training state, as real
//! training would; it goes down when LTSSM, the port's clock (`0x800` bit
//! 0) or PERST# (`0x814` bit 0) is turned off.
//!
//! The endpoint, at the root port's secondary bus, device 0, function 0, is
//! a type-0 function no macOS driver matches: QEMU's PCI test device ID
//! ([`VENDOR`]:[`DEVICE`]), class 0xff0000 (unassigned), a PCIe v2 endpoint
//! capability (x1, 2.5 GT/s), an MSI capability (one vector, 64-bit
//! address), no INTx, and one 4 KiB 32-bit memory BAR, which answers once
//! IOPCIFamily has assigned it from the port's window and enabled memory
//! decoding on the root port and the endpoint. The BAR is RAM up to 0xf00,
//! then the DMA self-test's registers ([`super::dma::SELFTEST`]).
use super::EXPRESS;
use super::dma::{
    COMMAND_READ, COMMAND_WRITE, SELFTEST, SELFTEST_MAX, STATUS_BUSY, STATUS_DONE, STATUS_MSI,
    STATUS_REFUSED,
};
use alloc::boxed::Box;

/// Guest time from LTSSM start to link-up: 5 ms of 24 MHz ticks, well inside
/// the driver's 100 ms `ltssm-timeout`.
pub const TRAINING_TICKS: u64 = 5 * 24_000;
/// `PORT_INTSTAT` link-up (Linux `PORT_INT_LINK_UP`).
pub const LINK_UP: u32 = 1 << 12;
/// The endpoint's vendor and device: QEMU's PCI test device.
pub const VENDOR: u16 = 0x1b36;
pub const DEVICE: u16 = 0x0005;
/// Class code (unassigned class) and revision, as config `0x08` reads.
pub const CLASS_REVISION: u32 = 0xff00_0001;
/// The endpoint's one BAR.
pub const BAR_BYTES: usize = 0x1000;
/// `LNKSTA`: data-link layer active, slot clock configuration.
const DLL_ACTIVE: u16 = 1 << 13;
const SLOT_CLOCK: u16 = 1 << 12;
/// DesignWare `PL_DEBUG0` (port logic `0x728`) LTSSM state L0.
pub const LTSSM_L0: u32 = 0x11;
/// The endpoint's PCIe capability, then its MSI capability.
const CAPABILITY: usize = 0x40;
/// After the PCIe capability's 0x3c bytes.
pub const MSI_CAPABILITY: usize = 0x80;
/// MSI control: 64-bit addresses, one vector (MMC 0).
const MSI_64BIT: u16 = 1 << 7;
const CONFIG_BYTES: usize = 0x1000;
/// The self-test's registers, by offset from [`SELFTEST`].
const TEST_IOVA_LOW: usize = 0x00;
const TEST_IOVA_HIGH: usize = 0x04;
const TEST_LENGTH: usize = 0x08;
const TEST_COMMAND: usize = 0x0c;
const TEST_STATUS: usize = 0x10;
const TEST_SEED: usize = 0x14;
const TEST_REFUSAL: usize = 0x18;
const TEST_MOVED: usize = 0x1c;
const TEST_REGISTERS: usize = 8;

/// A self-test run the guest started ([`super::dma::run_selftest`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfTest {
    pub command: u32,
    pub iova: u64,
    pub length: u32,
    pub seed: u32,
}

/// Where a port's link is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Training {
    Down,
    /// Training since LTSSM start; up at this counter value.
    Until(u64),
    Up,
}

/// The `LNKSTA` a root port with `lnkcap` and `lnkctl2` reports with its
/// link up: x1, at `LNKCTL2`'s target speed bounded by `LNKCAP`'s maximum
/// (at least 2.5 GT/s), data-link layer active.
pub fn link_status(lnkcap: u32, lnkctl2: u16) -> u16 {
    let maximum = (lnkcap & 0xf).max(1) as u16;
    let target = match lnkctl2 & 0xf {
        0 => maximum,
        speed => speed.min(maximum),
    };
    DLL_ACTIVE | SLOT_CLOCK | 1 << 4 | target
}

/// The endpoint function behind the modeled port.
pub struct Endpoint {
    config: Box<[u8; CONFIG_BYTES]>,
    bar: Box<[u8; BAR_BYTES]>,
    /// The self-test registers, as words.
    test: [u32; TEST_REGISTERS],
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl Endpoint {
    pub fn new() -> Self {
        let mut config = Box::new([0u8; CONFIG_BYTES]);
        let mut put = |offset: usize, value: u32| {
            config[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(0x00, u32::from(DEVICE) << 16 | u32::from(VENDOR));
        put(0x04, 0x0010_0000); // status: capability list
        put(0x08, CLASS_REVISION);
        put(0x0c, 0); // type-0 header, single function
        put(0x2c, u32::from(DEVICE) << 16 | u32::from(VENDOR)); // subsystem
        put(0x34, CAPABILITY as u32);
        // PCIe capability v2, endpoint; next the MSI capability.
        put(CAPABILITY, 0x0002_0010 | (MSI_CAPABILITY as u32) << 8);
        put(CAPABILITY + 0x04, 0x0000_8000); // DEVCAP: role-based errors, 128 B
        put(CAPABILITY + 0x08, 0x0000_2810); // DEVCTL: 512 B reads, RO, NS
        put(CAPABILITY + 0x0c, 0x0000_0011); // LNKCAP: x1, 2.5 GT/s
        put(CAPABILITY + 0x10, u32::from(SLOT_CLOCK | 1 << 4 | 1) << 16); // LNKSTA
        put(CAPABILITY + 0x2c, 0x0000_0002); // LNKCAP2: 2.5 GT/s
        put(CAPABILITY + 0x30, 0x0000_0001); // LNKCTL2: 2.5 GT/s
        // MSI, 64-bit, one vector, disabled; no next capability.
        put(MSI_CAPABILITY, u32::from(MSI_64BIT) << 16 | 0x05);
        let mut test = [0; TEST_REGISTERS];
        test[TEST_SEED / 4] = 0xa5;
        Self {
            config,
            bar: Box::new([0u8; BAR_BYTES]),
            test,
        }
    }

    /// Whether the endpoint may master the bus (command bit 2).
    pub fn bus_master(&self) -> bool {
        self.command() & 4 != 0
    }

    /// The MSI the endpoint sends, when its MSI is enabled: the address and
    /// data software programmed.
    pub fn msi_message(&self) -> Option<(u64, u32)> {
        let control = self.word(MSI_CAPABILITY) >> 16;
        (control & 1 != 0).then(|| {
            let address = u64::from(self.word(MSI_CAPABILITY + 4))
                | u64::from(self.word(MSI_CAPABILITY + 8)) << 32;
            (address, self.word(MSI_CAPABILITY + 0xc) & 0xffff)
        })
    }

    /// Whether a self-test run is waiting for the upstream path.
    pub fn selftest_pending(&self) -> bool {
        self.test[TEST_STATUS / 4] & STATUS_BUSY != 0
    }

    /// The waiting self-test run, if any; it stays busy until finished.
    pub fn take_selftest(&mut self) -> Option<SelfTest> {
        self.selftest_pending().then(|| {
            let length = match self.test[TEST_LENGTH / 4] {
                0 => 64,
                length => length.min(SELFTEST_MAX),
            };
            SelfTest {
                command: self.test[TEST_COMMAND / 4],
                iova: u64::from(self.test[TEST_IOVA_LOW / 4])
                    | u64::from(self.test[TEST_IOVA_HIGH / 4]) << 32,
                length,
                seed: self.test[TEST_SEED / 4],
            }
        })
    }

    /// The run is over: `moved` bytes, refused with `refusal` (0 if not),
    /// and whether the MSI went out.
    pub fn finish_selftest(&mut self, moved: u32, refusal: u32, msi: bool) {
        let runs = (self.test[TEST_STATUS / 4] >> 16).wrapping_add(1) & 0xffff;
        self.test[TEST_STATUS / 4] = runs << 16
            | STATUS_DONE
            | if refusal != 0 { STATUS_REFUSED } else { 0 }
            | if msi { STATUS_MSI } else { 0 };
        self.test[TEST_REFUSAL / 4] = refusal;
        self.test[TEST_MOVED / 4] = moved;
    }

    fn write_test(&mut self, register: usize, value: u32) {
        match register {
            TEST_COMMAND if value == COMMAND_WRITE || value == COMMAND_READ => {
                self.test[TEST_COMMAND / 4] = value;
                let runs = self.test[TEST_STATUS / 4] & 0xffff_0000;
                self.test[TEST_STATUS / 4] = runs | STATUS_BUSY;
            }
            TEST_STATUS | TEST_REFUSAL | TEST_MOVED | TEST_COMMAND => {}
            _ => self.test[register / 4] = value,
        }
    }

    pub fn read(&self, register: usize, bytes: usize) -> u64 {
        (0..bytes).fold(0, |value, at| {
            let byte = self.config.get(register + at).copied().unwrap_or(0xff);
            value | u64::from(byte) << (8 * at)
        })
    }

    pub fn write(&mut self, register: usize, bytes: usize, value: u64) {
        for at in 0..bytes {
            let offset = register + at;
            if offset >= CONFIG_BYTES {
                continue;
            }
            let byte = (value >> (8 * at)) as u8;
            let mask = writable(offset);
            self.config[offset] = self.config[offset] & !mask | byte & mask;
        }
    }

    fn word(&self, offset: usize) -> u32 {
        u32::from_le_bytes(
            self.config[offset..offset + 4]
                .try_into()
                .unwrap_or_default(),
        )
    }

    /// The command register.
    pub fn command(&self) -> u16 {
        self.word(0x04) as u16
    }

    /// Where BAR 0 decodes, once assigned and with memory decoding on.
    pub fn bar(&self) -> Option<u64> {
        let base = u64::from(self.word(0x10) & !0xf);
        (self.command() & 2 != 0 && base != 0).then_some(base)
    }

    /// BAR 0 as software wrote it, decoding or not.
    pub fn bar_register(&self) -> u32 {
        self.word(0x10)
    }

    pub fn read_bar(&self, offset: usize, bytes: usize) -> u64 {
        (0..bytes).fold(0, |value, at| {
            let here = offset + at;
            let byte = match here.checked_sub(SELFTEST) {
                Some(register) if register < 4 * TEST_REGISTERS => {
                    (self.test[register / 4] >> (8 * (register % 4))) as u8
                }
                _ => self.bar.get(here).copied().unwrap_or(0xff),
            };
            value | u64::from(byte) << (8 * at)
        })
    }

    pub fn write_bar(&mut self, offset: usize, bytes: usize, value: u64) {
        if let Some(register) = offset.checked_sub(SELFTEST)
            && register < 4 * TEST_REGISTERS
        {
            // Registers take aligned 32-bit (or 64-bit, as two) writes.
            if register % 4 == 0 && (bytes == 4 || bytes == 8) {
                for word in 0..bytes / 4 {
                    if register + 4 * word < 4 * TEST_REGISTERS {
                        self.write_test(register + 4 * word, (value >> (32 * word)) as u32);
                    }
                }
            }
            return;
        }
        for at in 0..bytes {
            if let Some(byte) = self.bar.get_mut(offset + at) {
                *byte = (value >> (8 * at)) as u8;
            }
        }
    }
}

/// The bits software may change in each endpoint config byte.
fn writable(offset: usize) -> u8 {
    match offset {
        // Command: memory space, bus master, parity, SERR#; INTx disable.
        0x04 => 0x46,
        0x05 => 0x05,
        // Cache line size, latency timer, interrupt line.
        0x0c | 0x0d | 0x3c => 0xff,
        // BAR 0: a 4 KiB 32-bit non-prefetchable memory BAR, so its low
        // twelve bits read 0 and sizing reads back 0xfffff000.
        0x10 => 0x00,
        0x11 => 0xf0,
        0x12 | 0x13 => 0xff,
        // DEVCTL, LNKCTL, DEVCTL2, LNKCTL2.
        o if o == CAPABILITY + 0x08 || o == CAPABILITY + 0x10 => 0xff,
        o if o == CAPABILITY + 0x09 || o == CAPABILITY + 0x11 => 0xff,
        o if o == CAPABILITY + 0x28 || o == CAPABILITY + 0x29 => 0xff,
        o if o == CAPABILITY + 0x30 || o == CAPABILITY + 0x31 => 0xff,
        // MSI control: enable and multiple message enable; the address
        // (dword aligned, 64-bit) and the data.
        o if o == MSI_CAPABILITY + 2 => 0x71,
        o if o == MSI_CAPABILITY + 4 => 0xfc,
        o if (MSI_CAPABILITY + 5..MSI_CAPABILITY + 0xe).contains(&o) => 0xff,
        _ => 0,
    }
}

/// Whether a root port whose type-1 header reads `header(register, bytes)`
/// forwards a memory request for `address` downstream: memory space is on
/// and the address is in its memory window or its prefetchable window.
pub fn forwards(header: &dyn Fn(usize, usize) -> u64, address: u64) -> bool {
    let command = header(0x04, 2);
    if command & 2 == 0 {
        return false;
    }
    let base = (header(0x20, 2) & 0xfff0) << 16;
    let limit = (header(0x22, 2) & 0xfff0) << 16 | 0xf_ffff;
    let prefetch_base = (header(0x24, 2) & 0xfff0) << 16 | header(0x28, 4) << 32;
    let prefetch_limit = ((header(0x26, 2) & 0xfff0) << 16 | 0xf_ffff) | header(0x2c, 4) << 32;
    (base <= limit && (base..=limit).contains(&address))
        || (prefetch_base <= prefetch_limit && (prefetch_base..=prefetch_limit).contains(&address))
}

/// The root port's `LNKCAP` and `LNKCTL2` offsets in its header.
pub const LNKCAP: usize = EXPRESS + 0x0c;
pub const LNKSTA: usize = EXPRESS + 0x12;
pub const LNKCTL2: usize = EXPRESS + 0x30;
/// DesignWare port logic: `PL_DEBUG0` (LTSSM state).
pub const PL_DEBUG0: usize = 0x728;
