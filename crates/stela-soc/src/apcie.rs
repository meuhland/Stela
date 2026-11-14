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
//! The windows overlap: on 26B123 J473AP the PHY window (`reg[2]`) spans
//! the PHY IP and each port's PHY glue (probe2's trace logged the port-0
//! refclk handshake at `phy+0x4000` and the fuse copies at `phy+0x4xxxx`).
//! So the stub sees guest physical addresses and names each access by the
//! smallest window that holds it.
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
//!   waits on it); port `0x100` (`PORT_INTSTAT`) is write-one-to-clear;
//! - each port's LTSSM debug block `0x30` bit 12 reads set: the tracer
//!   snapshot is ready. `_captureLTSSMDebugTracer` (0xfffffe0009fa06e4)
//!   writes `0x38 = 1` and polls it with no timeout when a link-training
//!   timer fires (probe3: 20 million reads of `ltssm1+0x30`, port 1's
//!   disable never ran). The status's entry count reads 0, an empty tracer.
//!
//! In ECAM each port's own type-1 header (bus `bus-range`[0], device N,
//! function 0) is a root port with a PCIe capability at 0x70, which
//! `_initializeRootComplex` requires, and RAM-backed DesignWare port-logic
//! registers; every other function reads all-ones (no device) and drops
//! writes. So no endpoint is found, the link times out after the driver's
//! `ltssm-timeout`, and it disables the port.
//!
//! With the root-complex model (`--apcie`, [`ApcieProbe::with_model`]) one
//! port is a real one ([`link`]): its link trains, its interrupt reaches the
//! AIC, and an endpoint answers on its secondary bus, with a RAM-backed BAR
//! in the complex's memory window. The other ports stay as above.
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
use stela_contracts::interrupt::{AicInput, Wire};

pub mod link;
use link::{Endpoint, Training};

const NAME: &str = "apcie-probe";
/// Most `APCIE LINK`/`ENUM` model events a boot prints; they are never
/// subject to the access log's cap.
const EVENT_CAP: u32 = 256;

/// A stored register's key is its window's `reg` index shifted by this,
/// then its offset in the window.
const WINDOW_SHIFT: u32 = 40;
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
    /// The complex's Nth PCI memory window (`ranges`), with the model on.
    Memory(usize),
    /// A placed address no window holds, by its physical address.
    Outside,
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
            Self::Memory(n) => 0xe_0000 + n,
            Self::Outside => 0xf_ffff,
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
            Self::Memory(n) => write!(f, "mem{n}"),
            Self::Outside => f.write_str("pa"),
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
    /// Its link is up (only the modeled port's ever is): `LNKSTA` and the
    /// LTSSM state read as a trained link.
    link_up: bool,
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
        Self {
            device,
            bytes,
            link_up: false,
        }
    }

    fn stored(&self, register: usize, bytes: usize) -> u64 {
        (0..bytes).fold(0, |value, at| {
            value | u64::from(self.bytes.get(register + at).copied().unwrap_or(0xff)) << (8 * at)
        })
    }

    /// One byte as software reads it: stored, except the link's status
    /// while the link is up.
    fn byte(&self, offset: usize) -> u8 {
        if self.link_up {
            let status = link::link_status(
                self.stored(link::LNKCAP, 4) as u32,
                self.stored(link::LNKCTL2, 2) as u16,
            );
            match offset {
                o if o == link::LNKSTA => return status as u8,
                o if o == link::LNKSTA + 1 => return (status >> 8) as u8,
                o if o == link::PL_DEBUG0 => return link::LTSSM_L0 as u8,
                _ => {}
            }
        }
        self.bytes.get(offset).copied().unwrap_or(0xff)
    }

    fn read(&self, register: usize, bytes: usize) -> u64 {
        (0..bytes).fold(0, |value, at| {
            value | u64::from(self.byte(register + at)) << (8 * at)
        })
    }

    /// The secondary bus number, when one is set below this port.
    fn secondary(&self) -> u8 {
        self.bytes[0x19]
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
    /// Each window, its guest PA and size, smallest first.
    windows: Vec<(Window, u64, u64)>,
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
    model: Option<Model>,
}

/// The root-complex model's port (`--apcie`, [`link`]).
struct Model {
    /// The port's index, which is also its root port's device number.
    port: usize,
    /// The port's AIC input (the complex's `interrupts[port]`).
    irq: Option<AicInput>,
    level: bool,
    training: Training,
    endpoint: Endpoint,
    /// Each PCI memory window: guest PA, bytes, PCI address.
    memory: Vec<(u64, u64, u64)>,
    events: u32,
}

