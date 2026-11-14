//! The PCIe root-complex probe (`--probe-apcie`): with it on, the boot shows
//! the guest each `apcie,*` root complex, its ports and their DARTs, and the
//! machine backs every register window of the complex with a logging stub
//! (`stela_soc::apcie`), so a boot records how far macOS's own
//! `AppleT8112PCIe`/`AppleEmbeddedPCIE` driver brings its ports up.
//!
//! There is no PCIe model: the stub answers the handshakes the driver waits
//! for and stores everything else, and no endpoint is behind any port. The
//! probe is off by default, and then nothing here changes what the machine
//! admits or mounts.
//!
//! The switch is process-wide because both the boot bundle's DeviceTree
//! admission and the board's power-resource ownership ask [`super::models`],
//! and the boot sets it once, before either runs.
use super::apertures;
use alloc::{format, string::String, vec::Vec};
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use stela_contracts::adt::Node;

static APCIE: AtomicBool = AtomicBool::new(false);
static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Where the probe's preparation notes go ([`note`]); nothing is printed
/// until it is set.
pub fn set_sink(sink: fn(fmt::Arguments<'_>)) {
    SINK.store(sink as *mut (), Ordering::Release);
}

/// One `APCIE PREPARE` line: what the probe's admission changed and why.
pub fn note(args: fmt::Arguments<'_>) {
    let sink = SINK.load(Ordering::Acquire);
    if !sink.is_null() {
        // SAFETY: SINK only ever holds a `fn(fmt::Arguments<'_>)` stored by
        // `set_sink`; function and data pointers have the same size here.
        let sink: fn(fmt::Arguments<'_>) = unsafe { core::mem::transmute(sink) };
        sink(args);
    }
}

/// The visible DARTs under `/arm-io` the board model refuses, by `name`,
/// with the refusal: the same per-DART check [`super::board`] makes
/// (`dart_topology::iommu`), run on each DART alone so the probe's
/// admission can retreat from the ones it showed rather than stop the boot.
/// An image without an `/arm-io` gives none.
pub fn dart_refusals(tree: &[u8]) -> Vec<(String, String)> {
    let Some(io) = Node::root(tree).and_then(|root| root.child("arm-io")) else {
        return Vec::new();
    };
    let extent = |offset: u64, bytes: u64| io.translate_range(offset, bytes);
    let mut refused = Vec::new();
    apertures::visit(&io, &extent, &mut |node, translate| {
        let dart = node.property("compatible").is_some_and(|list| {
            list.split(|byte| *byte == 0)
                .any(|name| name.starts_with(b"dart,"))
        });
        if dart && let Err(error) = super::dart_topology::iommu(node, translate) {
            refused.push((super::name_of(node), format!("{error}")));
        }
    });
    refused
}

/// Turn the probe on or off for every later admission and board.
pub fn set_apcie(on: bool) {
    APCIE.store(on, Ordering::Release);
}

/// Whether the probe is on.
pub fn apcie() -> bool {
    APCIE.load(Ordering::Acquire)
}

/// The port the root-complex model (`--apcie`) makes real: J473AP's
/// `apcie[2:lan-1gb]`, the built-in 1 GbE port. Its endpoint is a plain
/// network function with nothing of the platform's own around it (the
/// `wlan` port's endpoint is driven through the AMFM and Bluetooth, the
/// `pcie-xhci` port's carries the USB-A ports' wiring), so a dummy function,
/// then an emulated NIC or NVMe controller, or a passed-through device can
/// stand in for it with macOS's own PCI drivers.
pub const MODELED_PORT: u32 = 2;
const NO_PORT: u32 = u32::MAX;
static MODEL: AtomicU32 = AtomicU32::new(NO_PORT);

/// Model `port` (`--apcie`), or no port. The probe must be on as well.
pub fn set_model_port(port: Option<u32>) {
    MODEL.store(port.unwrap_or(NO_PORT), Ordering::Release);
}

/// The port the root-complex model makes real, while the probe is on.
pub fn model_port() -> Option<u32> {
    let port = MODEL.load(Ordering::Acquire);
    (apcie() && port != NO_PORT).then_some(port)
}

/// Whether `name` is a PCIe root complex's `compatible` (`apcie,t8112`).
/// The `apcie-phy` style names of other blocks do not match.
pub fn is_root_complex(name: &str) -> bool {
    name.strip_prefix("apcie,")
        .is_some_and(|soc| !soc.is_empty())
}

/// Whether the probe claims a node with this `compatible` entry and these
/// properties: a root complex, or one of its ports (a node with an
/// `apcie-port` index), only while the probe is on.
pub fn claims<'a>(name: &str, property: impl Fn(&str) -> Option<&'a [u8]>) -> bool {
    claims_with(apcie(), name, property)
}

pub(super) fn claims_with<'a>(
    on: bool,
    name: &str,
    property: impl Fn(&str) -> Option<&'a [u8]>,
) -> bool {
    on && (is_root_complex(name) || property("apcie-port").is_some())
}

