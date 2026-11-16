//! The kernel family classes the StelaVirtIO kext derives from or calls,
//! whose headers no open-source tree has: Stela declares them by hand in
//! `guest/stela-virtio/family/<Family>.h`, and keeps beside each header a
//! slot manifest, `<Family>.slots`, that `cargo xtask guest-family --kc
//! <kernelcache>` writes from a boot kernel collection
//! (`stela_loader::aux_kc::manifest`): every vtable slot's method and each
//! class's instance size, from its `MetaClass` constructor.
//!
//! The tests hold the header to the manifest. A probe class deriving from
//! each family class (`family/tests/slot_probe.cpp`) is compiled as the
//! kext is, so its vtable, slot by slot, is what the header declares; each
//! slot must name the manifest's method, or one with the same name, `const`
//! and parameters ([`stela_loader::aux_kc::itanium`]). The header's
//! `static_assert`s on each class's size must give the manifest's sizes.
//!
//! A pure virtual slot has no name in the collection. Where another class
//! names the same slots, the manifest records the method expected there
//! ([`Pure`]), from that evidence; where none does, the family names it
//! ([`Declared`]) and says why.
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use stela_loader::aux_kc::manifest::{self, Manifest, ManifestClass};

/// A family's classes, their header and manifest.
pub struct Family {
    /// The header in `guest/stela-virtio/family`, and its manifest there.
    pub header: &'static str,
    pub manifest: &'static str,
    /// The fileset entry that defines the classes, which the kext lists.
    pub entry: &'static str,
    pub classes: &'static [&'static str],
    pub pure: &'static [Pure],
    pub declared: &'static [Declared],
    /// The probe source in `family/tests`, and each probe class in it with
    /// the family class it derives from, overriding no slot the kernel
    /// names. Only the tests compile them.
    #[cfg_attr(not(test), allow(dead_code))]
    pub probe: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub probes: &'static [(&'static str, &'static str)],
}

/// A pure virtual slot of `class` no class of the collection names, or one
/// no export names: the method declared there, `method` after the class in
/// its mangling (`K` first for a const one), for the reason the manifest's
/// comment gives (`why`).
pub struct Declared {
    pub class: &'static str,
    pub slot: usize,
    pub method: &'static str,
    pub why: &'static str,
}

/// Pure virtual slots of `class` whose methods `from` names: `class`'s slot
/// `first + n` declares `from`'s method of that slot, with `trailing` added
/// to its parameters.
pub struct Pure {
    pub class: &'static str,
    pub from: &'static str,
    pub first: usize,
    pub count: usize,
    pub trailing: &'static str,
}