impl ApcieProbe {
    /// A stub for root complex `name` whose root ports' headers are on
    /// `first_bus` at the device numbers `ports`, with `windows` as
    /// (`reg` index, guest PA, bytes). It is placed at device offsets equal
    /// to guest PAs.
    pub fn new(name: &str, first_bus: u8, ports: &[u8], windows: &[(usize, u64, u64)]) -> Self {
        let mut windows: Vec<_> = windows
            .iter()
            .map(|&(index, physical, bytes)| (Window::of(index), physical, bytes))
            .collect();
        // Smallest first; of two alike, the later `reg` entry (a port's).
        windows.sort_by_key(|&(window, _, bytes)| (bytes, core::cmp::Reverse(window.index())));
        Self {
            name: name.into(),
            first_bus,
            ports: ports.iter().map(|&device| RootPort::new(device)).collect(),
            windows,
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
            model: None,
        }
    }

    /// Make port `port` a real one (`--apcie`): its link trains, `irq` is
    /// its AIC input, and an endpoint answers behind it, whose BAR decodes
    /// in `memory`, the complex's PCI memory windows as (guest PA, bytes,
    /// PCI address). The windows are placed at device offsets equal to their
    /// guest PAs, like the register windows. A port with no root port header
    /// stays a stub.
    pub fn with_model(
        mut self,
        port: usize,
        irq: Option<AicInput>,
        memory: &[(u64, u64, u64)],
    ) -> Self {
        if !self
            .ports
            .iter()
            .any(|root| usize::from(root.device) == port)
        {
            return self;
        }
        for (index, &(physical, bytes, _)) in memory.iter().enumerate() {
            self.windows.push((Window::Memory(index), physical, bytes));
        }
        self.windows
            .sort_by_key(|&(window, _, bytes)| (bytes, core::cmp::Reverse(window.index())));
        self.model = Some(Model {
            port,
            irq,
            level: false,
            training: Training::Down,
            endpoint: Endpoint::new(),
            memory: memory.to_vec(),
            events: 0,
        });
        self
    }

    /// The modeled port, if any.
    pub fn modeled_port(&self) -> Option<usize> {
        self.model.as_ref().map(|model| model.port)
    }

    /// Whether the modeled port's link is up.
    pub fn link_up(&self) -> bool {
        self.model
            .as_ref()
            .is_some_and(|model| model.training == Training::Up)
    }

    /// The device numbers of the root ports' own headers.
    pub fn root_ports(&self) -> Vec<u8> {
        self.ports.iter().map(|port| port.device).collect()
    }

    /// One model event line (`APCIE LINK`, `APCIE ENUM`), outside the
    /// access log's cap but within its own.
    fn event(&mut self, args: fmt::Arguments<'_>) {
        if let Some(model) = &mut self.model
            && model.events < EVENT_CAP
        {
            model.events += 1;
            self.flush();
            emit(args);
        }
    }

    /// Complete link training that is due at `now`.
    fn settle(&mut self, access: &mut Access<'_>) {
        let Some(model) = &mut self.model else {
            return;
        };
        let Training::Until(at) = model.training else {
            return;
        };
        if access.now < at {
            return;
        }
        model.training = Training::Up;
        let port = model.port;
        let speed;
        {
            let root = self
                .ports
                .iter_mut()
                .find(|root| usize::from(root.device) == port);
            speed = root.map_or(0, |root| {
                root.link_up = true;
                root.read(link::LNKSTA, 2) & 0xf
            });
        }
        // The link-up interrupt: status, then the AIC input if unmasked.
        let status = self.stored(Window::Port(port), 0x100) | link::LINK_UP;
        self.store(Window::Port(port), 0x100, status);
        let name = self.name.clone();
        let irq = self.model.as_ref().and_then(|model| model.irq);
        self.event(format_args!(
            "APCIE LINK {name} port{port} up: gen{speed} x1, endpoint {:04x}:{:04x} class {:06x} \
             on its secondary bus; INTSTAT LINK_UP, aic {}\n",
            link::VENDOR,
            link::DEVICE,
            link::CLASS_REVISION >> 8,
            irq.map_or(-1, |irq| i64::from(irq.irq)),
        ));
        self.update_irq(access);
    }

