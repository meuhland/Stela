//! The PCIe root-complex probe's DeviceTree admission (`--probe-apcie`).
//!
//! The probe is a process-wide switch, so these checks run in their own test
//! binary, one test that sets and clears it, rather than beside the unit
//! tests that admit the same tree without it.
use stela_loader::apple_device_tree::{DeviceTree, Node};
use stela_loader::devices::hide_unmodeled;
use stela_soc_data::board::apcie_probe;

fn node(name: &str, properties: &[(&str, Vec<u8>)], children: Vec<Node>) -> Node {
    let mut node = Node::named(name).unwrap();
    for (key, value) in properties {
        node.set_property(key, value.clone()).unwrap();
    }
    node.children = children;
    node
}

fn cells(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

const APCIE: u32 = 0x35;
const GPIO: u32 = 0x40;
const DART: u32 = 0x50;
const MAPPER: u32 = 0x51;
const OTHER_DART: u32 = 0x60;
const OTHER_MAPPER: u32 = 0x61;

/// A J473AP-shaped root complex: one port whose PERST# is an AP GPIO and
/// whose DART functions name its DART, an AMFM that would power a port up
/// through the complex, and an unrelated DART nothing uses.
fn tree() -> DeviceTree {
    let port = node(
        "pci-bridge0",
        &[
            ("apcie-port", cells(&[0])),
            ("function-perst", cells(&[GPIO, 0x4750_494f, 166, 0])),
            ("function-dart_force_active", cells(&[DART, 0x6466_6163])),
            ("function-dart_self", cells(&[DART, 0x6473_6c66])),
        ],
        vec![node(
            "wlan",
            &[("compatible", b"wlan-pcie,bcm4388\0".to_vec())],
            vec![],
        )],
    );
    let apcie = node(
        "apcie",
        &[
            ("compatible", b"apcie,t8112\0".to_vec()),
            ("AAPL,phandle", cells(&[APCIE])),
        ],
        vec![port],
    );
    let dart = |name: &str, phandle: u32, mapper: u32| {
        node(
            name,
            &[
                ("compatible", b"dart,t8110\0".to_vec()),
                ("AAPL,phandle", cells(&[phandle])),
            ],
            vec![node(
                "mapper",
                &[
                    ("compatible", b"iommu-mapper\0".to_vec()),
                    ("AAPL,phandle", cells(&[mapper])),
                    ("reg", cells(&[0])),
                ],
                vec![],
            )],
        )
    };
    let gpio = node(
        "gpio",
        &[
            ("compatible", b"gpio,t8101\0".to_vec()),
            ("AAPL,phandle", cells(&[GPIO])),
        ],
        vec![],
    );
    let amfm = node(
        "amfm",
        &[
            ("device_type", b"amfm\0".to_vec()),
            ("AAPL,phandle", cells(&[0x24])),
            (
                "function-pcie_port_control",
                cells(&[APCIE, 0x5072_7443, 0x36]),
            ),
        ],
        vec![],
    );
    DeviceTree {
        root: node(
            "",
            &[],
            vec![node(
                "arm-io",
                &[],
                vec![
                    apcie,
                    dart("dart-apcie0", DART, MAPPER),
                    dart("dart-other", OTHER_DART, OTHER_MAPPER),
                    gpio,
                    amfm,
                ],
            )],
        ),
    }
}

#[test]
fn the_probe_admits_the_root_complex_and_its_providers_only_when_on() {
    assert!(!apcie_probe::apcie(), "the probe is off by default");
    let mut off = tree();
    let hidden_off = hide_unmodeled(&mut off);
    for path in [
        "/arm-io/apcie",
        "/arm-io/dart-apcie0",
        "/arm-io/dart-apcie0/mapper",
        "/arm-io/gpio",
    ] {
        assert!(hidden_off.iter().any(|entry| entry == path), "{path}");
        assert!(off.property(path, "compatible").is_none(), "{path}");
    }

    apcie_probe::set_apcie(true);
    let mut on = tree();
    let hidden_on = hide_unmodeled(&mut on);
    apcie_probe::set_apcie(false);
    for path in [
        "/arm-io/apcie",
        "/arm-io/dart-apcie0",
        "/arm-io/dart-apcie0/mapper",
        "/arm-io/gpio",
    ] {
        assert!(!hidden_on.iter().any(|entry| entry == path), "{path}");
        assert!(on.property(path, "compatible").is_some(), "{path}");
    }
    // No endpoint is modeled, nothing else is admitted for the probe, and
    // the AMFM still finds no provider in a complex with nothing behind it.
    for path in [
        "/arm-io/apcie/pci-bridge0/wlan",
        "/arm-io/dart-other",
        "/arm-io/dart-other/mapper",
        "/arm-io/amfm",
    ] {
        assert!(hidden_on.iter().any(|entry| entry == path), "{path}");
    }
    assert!(on.property("/arm-io/amfm", "AAPL,ignore").is_some());

    // Off again, the same input prepares to the same bytes as before.
    let mut again = tree();
    assert_eq!(hide_unmodeled(&mut again), hidden_off);
    assert_eq!(again.serialize(), off.serialize());
}
