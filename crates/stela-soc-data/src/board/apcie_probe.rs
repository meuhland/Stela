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
use alloc::{string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};
use stela_contracts::adt::Node;

static APCIE: AtomicBool = AtomicBool::new(false);

/// Turn the probe on or off for every later admission and board.
pub fn set_apcie(on: bool) {
    APCIE.store(on, Ordering::Release);
}

/// Whether the probe is on.
pub fn apcie() -> bool {
    APCIE.load(Ordering::Acquire)
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
        .collect();
    Apcie {
        name: text(node.property("name").unwrap_or(b"?")),
        windows,
        interrupts: node.cells32("interrupts").unwrap_or_default(),
        ports,
        first_bus: node
            .property("bus-range")
            .and_then(|range| range.first().copied())
            .unwrap_or(0),
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
    }
}