    /// The modeled port's link goes down (`why`).
    fn link_down(&mut self, why: &str) {
        let Some(model) = &mut self.model else {
            return;
        };
        if model.training == Training::Down {
            return;
        }
        model.training = Training::Down;
        let port = model.port;
        if let Some(root) = self
            .ports
            .iter_mut()
            .find(|root| usize::from(root.device) == port)
        {
            root.link_up = false;
        }
        let name = self.name.clone();
        self.event(format_args!("APCIE LINK {name} port{port} down: {why}\n"));
    }

    /// Drive the modeled port's AIC input from its unmasked status.
    fn update_irq(&mut self, access: &mut Access<'_>) {
        let Some(port) = self.modeled_port() else {
            return;
        };
        let pending =
            self.stored(Window::Port(port), 0x100) & !self.stored(Window::Port(port), 0x104);
        let Some(model) = &mut self.model else {
            return;
        };
        let level = pending != 0;
        if level != model.level {
            model.level = level;
            if let Some(irq) = model.irq {
                access.interrupts.set_level(Wire::Aic(irq), level);
            }
        }
    }

    /// The modeled port's root port and endpoint, when `offset` (ECAM)
    /// names the endpoint: the port's secondary bus, device 0, function 0,
    /// with the link up.
    fn endpoint(&mut self, offset: u64) -> Option<(&mut Endpoint, usize)> {
        let (bus, device, function, register) = ecam(offset);
        let model = self.model.as_mut()?;
        let root = self
            .ports
            .iter()
            .find(|root| usize::from(root.device) == model.port)?;
        let secondary = root.secondary();
        (model.training == Training::Up
            && bus != self.first_bus
            && bus == secondary
            && device == 0
            && function == 0)
            .then_some((&mut model.endpoint, register as usize))
    }

    /// Where an access to memory window `index` at `offset` lands in the
    /// endpoint's BAR: the BAR decodes its PCI address (or, as a host
    /// bridge without translation would program it, its guest PA), and the
    /// root port forwards it.
    fn bar_offset(&self, index: usize, offset: u64) -> Option<usize> {
        let model = self.model.as_ref()?;
        if model.training != Training::Up {
            return None;
        }
        let &(physical, _, pci) = model.memory.get(index)?;
        let root = self
            .ports
            .iter()
            .find(|root| usize::from(root.device) == model.port)?;
        let bar = model.endpoint.bar()?;
        let header = |register: usize, bytes: usize| root.read(register, bytes);
        [pci.wrapping_add(offset), physical.wrapping_add(offset)]
            .into_iter()
            .find(|&address| {
                address >= bar
                    && address - bar < link::BAR_BYTES as u64
                    && link::forwards(&header, address)
            })
            .map(|address| (address - bar) as usize)
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
            // LINKSTS: link up only on the modeled port with its link up.
            (Window::Port(port), 0x208) => {
                stored & !0b101 | u32::from(self.modeled_port() == Some(port) && self.link_up())
            }
            (Window::Port(_), 0x80c) => 0,
            // The LTSSM debug tracer's snapshot is ready, and empty.
            (Window::Ltssm(_), 0x30) => stored & !0x7f | 0x1000,
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
        if let Some((port, register)) = self.root_port(offset) {
            return port.read(register, bytes as usize);
        }
        if let Some((endpoint, register)) = self.endpoint(offset) {
            return endpoint.read(register, bytes as usize);
        }
        Width::from_bytes(bytes).map_or(u64::MAX, Width::mask)
    }

