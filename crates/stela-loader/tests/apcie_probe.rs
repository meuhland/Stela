//! The PCIe root-complex probe's DeviceTree admission (`--probe-apcie`).
//!
//! The probe is a process-wide switch, so these checks run in their own test
//! binary, one at a time ([`LOCK`]), rather than beside the unit tests that
//! admit the same trees without it.
use std::sync::Mutex;
use stela_contracts::adt::encode;
use stela_loader::apple_device_tree::{DeviceTree, Node};
use stela_loader::devices::hide_unmodeled;
use stela_soc_data::board::{self, apcie_probe};

static LOCK: Mutex<()> = Mutex::new(());

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
    let _one = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
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
    for path in ["/arm-io/apcie", "/arm-io/gpio"] {
        assert!(!hidden_on.iter().any(|entry| entry == path), "{path}");
        assert!(on.property(path, "compatible").is_some(), "{path}");
    }
    // This DART has no register window or instance, so the board model
    // would refuse it: the probe retreats from it and its mapper.
    for path in ["/arm-io/dart-apcie0", "/arm-io/dart-apcie0/mapper"] {
        assert!(hidden_on.iter().any(|entry| entry == path), "{path}");
        assert!(on.property(path, "compatible").is_none(), "{path}");
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

fn z(text: &str) -> Vec<u8> {
    [text.as_bytes(), &[0]].concat()
}

fn pairs(values: &[u64]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// A J473AP-shaped board (`board::fixture_support`) with a root complex of
/// two ports, the first port's DART with one mapper of the board model's shape
/// and one of another (run 1's `dart-apcie1` refusal), and a second DART
/// the board model refuses outright (no instance descriptors), which the
/// second port names.
fn board_tree() -> Vec<u8> {
    let windows: Vec<u64> = [
        (0x7000_0000, 0x100_0000), // config
        (0x6800_0000, 0x4000),     // rc
        (0x6800_4000, 0x4000),     // phy
        (0x6800_8000, 0x4000),     // phy-ip
        (0x6800_c000, 0x4000),     // axi2af
        (0x6801_0000, 0x4000),     // fuse
        (0x6802_0000, 0x8000),     // port0
        (0x6802_8000, 0x4000),     // ltssm0
        (0x6802_c000, 0x4000),     // phy-glue0
        (0x6803_0000, 0x4000),     // phy-ip-lane0
        (0x6803_4000, 0x4000),     // intr2axi0
    ]
    .into_iter()
    .flat_map(|(at, bytes)| [at, bytes])
    .collect();
    let port = encode(
        &[
            ("name", &z("pci-bridge0")),
            ("apcie-port", &cells(&[0])),
            ("reg", &cells(&[0, 0, 0, 0, 0])),
            ("function-dart_force_active", &cells(&[DART, 0x6466_6163])),
            ("function-dart_self", &cells(&[DART, 0x6473_6c66])),
        ],
        &[],
    );
    // A second port names the second DART the same way.
    let second = encode(
        &[
            ("name", &z("pci-bridge1")),
            ("apcie-port", &cells(&[1])),
            ("reg", &cells(&[0x800, 0, 0, 0, 0])),
            ("function-dart_self", &cells(&[OTHER_DART, 0x6473_6c66])),
        ],
        &[],
    );
    let apcie = encode(
        &[
            ("name", &z("apcie")),
            ("compatible", &z("apcie,t8112")),
            ("AAPL,phandle", &cells(&[APCIE])),
            ("reg", &pairs(&windows)),
            ("interrupts", &cells(&[0x300])),
            ("bus-range", &cells(&[0, 0xff])),
            ("#ports", &cells(&[1])),
            (
                "ranges",
                &cells(&[0x4200_0000, 0, 0x8000_0000, 0, 0x8000_0000, 0, 0x4000_0000]),
            ),
        ],
        &[port, second],
    );
    let mapper = |name: &str, phandle: u32, reg: &[u32]| {
        encode(
            &[
                ("name", &z(name)),
                ("compatible", &z("iommu-mapper")),
                ("AAPL,phandle", &cells(&[phandle])),
                ("reg", &cells(reg)),
            ],
            &[],
        )
    };
    let dart = encode(
        &[
            ("name", &z("dart-apcie0")),
            ("compatible", &z("dart,t8110")),
            ("AAPL,phandle", &cells(&[DART])),
            ("reg", &pairs(&[0x6900_0000, 0x4000])),
            ("instance", b"TRAD\0\0\0\0DART\0\0\0\0"),
            ("interrupts", &cells(&[0x301])),
        ],
        &[
            mapper("mapper-apcie0", MAPPER, &[0]),
            mapper("mapper-apcie0-range", 0x52, &[0, 8]),
        ],
    );
    let broken = encode(
        &[
            ("name", &z("dart-apcie1")),
            ("compatible", &z("dart,t8110")),
            ("AAPL,phandle", &cells(&[OTHER_DART])),
            ("reg", &pairs(&[0x6901_0000, 0x4000])),
            ("interrupts", &cells(&[0x302])),
        ],
        &[mapper("mapper-apcie1", OTHER_MAPPER, &[0])],
    );
    board::fixture_support::tree_with_io(
        0x8112,
        "Apple M2",
        [board::UART, board::WATCHDOG, "aic,2"],
        &[],
        &[apcie, dart, broken],
    )
}

#[test]
fn the_probes_prepared_tree_passes_the_board_model() {
    let _one = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let bytes = board_tree();
    // Without the probe nothing of it is shown, and the board is modeled.
    let mut off = DeviceTree::parse(&bytes).unwrap();
    hide_unmodeled(&mut off);
    let modeled = board::board(&off.serialize()).unwrap();
    assert!(modeled.apcie.is_empty());
    assert!(modeled.darts.is_empty());

    apcie_probe::set_apcie(true);
    let mut on = DeviceTree::parse(&bytes).unwrap();
    let hidden = hide_unmodeled(&mut on);
    let found = board::board(&on.serialize());
    apcie_probe::set_apcie(false);
    let found = found.unwrap();
    // The odd mapper and the refused DART are hidden; the rest is shown.
    for path in [
        "/arm-io/dart-apcie0/mapper-apcie0-range",
        "/arm-io/dart-apcie1",
        "/arm-io/dart-apcie1/mapper-apcie1",
    ] {
        assert!(hidden.iter().any(|entry| entry == path), "{path}");
    }
    for path in [
        "/arm-io/apcie",
        "/arm-io/dart-apcie0",
        "/arm-io/dart-apcie0/mapper-apcie0",
    ] {
        assert!(!hidden.iter().any(|entry| entry == path), "{path}");
    }
    assert_eq!(found.darts.len(), 1);
    assert_eq!(found.darts[0].mappers, [(MAPPER, 0)]);
    assert_eq!(found.apcie.len(), 1);
    let apcie = &found.apcie[0];
    assert_eq!(apcie.windows.len(), 11);
    assert_eq!(apcie.windows[1].physical, Some(0x2_6800_0000));
    assert!(apcie.windows.iter().all(|window| window.physical.is_some()));
    assert_eq!(apcie.ports.len(), 2);
    assert_eq!(apcie.interrupts, [0x300]);
}
