//! The PCIe root-complex probe's register stub (`--probe-apcie`,
//! `stela_soc_data::board::apcie_probe`): every `reg` window of an `apcie`
//! node, logged, with no PCIe behind it.
//!
//! The driver (`AppleT8112PCIe : AppleT811xPCIe : AppleEmbeddedPCIE`, 26B123
//! J473AP kernelcache) maps its windows by `reg` index
//! (`mapDeviceMemoryWithIndex`, no `reg-names`): 0 the ECAM config space,
//! 1 the common (root-complex) registers, 2 the PHY, 3 the PHY IP, 4 AXI2AF,
//! 5 the PMGR eFuses, then per port N five windows from 6 + 5N: the port's
//! registers, its LTSSM debug block, PHY glue, PHY IP lane and INTR2AXI.
//! Each window is placed at its own device offset (`index << WINDOW_SHIFT`).
//!
//! Every register stores what software writes and reads it back, except the
//! acknowledgements the driver polls without a timeout (from the
//! disassembly; the names are Linux `pcie-apple.c`'s):
//!
//! - PHY and each port's PHY glue `0x0`: bit 2 follows bit 0 and bit 3
//!   follows bit 1 (`PHY_LANE_CFG` REFCLK0/1 request and acknowledge);
//! - common `0x58` bit 0 (`CORE_RC_STAT` ready) and `0x28` bit 4
//!   (`CORE_RC_PHYIF_STAT` refclk) read set;
//! - port `0x804` bit 0 (`PORT_STATUS` ready) follows `0x800` bit 0
//!   (`PORT_APPCLK` enable), both ways: the disable path waits for it to
//!   clear;
//! - port `0x208` bits 0 and 2 (`PORT_LINKSTS` up and busy) read clear: no
//!   link, and nothing in flight;
//! - port `0x80c` reads 0 (no outstanding transactions; the disable path
//!   waits on it); port `0x100` (`PORT_INTSTAT`) is write-one-to-clear.
//!
//! In ECAM each port's own type-1 header (bus `bus-range`[0], device N,
//! function 0) is a root port with a PCIe capability at 0x70, which
//! `_initializeRootComplex` requires, and RAM-backed DesignWare port-logic
//! registers; every other function reads all-ones (no device) and drops
//! writes. So no endpoint is ever found, every link times out after the
//! driver's `ltssm-timeout`, and it disables the port.
//!
//! No access is ever refused: a probe must not stop the machine. Each access
//! is logged as one `APCIE` line (the guest PC is not known at the device
//! boundary; the line gives the PE and the time instead). An access repeated
//! with the same value is counted, not printed, until something else
//! happens (`APCIE poll`), with a reminder at 1000, 10000, ... repeats. The
//! log stops at [`LINE_CAP`] lines; a per-register table (`APCIE SUMMARY`)
//! follows every [`BEAT_SECONDS`] while anything changed, when a port is
//! disabled and when the cap is reached.
use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::fmt;
use core::sync::atomic::{AtomicPtr, Ordering};
use stela_contracts::Refusal;
use stela_contracts::bus::{Access, Lifecycle, LifecycleState, MmioDevice, MmioError, Width};

const NAME: &str = "apcie-probe";

/// Each window's device offset is its `reg` index shifted by this.
pub const WINDOW_SHIFT: u32 = 40;
/// The most `APCIE` access lines a boot prints.
pub const LINE_CAP: u32 = 20_000;
/// Seconds between summaries while the registers change.
pub const BEAT_SECONDS: u64 = 10;
/// Most summaries a boot prints at the beat and at port disables each.
const SUMMARY_CAP: u32 = 8;
/// Most registers a summary lists (and tracks).
const TRACKED: usize = 2048;
const TICKS_PER_SECOND: u64 = 24_000_000;
const CONFIG_BYTES: usize = 0x1000;