/// IOSerialFamily (K2's console): the driver StelaVirtIOConsole derives
/// from, and the stream nub it publishes. IOSerialStreamSync forwards each
/// of its twelve stream methods to the driver's pure virtual of the same
/// slot, adding the `void *refCon` the stream was made with.
///
/// IONetworkingFamily (K3's network device): the Ethernet controller
/// StelaVirtIONet derives from, the interface it attaches, the output
/// queues, the medium and the statistics buffer. Each pure virtual slot is
/// the method the subclass of the family fills it with, but one no class
/// fills: IOEthernetController's own `getHardwareAddress(IOEthernetAddress *)`.
/// One slot no export names, IONetworkMedium's 45, is declared from memory.
pub const FAMILIES: &[Family] = &[
    Family {
        header: "IOSerialFamily.h",
        manifest: "IOSerialFamily.slots",
        entry: "com.apple.iokit.IOSerialFamily",
        classes: &[
            "IOSerialDriverSync",
            "IOSerialStreamSync",
            "IORS232SerialStreamSync",
        ],
        pure: &[Pure {
            class: "IOSerialDriverSync",
            from: "IOSerialStreamSync",
            first: 272,
            count: 12,
            trailing: "Pv",
        }],
        declared: &[],
        probe: "slot_probe.cpp",
        probes: &[
            ("StelaSlotProbeDriver", "IOSerialDriverSync"),
            ("StelaSlotProbeRS232", "IORS232SerialStreamSync"),
        ],
    },
    Family {
        header: "IONetworkingFamily.h",
        manifest: "IONetworkingFamily.slots",
        entry: "com.apple.iokit.IONetworkingFamily",
        classes: &NETWORKING,
        pure: &[
            Pure {
                class: "IONetworkController",
                from: "IOEthernetController",
                first: 285,
                count: 2,
                trailing: "",
            },
            Pure {
                class: "IONetworkController",
                from: "IOEthernetController",
                first: 290,
                count: 2,
                trailing: "",
            },
            Pure {
                class: "IONetworkController",
                from: "IOEthernetController",
                first: 294,
                count: 3,
                trailing: "",
            },
            Pure {
                class: "IONetworkController",
                from: "IOEthernetController",
                first: 320,
                count: 1,
                trailing: "",
            },
            Pure {
                class: "IONetworkInterface",
                from: "IOEthernetInterface",
                first: 282,
                count: 1,
                trailing: "",
            },
            Pure {
                class: "IOOutputQueue",
                from: "IOBasicOutputQueue",
                first: 42,
                count: 8,
                trailing: "",
            },
        ],
        declared: &[
            Declared {
                class: "IOEthernetController",
                slot: 362,
                method: "18getHardwareAddressEP17IOEthernetAddress",
                why: "the only pure virtual between getPacketFilters(UInt32 *) and \
                      setHardwareAddress(const IOEthernetAddress *), which drivers implement",
            },
            Declared {
                class: "IONetworkMedium",
                slot: 45,
                method: "K6getKeyEv",
                why: "no export names it; between getName() and isEqualTo(const \
                      IONetworkMedium *) the open-source header has getKey() const [mem], \
                      which StelaVirtIO never calls",
            },
        ],
        probe: "networking_probe.cpp",
        probes: &[
            ("StelaProbeIONetworkController", "IONetworkController"),
            ("StelaProbeIOEthernetController", "IOEthernetController"),
            ("StelaProbeIONetworkInterface", "IONetworkInterface"),
            ("StelaProbeIOEthernetInterface", "IOEthernetInterface"),
            ("StelaProbeIOOutputQueue", "IOOutputQueue"),
            ("StelaProbeIOBasicOutputQueue", "IOBasicOutputQueue"),
            ("StelaProbeIOGatedOutputQueue", "IOGatedOutputQueue"),
            ("StelaProbeIOPacketQueue", "IOPacketQueue"),
            ("StelaProbeIONetworkMedium", "IONetworkMedium"),
            ("StelaProbeIONetworkData", "IONetworkData"),
        ],
    },
];

/// IONetworkingFamily's classes StelaVirtIO declares.
const NETWORKING: [&str; 10] = [
    "IONetworkController",
    "IOEthernetController",
    "IONetworkInterface",
    "IOEthernetInterface",
    "IOOutputQueue",
    "IOBasicOutputQueue",
    "IOGatedOutputQueue",
    "IOPacketQueue",
    "IONetworkMedium",
    "IONetworkData",
];

/// Where the families' headers and manifests are.
fn directory() -> PathBuf {
    crate::tools::root().join("guest/stela-virtio/family")
}

/// `cargo xtask guest-family --kc <kernelcache>`: writes each family's
/// manifest from the boot kernel collection `kc`.
pub fn generate(kc: &Path) -> Result<()> {
    let boot = fs::read(kc).with_context(|| format!("reading {}", kc.display()))?;
    for family in FAMILIES {
        let manifest = manifest(&boot, family)?;
        let path = directory().join(family.manifest);
        fs::write(&path, manifest.to_text())?;
        println!("{}: {} classes", path.display(), manifest.classes.len());
    }
    Ok(())
}