/// One register window of a root complex: its `reg` index, which the driver
/// maps by index, and its translated guest PA, or `None` when `/arm-io`'s
/// ranges do not cover it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApcieWindow {
    pub index: usize,
    pub physical: Option<u64>,
    pub bytes: u64,
}

/// A port child: its name and its `apcie-port` index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApciePort {
    pub name: String,
    pub index: u32,
}

/// A visible root complex the probe backs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Apcie {
    pub name: String,
    /// Every `reg` entry in order.
    pub windows: Vec<ApcieWindow>,
    /// The AIC inputs of `interrupts`, one per port in port order.
    pub interrupts: Vec<u32>,
    pub ports: Vec<ApciePort>,
    /// `bus-range`'s first bus, where the root ports' own headers are.
    pub first_bus: u8,
    /// The PCI memory windows of `ranges`.
    pub memory: Vec<ApcieMemory>,
    /// The port the root-complex model makes real (`--apcie`), when the
    /// complex has it.
    pub model: Option<u32>,
}

/// One PCI memory window of a root complex's `ranges`: its PCI address,
/// its guest PA (`None` when `/arm-io`'s ranges do not cover it) and size,
/// and whether it is prefetchable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApcieMemory {
    pub pci: u64,
    pub physical: Option<u64>,
    pub bytes: u64,
    pub prefetchable: bool,
}

/// `ranges` entries: the PCI address (`phys.hi`, then the 64-bit address
/// as two cells, low first, as XNU reads multi-cell values), the parent
/// address (two cells) and the size (two cells), seven cells in all, the
/// layout `AppleEmbeddedPCIE::configure` (0xfffffe000944286c) copies.
/// Memory spaces only (`phys.hi` bits 25:24 of 2 or 3); I/O is skipped.
pub fn memory_ranges(io: &Node<'_>, raw: &[u8]) -> Vec<ApcieMemory> {
    if !raw.len().is_multiple_of(28) {
        return Vec::new();
    }
    let word = |entry: &[u8; 28], cell: usize| {
        u64::from(u32::from_le_bytes(
            entry[4 * cell..4 * cell + 4].try_into().unwrap_or_default(),
        ))
    };
    raw.as_chunks::<28>()
        .0
        .iter()
        .filter_map(|entry| {
            let hi = word(entry, 0);
            let space = hi >> 24 & 3;
            if space < 2 {
                return None;
            }
            let pci = word(entry, 1) | word(entry, 2) << 32;
            let parent = word(entry, 3) | word(entry, 4) << 32;
            let bytes = word(entry, 5) | word(entry, 6) << 32;
            (bytes != 0).then(|| ApcieMemory {
                pci,
                physical: io.translate_range(parent, bytes),
                bytes,
                prefetchable: hi & 1 << 30 != 0,
            })
        })
        .collect()
}

/// The visible root complexes under `/arm-io`, when the probe is on.
pub(super) fn root_complexes(io: &Node<'_>) -> Vec<Apcie> {
    if !apcie() {
        return Vec::new();
    }
    let Some(children) = io.children() else {
        return Vec::new();
    };
    children
        .iter()
        .filter(|node| {
            node.property("compatible").is_some_and(|list| {
                list.split(|byte| *byte == 0)
                    .filter_map(|name| core::str::from_utf8(name).ok())
                    .any(is_root_complex)
            })
        })
        .map(|node| root_complex(io, node))
        .collect()
}

fn text(bytes: &[u8]) -> String {
    String::from(
        core::str::from_utf8(bytes.split(|byte| *byte == 0).next().unwrap_or_default())
            .unwrap_or("?"),
    )
}