static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Where every probe line goes; nothing is printed until it is set.
pub fn set_sink(sink: fn(fmt::Arguments<'_>)) {
    SINK.store(sink as *mut (), Ordering::Release);
}

fn emit(args: fmt::Arguments<'_>) {
    let sink = SINK.load(Ordering::Acquire);
    if !sink.is_null() {
        // SAFETY: SINK only ever holds a `fn(fmt::Arguments<'_>)` stored by
        // `set_sink`; function and data pointers have the same size here.
        let sink: fn(fmt::Arguments<'_>) = unsafe { core::mem::transmute(sink) };
        sink(args);
    }
}

/// What a window is, from its `reg` index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Window {
    Config,
    Common,
    Phy,
    PhyIp,
    Axi2Af,
    Fuse,
    Port(usize),
    Ltssm(usize),
    PhyGlue(usize),
    PhyIpLane(usize),
    Intr2Axi(usize),
}

impl Window {
    pub fn of(index: usize) -> Self {
        match index {
            0 => Self::Config,
            1 => Self::Common,
            2 => Self::Phy,
            3 => Self::PhyIp,
            4 => Self::Axi2Af,
            5 => Self::Fuse,
            n => {
                let port = (n - 6) / 5;
                match (n - 6) % 5 {
                    0 => Self::Port(port),
                    1 => Self::Ltssm(port),
                    2 => Self::PhyGlue(port),
                    3 => Self::PhyIpLane(port),
                    _ => Self::Intr2Axi(port),
                }
            }
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Config => 0,
            Self::Common => 1,
            Self::Phy => 2,
            Self::PhyIp => 3,
            Self::Axi2Af => 4,
            Self::Fuse => 5,
            Self::Port(n) => 6 + 5 * n,
            Self::Ltssm(n) => 7 + 5 * n,
            Self::PhyGlue(n) => 8 + 5 * n,
            Self::PhyIpLane(n) => 9 + 5 * n,
            Self::Intr2Axi(n) => 10 + 5 * n,
        }
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config => f.write_str("config"),
            Self::Common => f.write_str("rc"),
            Self::Phy => f.write_str("phy"),
            Self::PhyIp => f.write_str("phy-ip"),
            Self::Axi2Af => f.write_str("axi2af"),
            Self::Fuse => f.write_str("fuse"),
            Self::Port(n) => write!(f, "port{n}"),
            Self::Ltssm(n) => write!(f, "ltssm{n}"),
            Self::PhyGlue(n) => write!(f, "phy-glue{n}"),
            Self::PhyIpLane(n) => write!(f, "phy-ip-lane{n}"),
            Self::Intr2Axi(n) => write!(f, "intr2axi{n}"),
        }
    }
}

/// A register's place for the log: its window and offset in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Place(Window, u64);

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Place(window, offset) = *self;
        write!(f, "{window}+{offset:#x}")?;
        if window == Window::Config {
            let (bus, device, function, register) = ecam(offset);
            write!(f, "[{bus:02x}:{device:02x}.{function}+{register:#x}]")?;
        }
        Ok(())
    }
}

