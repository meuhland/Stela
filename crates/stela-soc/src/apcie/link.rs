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
//! capability (x1, 2.5 GT/s), no MSI and no INTx, and one 4 KiB 32-bit
//! memory BAR backed by RAM, which answers once IOPCIFamily has assigned it
//! from the port's window and enabled memory decoding on the root port and
//! the endpoint.
use super::EXPRESS;
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
/// The endpoint's PCIe capability.
const CAPABILITY: usize = 0x40;
const CONFIG_BYTES: usize = 0x1000;

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
        // PCIe capability v2, endpoint, no next capability.
        put(CAPABILITY, 0x0002_0010);
        put(CAPABILITY + 0x04, 0x0000_8000); // DEVCAP: role-based errors, 128 B
        put(CAPABILITY + 0x08, 0x0000_2810); // DEVCTL: 512 B reads, RO, NS
        put(CAPABILITY + 0x0c, 0x0000_0011); // LNKCAP: x1, 2.5 GT/s
        put(CAPABILITY + 0x10, u32::from(SLOT_CLOCK | 1 << 4 | 1) << 16); // LNKSTA
        put(CAPABILITY + 0x2c, 0x0000_0002); // LNKCAP2: 2.5 GT/s
        put(CAPABILITY + 0x30, 0x0000_0001); // LNKCTL2: 2.5 GT/s
        Self {
            config,
            bar: Box::new([0u8; BAR_BYTES]),
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
            let byte = self.bar.get(offset + at).copied().unwrap_or(0xff);
            value | u64::from(byte) << (8 * at)
        })
    }

    pub fn write_bar(&mut self, offset: usize, bytes: usize, value: u64) {
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