/// `family`'s manifest from the boot collection `boot`.
pub fn manifest(boot: &[u8], family: &Family) -> Result<Manifest> {
    let collection = stela_loader::kernel_collection::KernelCollection::parse(boot)?;
    let uuid = collection.uuid.unwrap_or_else(|| "with no UUID".into());
    let mut classes = Vec::new();
    for class in family.classes {
        let layout = manifest::class_layout(boot, class, &[family.entry])
            .with_context(|| format!("{class} in the boot collection"))?;
        classes.push(ManifestClass {
            layout,
            expected: BTreeMap::new(),
        });
    }
    for pure in family.pure {
        let from = classes
            .iter()
            .find(|class| class.layout.class == pure.from)
            .with_context(|| format!("{} is not in the family", pure.from))?
            .layout
            .slots
            .clone();
        let class = classes
            .iter_mut()
            .find(|class| class.layout.class == pure.class)
            .with_context(|| format!("{} is not in the family", pure.class))?;
        for slot in pure.first..pure.first + pure.count {
            ensure!(
                class.layout.slots.get(slot).map(Vec::as_slice)
                    == Some(&["___cxa_pure_virtual".to_owned()][..]),
                "{} slot {slot} is not pure virtual",
                pure.class
            );
            let [named] = from.get(slot).map(Vec::as_slice).unwrap_or_default() else {
                anyhow::bail!("{} slot {slot} has no one name", pure.from);
            };
            // `__ZN` for a method, `__ZNK` for a const one.
            let (nested, method) = ["__ZNK", "__ZN"]
                .into_iter()
                .find_map(|nested| {
                    named
                        .strip_prefix(&format!("{nested}{}{}", pure.from.len(), pure.from))
                        .map(|method| (nested, method))
                })
                .with_context(|| format!("{} slot {slot} is {named}", pure.from))?;
            let method = with_parameter(method, pure.trailing)
                .with_context(|| format!("{} slot {slot}'s {named}", pure.from))?;
            class.expected.insert(
                slot,
                format!("{nested}{}{}{method}", pure.class.len(), pure.class),
            );
        }
    }
    for declared in family.declared {
        let class = classes
            .iter_mut()
            .find(|class| class.layout.class == declared.class)
            .with_context(|| format!("{} is not in the family", declared.class))?;
        let slot = class.layout.slots.get(declared.slot).map(Vec::as_slice);
        ensure!(
            slot == Some(&["___cxa_pure_virtual".to_owned()][..]) || slot == Some(&[][..]),
            "{} slot {} is neither pure virtual nor unnamed",
            declared.class,
            declared.slot
        );
        let (constant, method) = match declared.method.strip_prefix('K') {
            Some(method) => ("K", method),
            None => ("", declared.method),
        };
        class.expected.insert(
            declared.slot,
            format!(
                "__ZN{constant}{}{}{method}",
                declared.class.len(),
                declared.class,
            ),
        );
    }
    let mut source = vec![
        format!(
            "The vtable slots and instance sizes of the classes {} declares,",
            family.header
        ),
        format!("from the boot kernel collection {uuid} (`cargo xtask guest-family --kc <file>`)."),
        "Each class: name, superclass, size from its MetaClass constructor, slot count;".into(),
        "each slot: index, vtable offset, the symbols exported at its method (? for none).".into(),
    ];
    for pure in family.pure {
        let trailing = if pure.trailing.is_empty() {
            String::new()
        } else {
            format!(", plus `{}`", pure.trailing)
        };
        source.push(format!(
            "`= name`: {}'s pure virtual slots {}-{} declare {}'s methods there{trailing}.",
            pure.class,
            pure.first,
            pure.first + pure.count - 1,
            pure.from,
        ));
    }
    for declared in family.declared {
        source.push(format!(
            "`= name`: {}'s slot {}: {}.",
            declared.class, declared.slot, declared.why
        ));
    }
    Ok(Manifest { source, classes })
}

/// A nested method's mangling after its class, `<name>E<parameters>`, with
/// `parameter` added last: `11releasePortEv` and `Pv` give
/// `11releasePortEPv`.
fn with_parameter(method: &str, parameter: &str) -> Option<String> {
    if parameter.is_empty() {
        return Some(method.to_owned());
    }
    let digits = method.bytes().take_while(u8::is_ascii_digit).count();
    let length: usize = method[..digits].parse().ok()?;
    let name = method.get(..digits + length)?;
    let parameters = method.get(digits + length..)?.strip_prefix('E')?;
    let parameters = if parameters == "v" { "" } else { parameters };
    Some(format!("{name}E{parameters}{parameter}"))
}

/// The `__ZTV<class>` vtables `object` defines, each slot's symbol by
/// index from the first method (`None` for a slot with no relocation).
#[cfg(test)]
pub fn object_vtables(object: &[u8]) -> Result<BTreeMap<String, Vec<Option<String>>>> {
    use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
    let file = object::File::parse(object)?;
    let mut vtables = BTreeMap::new();
    for symbol in file.symbols() {
        let name = symbol.name()?;
        let Some(index) = symbol.section_index() else {
            continue;
        };
        if !name.starts_with("__ZTV") {
            continue;
        }
        let section = file.section_by_index(index)?;
        let start = symbol.address();
        let end = file
            .symbols()
            .filter(|other| other.section_index() == Some(index) && other.address() > start)
            .map(|other| other.address())
            .min()
            .unwrap_or(section.address() + section.size());
        let mut slots = vec![None; ((end - start).saturating_sub(16) / 8) as usize];
        for (offset, relocation) in section.relocations() {
            let at = section.address() + offset;
            if at < start + 16 || at >= end || (at - start) % 8 != 0 {
                continue;
            }
            let RelocationTarget::Symbol(target) = relocation.target() else {
                continue;
            };
            let target = file.symbol_by_index(target)?.name()?.to_owned();
            slots[((at - start - 16) / 8) as usize] = Some(target);
        }
        // Up to the next symbol may include alignment padding, which no
        // relocation fills.
        while slots.last() == Some(&None) {
            slots.pop();
        }
        vtables.insert(name.to_owned(), slots);
    }
    Ok(vtables)
}