fn ecam(offset: u64) -> (u8, u8, u8, u64) {
    (
        (offset >> 20) as u8,
        ((offset >> 15) & 0x1f) as u8,
        ((offset >> 12) & 7) as u8,
        offset & 0xfff,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    direction: Direction,
    place: Place,
    bytes: u64,
    value: u64,
    count: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Stat {
    reads: u64,
    writes: u64,
    last: u64,
}

/// One root port's own configuration header and port logic.
struct RootPort {
    device: u8,
    bytes: Box<[u8; CONFIG_BYTES]>,
}

/// Configuration bytes software cannot change, unless DBI writes to
/// read-only registers are enabled (port logic `0x8bc` bit 0) where noted.
fn read_only(register: usize, dbi_writable: bool) -> bool {
    match register {
        // Identity: vendor, device, revision and class.
        0x00..=0x03 | 0x08..=0x0b => !dbi_writable,
        // Status (write-one-to-clear; nothing to clear), header type.
        0x06 | 0x07 | 0x0e => true,
        // No BARs, no I/O window, no expansion ROM; the capability pointer.
        0x10..=0x17 | 0x1c | 0x1d | 0x30..=0x34 | 0x38..=0x3b => true,
        // The PCIe capability's header, DEVCAP, LNKCAP, LNKSTA, DEVCAP2 and
        // LNKCAP2, and the end of the extended capability list.
        0x70..=0x77 | 0x7c..=0x7f | 0x82 | 0x83 | 0x94..=0x97 | 0x9c..=0x9f => true,
        0x100..=0x103 => true,
        _ => false,
    }
}

/// Port logic: DBI_RO_WR_EN (DesignWare `MISC_CONTROL_1`).
const DBI_RO_WR_EN: usize = 0x8bc;
/// Where the root port's PCIe capability is: the driver's fixed accesses
/// at 0x80 (LNKCTL) and 0xa0 (LNKCTL2) put it at 0x70.
const EXPRESS: usize = 0x70;

impl RootPort {
    fn new(device: u8) -> Self {
        let mut bytes = Box::new([0u8; CONFIG_BYTES]);
        let mut put = |offset: usize, value: u32| {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        // Apple's vendor ID; the device ID is the probe's own.
        put(0x00, 0x100c_106b);
        put(0x04, 0x0010_0000); // status: capability list
        put(0x08, 0x0604_0000); // PCI-to-PCI bridge
        put(0x0c, 0x0001_0000); // type-1 header
        put(0x34, EXPRESS as u32);
        // PCIe capability v2, root port, no next capability.
        put(EXPRESS, 0x0042_0010);
        put(EXPRESS + 0x04, 0x0000_8000); // DEVCAP: role-based errors
        // LNKCAP: port N, data-link-layer active reporting, x1, 8 GT/s.
        put(EXPRESS + 0x0c, (u32::from(device) << 24) | 0x0010_0013);
        // LNKCAP2: 2.5, 5 and 8 GT/s; LNKCTL2: target 8 GT/s.
        put(EXPRESS + 0x2c, 0x0000_000e);
        put(EXPRESS + 0x30, 0x0000_0003);
        Self { device, bytes }
    }

    fn read(&self, register: usize, bytes: usize) -> u64 {
        (0..bytes).fold(0, |value, at| {
            value | u64::from(self.bytes.get(register + at).copied().unwrap_or(0xff)) << (8 * at)
        })
    }

    fn write(&mut self, register: usize, bytes: usize, value: u64) {
        let dbi_writable = self.bytes[DBI_RO_WR_EN] & 1 != 0;
        for at in 0..bytes {
            let offset = register + at;
            if offset < CONFIG_BYTES && !read_only(offset, dbi_writable) {
                self.bytes[offset] = (value >> (8 * at)) as u8;
            }
        }
    }
}

/// The stub for one root complex's windows.
pub struct ApcieProbe {
    name: String,
    first_bus: u8,
    ports: Vec<RootPort>,
    /// Stored 32-bit words by device offset (window and offset).
    words: BTreeMap<u64, u32>,
    lines: u32,
    capped: bool,
    unlogged: u64,
    pending: Option<Seen>,
    stats: BTreeMap<Place, Stat>,
    untracked: u64,
    changed: bool,
    next_beat: Option<u64>,
    beats: u32,
    disables: u32,
    lifecycle: Option<LifecycleState>,
}

impl ApcieProbe {
    /// A stub for root complex `name` whose root ports' headers are on
    /// `first_bus` at the device numbers `ports`.
    pub fn new(name: &str, first_bus: u8, ports: &[u8]) -> Self {
        Self {
            name: name.into(),
            first_bus,
            ports: ports.iter().map(|&device| RootPort::new(device)).collect(),
            words: BTreeMap::new(),
            lines: 0,
            capped: false,
            unlogged: 0,
            pending: None,
            stats: BTreeMap::new(),
            untracked: 0,
            changed: false,
            next_beat: None,
            beats: 0,
            disables: 0,
            lifecycle: None,
        }
    }

    /// The device numbers of the root ports' own headers.
    pub fn root_ports(&self) -> Vec<u8> {
        self.ports.iter().map(|port| port.device).collect()
    }

    fn stored(&self, window: Window, offset: u64) -> u32 {
        let key = ((window.index() as u64) << WINDOW_SHIFT) | (offset & !3);
        self.words.get(&key).copied().unwrap_or(0)
    }

    fn store(&mut self, window: Window, offset: u64, value: u32) {
        let key = ((window.index() as u64) << WINDOW_SHIFT) | (offset & !3);
        self.words.insert(key, value);
    }

    /// What a 32-bit register reads: its stored value with the
    /// acknowledgements the driver waits for.
    fn view(&self, window: Window, offset: u64) -> u32 {
        let stored = self.stored(window, offset);
        match (window, offset & !3) {
            (Window::Common, 0x58) => stored | 1,
            (Window::Common, 0x28) => stored | 0x10,
            (Window::Phy | Window::PhyGlue(_), 0x0) => stored | (stored & 3) << 2,
            (Window::Port(_), 0x804) => stored & !1 | self.stored(window, 0x800) & 1,
            (Window::Port(_), 0x208) => stored & !0b101,
            (Window::Port(_), 0x80c) => 0,
            _ => stored,
        }
    }

    fn root_port(&mut self, offset: u64) -> Option<(&mut RootPort, usize)> {
        let (bus, device, function, register) = ecam(offset);
        if bus != self.first_bus || function != 0 {
            return None;
        }
        self.ports
            .iter_mut()
            .find(|port| port.device == device)
            .map(|port| (port, register as usize))
    }

    fn read_config(&mut self, offset: u64, bytes: u64) -> u64 {
        match self.root_port(offset) {
            Some((port, register)) => port.read(register, bytes as usize),
            None => Width::from_bytes(bytes).map_or(u64::MAX, Width::mask),
        }
    }

    fn write_config(&mut self, offset: u64, bytes: u64, value: u64) {
        if let Some((port, register)) = self.root_port(offset) {
            port.write(register, bytes as usize, value);
        }
    }

    fn read_register(&self, window: Window, offset: u64, bytes: u64) -> u64 {
        (0..bytes).step_by(4).fold(0, |value, at| {
            let word = u64::from(self.view(window, offset + at));
            let shift = 8 * ((offset + at) & 3);
            let width = bytes.min(4);
            let part = (word >> shift) & ((1u64 << (8 * width)) - 1);
            value | part << (8 * at)
        })
    }

    fn write_register(&mut self, window: Window, offset: u64, bytes: u64, value: u64) {
        let mut at = 0;
        while at < bytes {
            let here = offset + at;
            let width = bytes.min(4);
            let shift = 8 * (here & 3);
            let mask = (((1u64 << (8 * width)) - 1) << shift) as u32;
            let incoming = (((value >> (8 * at)) << shift) as u32) & mask;
            let stored = self.stored(window, here);
            let next = match (window, here & !3) {
                // PORT_INTSTAT: write one to clear.
                (Window::Port(_), 0x100) => stored & !incoming,
                _ => stored & !mask | incoming,
            };
            self.store(window, here, next);
            if let (Window::Port(port), 0x800) = (window, here & !3)
                && stored & 1 != 0
                && next & 1 == 0
            {
                self.port_disabled(port);
            }
            at += 4;
        }
    }

    fn port_disabled(&mut self, port: usize) {
        self.flush();
        let name = self.name.clone();
        self.line(format_args!(
            "APCIE EVENT {name} port{port} appclk off (the driver disables the port)\n"
        ));
        if self.disables < SUMMARY_CAP {
            self.disables += 1;
            self.summary("port disabled");
        }
    }

    /// One logged line, while under the cap.
    fn line(&mut self, args: fmt::Arguments<'_>) {
        if self.lines < LINE_CAP {
            self.lines += 1;
            emit(args);
        } else {
            self.unlogged += 1;
            if !self.capped {
                self.capped = true;
                emit(format_args!(
                    "APCIE CAP {} {LINE_CAP} lines; later accesses are only counted\n",
                    self.name
                ));
                self.summary("line cap");
            }
        }
    }

    /// Print the pending repeated access, if it repeated.
    fn flush(&mut self) {
        if let Some(seen) = self.pending.take()
            && seen.count > 1
        {
            self.line(format_args!(
                "APCIE {} {} size={} val={:#x} x{}\n",
                match seen.direction {
                    Direction::Read => "poll",
                    Direction::Write => "repeat w",
                },
                seen.place,
                seen.bytes,
                seen.value,
                seen.count
            ));
        }
    }

    fn record(
        &mut self,
        direction: Direction,
        place: Place,
        bytes: u64,
        value: u64,
        access: &Access<'_>,
    ) {
        if self.stats.len() < TRACKED || self.stats.contains_key(&place) {
            let stat = self.stats.entry(place).or_default();
            match direction {
                Direction::Read => stat.reads += 1,
                Direction::Write => stat.writes += 1,
            }
            stat.last = value;
        } else {
            self.untracked += 1;
        }
        if let Some(seen) = &mut self.pending
            && seen.direction == direction
            && seen.place == place
            && seen.bytes == bytes
            && seen.value == value
        {
            seen.count += 1;
            let count = seen.count;
            if count >= 1000 && is_power_of_ten(count) {
                self.line(format_args!(
                    "APCIE poll {place} size={bytes} val={value:#x} x{count} (still repeating)\n"
                ));
            }
            return;
        }
        self.changed = true;
        self.flush();
        let millis = access.now / (TICKS_PER_SECOND / 1000);
        self.line(format_args!(
            "APCIE {} {place} size={bytes} val={value:#x} cpu={} t={}.{:03}s\n",
            match direction {
                Direction::Read => "r",
                Direction::Write => "w",
            },
            access.cpu,
            millis / 1000,
            millis % 1000
        ));
        self.pending = Some(Seen {
            direction,
            place,
            bytes,
            value,
            count: 1,
        });
    }

    /// The per-register table.
    fn summary(&mut self, why: &str) {
        emit(format_args!(
            "APCIE SUMMARY {} ({why}) registers={} untracked={} unlogged={} region off reads writes last_val\n",
            self.name,
            self.stats.len(),
            self.untracked,
            self.unlogged
        ));
        for (Place(window, offset), stat) in &self.stats {
            emit(format_args!(
                "APCIE SUMMARY {window} {offset:#x} {} {} {:#x}\n",
                stat.reads, stat.writes, stat.last
            ));
        }
        emit(format_args!("APCIE SUMMARY {} end\n", self.name));
    }

    fn place(offset: u64) -> Place {
        Place(
            Window::of((offset >> WINDOW_SHIFT) as usize),
            offset & ((1 << WINDOW_SHIFT) - 1),
        )
    }
}

fn is_power_of_ten(mut value: u64) -> bool {
    while value >= 10 && value % 10 == 0 {
        value /= 10;
    }
    value == 1
}

impl MmioDevice for ApcieProbe {
    fn name(&self) -> &'static str {
        NAME
    }

    fn lifecycle(&mut self) -> Option<&mut dyn Lifecycle> {
        Some(self)
    }

    fn read(
        &mut self,
        offset: u64,
        width: Width,
        access: &mut Access<'_>,
    ) -> Result<u64, MmioError> {
        let place = Self::place(offset);
        let bytes = width.bytes();
        let value = match place.0 {
            Window::Config => self.read_config(place.1, bytes),
            window => self.read_register(window, place.1, bytes),
        };
        self.record(Direction::Read, place, bytes, value, access);
        Ok(value)
    }

    fn write(
        &mut self,
        offset: u64,
        width: Width,
        value: u64,
        access: &mut Access<'_>,
    ) -> Result<(), MmioError> {
        let place = Self::place(offset);
        let bytes = width.bytes();
        let value = value & width.mask();
        self.record(Direction::Write, place, bytes, value, access);
        match place.0 {
            Window::Config => self.write_config(place.1, bytes, value),
            window => self.write_register(window, place.1, bytes, value),
        }
        Ok(())
    }

    fn advance(&mut self, access: &mut Access<'_>) -> Result<Option<u64>, MmioError> {
        let beat = BEAT_SECONDS * TICKS_PER_SECOND;
        match self.next_beat {
            Some(at) if access.now >= at => {
                if self.changed && self.beats < SUMMARY_CAP {
                    self.beats += 1;
                    self.changed = false;
                    self.flush();
                    self.summary("beat");
                }
            }
            Some(at) => return Ok(Some(at)),
            None => {}
        }
        let next = access.now.saturating_add(beat);
        self.next_beat = Some(next);
        Ok(Some(next))
    }
}

impl Lifecycle for ApcieProbe {
    fn check_lifecycle(&self, _: LifecycleState) -> Result<(), Refusal> {
        Ok(())
    }

    fn set_lifecycle(&mut self, proposed: LifecycleState, _: &mut Access<'_>) {
        if self.lifecycle != Some(proposed) {
            self.lifecycle = Some(proposed);
            self.flush();
            let name = self.name.clone();
            self.line(format_args!(
                "APCIE EVENT {name} power powered={} clocked={} quiesced={} reset={}\n",
                proposed.powered, proposed.clocked, proposed.quiesced, proposed.reset
            ));
        }
    }
}

#[cfg(test)]
mod tests;