fn root_complex(io: &Node<'_>, node: &Node<'_>) -> Apcie {
    let windows = apertures::registers(node)
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, (offset, bytes))| ApcieWindow {
            index,
            physical: io.translate_range(offset, bytes),
            bytes,
        })
        .collect();
    let ports = node
        .children()
        .unwrap_or_default()
        .iter()
        .filter_map(|child| {
            Some(ApciePort {
                name: text(child.property("name").unwrap_or(b"?")),
                index: child.first_u32("apcie-port")?,
            })
        })
        .collect::<Vec<ApciePort>>();
    let model = model_port().filter(|&port| ports.iter().any(|child| child.index == port));
    Apcie {
        name: text(node.property("name").unwrap_or(b"?")),
        windows,
        interrupts: node.cells32("interrupts").unwrap_or_default(),
        ports,
        first_bus: node
            .property("bus-range")
            .and_then(|range| range.first().copied())
            .unwrap_or(0),
        memory: node
            .property("ranges")
            .map(|raw| memory_ranges(io, raw))
            .unwrap_or_default(),
        model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_complexes_and_their_ports_are_claimed_and_only_when_on() {
        let none = |_: &str| None;
        let port = |key: &str| (key == "apcie-port").then_some(&[1u8, 0, 0, 0][..]);
        for on in [false, true] {
            assert_eq!(claims_with(on, "apcie,t8112", none), on);
            assert_eq!(claims_with(on, "pci-bridge", port), on);
            assert!(!claims_with(on, "apcie-phy,t8112", none));
            assert!(!claims_with(on, "apcie,", none));
            assert!(!claims_with(on, "dart,t8110", none));
        }
    }

    #[test]
    fn the_probe_is_off_by_default() {
        // Nothing in this crate's tests turns it on.
        assert!(!apcie());
        assert!(!claims("apcie,t8112", |_| None));
        // A model port means nothing without the probe.
        set_model_port(Some(MODELED_PORT));
        assert_eq!(model_port(), None);
        set_model_port(None);
    }

    #[test]
    fn memory_windows_come_from_ranges_through_arm_io() {
        use stela_contracts::adt::encode;
        let words = |values: &[u64]| -> Vec<u8> {
            values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect()
        };
        // `/arm-io` maps 0x4_0000_0000 to 0x6_0000_0000.
        let io = encode(
            &[
                ("name", b"arm-io\0"),
                (
                    "ranges",
                    &words(&[0x4_0000_0000, 0x6_0000_0000, 0x1_0000_0000]),
                ),
            ],
            &[],
        );
        let io = Node::root(&io).unwrap();
        let entry = |hi: u32, pci: u64, parent: u64, bytes: u64| -> Vec<u8> {
            [
                hi,
                pci as u32,
                (pci >> 32) as u32,
                parent as u32,
                (parent >> 32) as u32,
            ]
            .into_iter()
            .chain([bytes as u32, (bytes >> 32) as u32])
            .flat_map(u32::to_le_bytes)
            .collect()
        };
        // J473AP-like: a 64-bit prefetchable window, a 32-bit window whose
        // PCI address differs from its CPU address, and an I/O window.
        let raw = [
            entry(0x4300_0000, 0x6_a000_0000, 0x4_a000_0000, 0x2000_0000),
            entry(0x0200_0000, 0xc000_0000, 0x4_c000_0000, 0x4000_0000),
            entry(0x0100_0000, 0, 0x4_f000_0000, 0x1_0000),
        ]
        .concat();
        let memory = memory_ranges(&io, &raw);
        assert_eq!(
            memory,
            [
                ApcieMemory {
                    pci: 0x6_a000_0000,
                    physical: Some(0x6_a000_0000),
                    bytes: 0x2000_0000,
                    prefetchable: true,
                },
                ApcieMemory {
                    pci: 0xc000_0000,
                    physical: Some(0x6_c000_0000),
                    bytes: 0x4000_0000,
                    prefetchable: false,
                },
            ]
        );
        // A window outside `/arm-io`'s ranges has no PA; a malformed
        // property gives no window.
        let outside = entry(0x0200_0000, 0, 0x9_0000_0000, 0x1000);
        assert_eq!(memory_ranges(&io, &outside)[0].physical, None);
        assert!(memory_ranges(&io, &raw[..27]).is_empty());
    }
}