/// Whether `symbol`, in a vtable built from the header, is the method the
/// manifest has in that slot: one of its names, or the same method by
/// name, `const` and parameters; for a pure slot, the method expected.
#[cfg(test)]
pub fn slot_agrees(symbol: &str, names: &[String], expected: Option<&str>) -> bool {
    use stela_loader::aux_kc::itanium::signature;
    if names.iter().any(|name| name == symbol) || expected == Some(symbol) {
        return true;
    }
    // A slot the header reserves and the kernel fills (xnu's open headers
    // reserve OSMetaClassBase's Dispatch slot): the kext calls nothing
    // there, and the link binds the slot to the kernel's method.
    if symbol.contains("_RESERVED") && !names.is_empty() && names != ["___cxa_pure_virtual"] {
        return true;
    }
    let pure = names == ["___cxa_pure_virtual"];
    let theirs: Vec<&str> = match (pure, expected) {
        (true, Some(expected)) => vec![expected],
        (true, None) => return false,
        (false, _) => names.iter().map(String::as_str).collect(),
    };
    let Some(own) = signature(symbol) else {
        return false;
    };
    theirs
        .iter()
        .any(|their| signature(their).as_ref() == Some(&own))
}

/// Compares the probe classes' vtables in `object` with `manifest`: the
/// probe `probe` derives from `class`, so has its slots and no more.
#[cfg(test)]
pub fn check_probe(
    vtables: &BTreeMap<String, Vec<Option<String>>>,
    manifest: &Manifest,
    probe: &str,
    class: &str,
) -> Result<()> {
    let vtable = vtables
        .get(&format!("__ZTV{}{probe}", probe.len()))
        .with_context(|| format!("the probe object has no {probe} vtable"))?;
    let expected = manifest
        .class(class)
        .with_context(|| format!("the manifest has no {class}"))?;
    let slots = &expected.layout.slots;
    ensure!(
        vtable.len() == slots.len(),
        "{probe}: the header gives {class} {} slots, the kernel {}",
        vtable.len(),
        slots.len()
    );
    let mut wrong = Vec::new();
    for (index, (symbol, names)) in vtable.iter().zip(slots).enumerate() {
        let symbol = symbol.as_deref().unwrap_or("(none)");
        if !slot_agrees(
            symbol,
            names,
            expected.expected.get(&index).map(String::as_str),
        ) {
            wrong.push(format!(
                "slot {index}: the header has {symbol}, the kernel {}",
                if names.is_empty() {
                    "an unexported method".into()
                } else {
                    names.join(" or ")
                }
            ));
        }
    }
    ensure!(wrong.is_empty(), "{class}:\n{}", wrong.join("\n"));
    Ok(())
}