    fn write_config(&mut self, offset: u64, bytes: u64, value: u64) {
        if let Some((port, register)) = self.root_port(offset) {
            let device = port.device;
            let before = (port.stored(0x18, 4), port.stored(0x04, 2));
            port.write(register, bytes as usize, value);
            let after = (port.stored(0x18, 4), port.stored(0x04, 2));
            if self.modeled_port() == Some(usize::from(device)) && before != after {
                let name = self.name.clone();
                let first = self.first_bus;
                let (buses, command) = after;
                self.event(format_args!(
                    "APCIE ENUM {name} [{first:02x}:{device:02x}.0] root port: buses primary={} \
                     secondary={} subordinate={} command={command:#06x}\n",
                    buses & 0xff,
                    buses >> 8 & 0xff,
                    buses >> 16 & 0xff,
                ));
            }
            return;
        }
        let Some((endpoint, register)) = self.endpoint(offset) else {
            return;
        };
        let before = (endpoint.bar_register(), endpoint.command());
        endpoint.write(register, bytes as usize, value);
        let after = (endpoint.bar_register(), endpoint.command());
        // Sizing (all ones) is not an assignment worth a line.
        if before != after && after.0 != 0xffff_f000 {
            let name = self.name.clone();
            let (bus, ..) = ecam(offset);
            let (bar, command) = after;
            self.event(format_args!(
                "APCIE ENUM {name} [{bus:02x}:00.0] endpoint: BAR0={bar:#010x} \
                 command={command:#06x}{}\n",
                if command & 2 != 0 {
                    " (memory decoding on)"
                } else {
                    ""
                }
            ));
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

    fn write_register(&mut self, window: Window, offset: u64, bytes: u64, value: u64, now: u64) {
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
            if let Window::Port(port) = window
                && self.modeled_port() == Some(port)
            {
                self.port_written(port, here & !3, stored, next, now);
            }
            at += 4;
        }
    }

    /// The modeled port's register `offset` went from `stored` to `next`:
    /// LTSSM start trains the link; LTSSM, APPCLK or PERST# off drops it.
    fn port_written(&mut self, port: usize, offset: u64, stored: u32, next: u32, now: u64) {
        let rose = stored & 1 == 0 && next & 1 != 0;
        let fell = stored & 1 != 0 && next & 1 == 0;
        match offset {
            0x80 if rose => {
                if let Some(model) = &mut self.model
                    && model.training == Training::Down
                {
                    let at = now.saturating_add(link::TRAINING_TICKS);
                    model.training = Training::Until(at);
                    let name = self.name.clone();
                    self.event(format_args!(
                        "APCIE LINK {name} port{port} training: LTSSM started, link up in {} ms\n",
                        link::TRAINING_TICKS / 24_000
                    ));
                }
            }
            0x80 if fell => self.link_down("LTSSM stopped"),
            0x800 if fell => self.link_down("APPCLK off"),
            0x814 if fell => self.link_down("PERST# asserted"),
            _ => {}
        }
    }

    /// Deadline of the modeled port's link training, if it is training.
    fn training_deadline(&self) -> Option<u64> {
        match self.model.as_ref()?.training {
            Training::Until(at) => Some(at),
            _ => None,
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

    /// The smallest window holding guest PA `physical`, and the offset in it.
    fn place(&self, physical: u64) -> Place {
        self.windows
            .iter()
            .find(|&&(_, at, bytes)| at <= physical && physical - at < bytes)
            .map_or(
                Place(Window::Outside, physical & ((1 << WINDOW_SHIFT) - 1)),
                |&(window, at, _)| Place(window, physical - at),
            )
    }
}

fn is_power_of_ten(mut value: u64) -> bool {
    while value >= 10 && value.is_multiple_of(10) {
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
        self.settle(access);
        let place = self.place(offset);
        let bytes = width.bytes();
        let value = match place.0 {
            Window::Config => self.read_config(place.1, bytes),
            Window::Memory(index) => match (self.bar_offset(index, place.1), &self.model) {
                (Some(at), Some(model)) => model.endpoint.read_bar(at, bytes as usize),
                // Nothing decodes it: an unsupported request reads all ones.
                _ => width.mask(),
            },
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
        self.settle(access);
        let place = self.place(offset);
        let bytes = width.bytes();
        let value = value & width.mask();
        self.record(Direction::Write, place, bytes, value, access);
        match place.0 {
            Window::Config => self.write_config(place.1, bytes, value),
            Window::Memory(index) => {
                if let Some(at) = self.bar_offset(index, place.1)
                    && let Some(model) = &mut self.model
                {
                    model.endpoint.write_bar(at, bytes as usize, value);
                }
            }
            window => {
                self.write_register(window, place.1, bytes, value, access.now);
                self.update_irq(access);
            }
        }
        Ok(())
    }

    fn advance(&mut self, access: &mut Access<'_>) -> Result<Option<u64>, MmioError> {
        self.settle(access);
        let beat = match self.next_beat {
            Some(at) if access.now < at => at,
            due => {
                if due.is_some() && self.changed && self.beats < SUMMARY_CAP {
                    self.beats += 1;
                    self.changed = false;
                    self.flush();
                    self.summary("beat");
                }
                let next = access.now.saturating_add(BEAT_SECONDS * TICKS_PER_SECOND);
                self.next_beat = Some(next);
                next
            }
        };
        Ok(Some(
            self.training_deadline()
                .map_or(beat, |training| training.min(beat)),
        ))
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