/// Each `static_assert(sizeof(<class>) == <size>` of `header`, by class.
#[cfg(test)]
pub fn header_sizes(header: &str) -> BTreeMap<String, u64> {
    let mut sizes = BTreeMap::new();
    for line in header.lines() {
        let Some(rest) = line.trim().strip_prefix("static_assert(sizeof(") else {
            continue;
        };
        let Some((class, rest)) = rest.split_once(") == ") else {
            continue;
        };
        let size = rest
            .split([',', ')'])
            .next()
            .and_then(|size| size.strip_prefix("0x"))
            .and_then(|hex| u64::from_str_radix(hex, 16).ok());
        if let Some(size) = size {
            sizes.insert(class.to_owned(), size);
        }
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn committed(family: &Family) -> Manifest {
        let text = fs::read_to_string(directory().join(family.manifest)).unwrap();
        Manifest::parse(&text).unwrap()
    }

    /// Compiles `source` as the kext's sources are compiled, against the
    /// family headers in `headers`.
    fn compile(source: &Path, headers: &Path) -> Vec<u8> {
        let built = tempfile::tempdir().unwrap();
        let root = crate::tools::root();
        let xnu = root.join("third_party/xnu");
        let include = built.path().join("include");
        super::super::sdk::generate(&xnu, &include).unwrap();
        let clang = &crate::clang::toolset().unwrap().clangxx;
        let object = built.path().join("object.o");
        let mut command = Command::new(clang);
        command
            .args(super::super::KEXT_FLAGS)
            .arg(format!("--target={}", super::super::TARGET))
            .arg("-I")
            .arg(headers);
        for headers in super::super::XNU_HEADERS {
            command.arg("-isystem").arg(xnu.join(headers));
        }
        command.arg("-isystem").arg(&include);
        crate::tools::exec(command.arg("-c").arg(source).arg("-o").arg(&object)).unwrap();
        fs::read(object).unwrap()
    }

    fn probe_object(family: &Family, headers: &Path) -> Vec<u8> {
        compile(&directory().join("tests").join(family.probe), headers)
    }

    #[test]
    fn the_family_headers_put_every_method_in_the_kernels_slot() {
        for family in FAMILIES {
            let vtables = object_vtables(&probe_object(family, &directory())).unwrap();
            let manifest = committed(family);
            for (probe, class) in family.probes {
                check_probe(&vtables, &manifest, probe, class).unwrap();
            }
        }
    }

    /// The kext's own network driver: every slot of IOEthernetController,
    /// its overrides included, the pure getHardwareAddress among them.
    #[test]
    fn the_network_driver_overrides_the_kernels_slots() {
        let source = crate::tools::root().join("guest/stela-virtio/kext/Net.cpp");
        let vtables = object_vtables(&compile(&source, &directory())).unwrap();
        let manifest = committed(&FAMILIES[1]);
        check_probe(
            &vtables,
            &manifest,
            "StelaVirtIONet",
            "IOEthernetController",
        )
        .unwrap();
        let vtable = &vtables["__ZTV14StelaVirtIONet"];
        let own = |slot: usize| {
            vtable[slot]
                .as_deref()
                .is_some_and(|symbol| symbol.starts_with("__ZN14StelaVirtIONet"))
        };
        // start, outputPacket, enable(IONetworkInterface *),
        // createOutputQueue and the pure getHardwareAddress.
        for slot in [190, 273, 316, 322, 362] {
            assert!(own(slot), "slot {slot}: {:?}", vtable[slot]);
        }
    }

    /// The kext's own serial driver: every slot, its overrides of the pure
    /// virtuals included, is the method the manifest has there.
    #[test]
    fn the_console_driver_overrides_the_kernels_slots() {
        let source = crate::tools::root().join("guest/stela-virtio/kext/Console.cpp");
        let vtables = object_vtables(&compile(&source, &directory())).unwrap();
        let manifest = committed(&FAMILIES[0]);
        check_probe(
            &vtables,
            &manifest,
            "StelaVirtIOConsole",
            "IOSerialDriverSync",
        )
        .unwrap();
        // Its twelve stream methods are its own, in the pure slots.
        let vtable = &vtables["__ZTV18StelaVirtIOConsole"];
        let own = vtable[272..284]
            .iter()
            .filter(|symbol| {
                symbol
                    .as_deref()
                    .is_some_and(|symbol| symbol.starts_with("__ZN18StelaVirtIOConsole"))
            })
            .count();
        assert_eq!(own, 12);
    }

    /// Two of the driver's pure virtuals swapped: the probe's vtable shows
    /// it, slot by slot, where only the kernel's names could not.
    #[test]
    fn a_header_with_two_methods_swapped_is_caught() {
        let family = &FAMILIES[0];
        let header = fs::read_to_string(directory().join(family.header)).unwrap();
        let set = "\tvirtual IOReturn setState(UInt32 state, UInt32 mask, void *refCon) = 0;\n";
        let get = "\tvirtual UInt32 getState(void *refCon) = 0;\n";
        assert!(header.contains(&format!("{set}{get}")));
        let swapped = header.replace(&format!("{set}{get}"), &format!("{get}{set}"));
        let headers = tempfile::tempdir().unwrap();
        fs::write(headers.path().join(family.header), swapped).unwrap();
        let vtables = object_vtables(&probe_object(family, headers.path())).unwrap();
        let error = check_probe(
            &vtables,
            &committed(family),
            "StelaSlotProbeDriver",
            "IOSerialDriverSync",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("slot 274: the header has __ZN20StelaSlotProbeDriver8getStateEPv")
                && error
                    .contains("slot 275: the header has __ZN20StelaSlotProbeDriver8setStateEjjPv"),
            "{error}"
        );
    }

    #[test]
    fn the_family_headers_assert_the_kernels_sizes() {
        for family in FAMILIES {
            let manifest = committed(family);
            let header = fs::read_to_string(directory().join(family.header)).unwrap();
            let sizes = header_sizes(&header);
            for class in family.classes {
                let layout = &manifest.class(class).unwrap().layout;
                let declaration = format!("class {class} : public {} {{", layout.superclass);
                assert!(header.contains(&declaration), "{declaration}");
                assert_eq!(
                    sizes.get(*class),
                    Some(&layout.size),
                    "{}'s static_assert on {class}",
                    family.header
                );
            }
        }
    }

    #[test]
    fn a_slot_agrees_by_name_or_by_method() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|&name| name.to_owned())
                .collect::<Vec<_>>()
        };
        let start = names(&["__ZN9IOService5startEPS_"]);
        assert!(slot_agrees("__ZN9IOService5startEPS_", &start, None));
        assert!(slot_agrees("__ZN5Probe5startEP9IOService", &start, None));
        assert!(!slot_agrees("__ZN5Probe4stopEP9IOService", &start, None));
        let pure = names(&["___cxa_pure_virtual"]);
        let expected = "__ZN18IOSerialDriverSync11acquirePortEbPv";
        assert!(slot_agrees(
            "__ZN20StelaSlotProbeDriver11acquirePortEbPv",
            &pure,
            Some(expected)
        ));
        // Without the refCon, or with no evidence, a pure slot disagrees.
        assert!(!slot_agrees(
            "__ZN20StelaSlotProbeDriver11acquirePortEb",
            &pure,
            Some(expected)
        ));
        assert!(!slot_agrees("__ZN1A1fEv", &pure, None));
        assert!(!slot_agrees("__ZN1A1fEv", &[], None));
        // The header reserves a slot the kernel fills; not the reverse.
        let dispatch = names(&["__ZN15OSMetaClassBase8DispatchE5IORPC"]);
        assert!(slot_agrees(
            "__ZN15OSMetaClassBase25_RESERVEDOSMetaClassBase3Ev",
            &dispatch,
            None
        ));
        let reserved = names(&["__ZN15OSMetaClassBase25_RESERVEDOSMetaClassBase3Ev"]);
        assert!(!slot_agrees(
            "__ZN15OSMetaClassBase8DispatchE5IORPC",
            &reserved,
            None
        ));
        assert_eq!(
            with_parameter("11releasePortEv", "Pv").unwrap(),
            "11releasePortEPv"
        );
        assert_eq!(
            with_parameter("12dequeueEventEPjS0_b", "Pv").unwrap(),
            "12dequeueEventEPjS0_bPv"
        );
        assert_eq!(with_parameter("4initE", "Pv").unwrap(), "4initEPv");
        assert_eq!(with_parameter("x", "Pv"), None);
        assert_eq!(
            with_parameter("15createInterfaceEv", "").unwrap(),
            "15createInterfaceEv"
        );
        let header = "static_assert(sizeof(IOSerialDriverSync) == 0x88, \"x\");\n\
                      static_assert(sizeof(Other) == 136);\n";
        assert_eq!(
            header_sizes(header),
            [("IOSerialDriverSync".to_owned(), 0x88)].into()
        );
    }

    /// The committed manifests are what the reference kernelcache gives:
    /// `STELA_BOOT_KC=<kernelcache> cargo test -p xtask -- --ignored
    /// the_manifests_are`.
    #[test]
    #[ignore = "oracle: STELA_BOOT_KC"]
    fn the_manifests_are_the_reference_kernelcaches() {
        let Ok(kc) = std::env::var("STELA_BOOT_KC") else {
            eprintln!("STELA_BOOT_KC unset: nothing compared");
            return;
        };
        let boot = fs::read(kc).unwrap();
        for family in FAMILIES {
            let generated = manifest(&boot, family).unwrap().to_text();
            let committed = fs::read_to_string(directory().join(family.manifest)).unwrap();
            assert_eq!(generated, committed, "{}", family.manifest);
        }
    }
}
