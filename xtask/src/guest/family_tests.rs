//! StelaVirtIO's hand-written IONetworkingFamily declarations
//! (`guest/stela-virtio/family/networking`) against the slot manifest beside
//! them, which the reference boot collection gave: every vtable slot of
//! every class, by the symbol it holds, and every class's size and parent.
//!
//! `tests/probe.cpp` derives a class that overrides nothing from each
//! declared class, so the vtable clang emits for it names, slot by slot, what
//! the header puts there; the kext's own StelaVirtIONet must match its
//! parent's slots too, its overrides by method name and parameters. Both are
//! compiled with the kext's flags and read back from the objects'
//! relocations.
use super::{FAMILY, compile_kext_source, sdk};
use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const COMPONENT: &str = "guest/stela-virtio";
const NETWORKING: &str = "networking";
const MANIFEST: &str = "networking.slots";

/// One class of a slot manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Class {
    name: String,
    superclass: String,
    size: u64,
    /// Each slot's names; empty where no export names it (`?`). A pure
    /// virtual slot's are `___cxa_pure_virtual` and the method declared
    /// there (`= name`).
    slots: Vec<Vec<String>>,
}

/// Reads a manifest's text: `class <name> <superclass> <size> <count>`, then
/// `<index> <offset> <names|?>[ = <declared>]` per slot; `#` lines are
/// comments.
fn parse_manifest(text: &str) -> Vec<Class> {
    let mut classes: Vec<Class> = Vec::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        let fields: Vec<&str> = line.split(' ').collect();
        if let ["class", name, superclass, size, count] = fields[..] {
            classes.push(Class {
                name: name.into(),
                superclass: superclass.into(),
                size: u64::from_str_radix(size.trim_start_matches("0x"), 16).unwrap(),
                slots: Vec::with_capacity(count.parse().unwrap()),
            });
            continue;
        }
        let class = classes.last_mut().expect("a slot after its class");
        let (index, offset, named, declared) = match fields[..] {
            [index, offset, named] => (index, offset, named, None),
            [index, offset, named, "=", declared] => (index, offset, named, Some(declared)),
            _ => panic!("a manifest line {line:?}"),
        };
        let index: usize = index.parse().unwrap();
        assert_eq!(index, class.slots.len(), "{line}");
        assert_eq!(offset, format!("{:#x}", 16 + 8 * index), "{line}");
        let mut names: Vec<String> = if named == "?" {
            Vec::new()
        } else {
            named.split('|').map(String::from).collect()
        };
        names.extend(declared.map(String::from));
        class.slots.push(names);
    }
    for class in &classes {
        assert!(!class.slots.is_empty(), "{} has no slots", class.name);
    }
    classes
}

fn networking() -> PathBuf {
    crate::tools::root()
        .join(COMPONENT)
        .join(FAMILY)
        .join(NETWORKING)
}

fn manifest() -> Vec<Class> {
    parse_manifest(&fs::read_to_string(networking().join(MANIFEST)).unwrap())
}

/// `__ZN<n><class>...` or `__ZNK<n><class>...`: the class, and the rest (a
/// `K` kept for a const method).
fn split(symbol: &str) -> Option<(&str, String)> {
    let (constant, inner) = match symbol.strip_prefix("__ZNK") {
        Some(inner) => (true, inner),
        None => (false, symbol.strip_prefix("__ZN")?),
    };
    let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
    let length: usize = inner[..digits].parse().ok()?;
    let class = inner.get(digits..digits + length)?;
    let rest = &inner[digits + length..];
    Some((class, format!("{}{rest}", if constant { "K" } else { "" })))
}

/// Where `slots`, the vtable of a class derived from `class`, differs from
/// the manifest's: a slot must hold one of the manifest's names, or a method
/// of `own` (the derived class) with the same name and parameters as one of
/// them, which is an override. A slot xnu's open headers still reserve
/// (`_RESERVED<Class><n>`) where the kernel's same class has since put a
/// method is that method, inherited: the linker binds inherited slots by
/// offset to the kernel's. Slots no export names are not checked.
fn mismatches(class: &Class, own: &str, slots: &[Option<String>]) -> Vec<String> {
    let mut wrong = Vec::new();
    for (index, expected) in class.slots.iter().enumerate() {
        if expected.is_empty() {
            continue;
        }
        let got = slots.get(index).cloned().flatten();
        let fits = got.as_deref().is_some_and(|got| {
            expected.iter().any(|name| name == got)
                || split(got).is_some_and(|(owner, method)| {
                    let reserved = method
                        .starts_with(&format!("{}_RESERVED{owner}", 10 + owner.len()))
                        || method.starts_with(&format!("{}_RESERVED{owner}", 11 + owner.len()));
                    expected
                        .iter()
                        .filter_map(|name| split(name))
                        .any(|(class, expected)| {
                            owner == own && expected == method
                                || reserved && class == owner && !expected.contains("_RESERVED")
                        })
                })
        });
        if !fits {
            wrong.push(format!(
                "{} slot {index}: {} where the boot collection has {}",
                class.name,
                got.as_deref().unwrap_or("nothing"),
                expected.join("|")
            ));
        }
    }
    wrong
}

/// Each vtable `object` defines, by symbol: the symbol each of its first
/// `slots` slots points at, as its relocation names it.
fn vtables(
    object: &[u8],
    wanted: &BTreeMap<String, usize>,
) -> BTreeMap<String, Vec<Option<String>>> {
    let file = object::File::parse(object).unwrap();
    let mut by_address: BTreeMap<u64, String> = BTreeMap::new();
    for symbol in file.symbols() {
        if let (Ok(name), Some(_)) = (symbol.name(), symbol.section_index()) {
            by_address
                .entry(symbol.address())
                .and_modify(|kept| {
                    if !kept.starts_with("__Z") && name.starts_with("__Z") {
                        *kept = name.to_string();
                    }
                })
                .or_insert_with(|| name.to_string());
        }
    }
    let mut tables = BTreeMap::new();
    for symbol in file.symbols() {
        let Ok(name) = symbol.name() else {
            continue;
        };
        let Some(&count) = wanted.get(name) else {
            continue;
        };
        let section = file
            .section_by_index(symbol.section_index().expect("a defined vtable"))
            .unwrap();
        let data = section.data().unwrap();
        let mut targets: BTreeMap<u64, String> = BTreeMap::new();
        for (offset, relocation) in section.relocations() {
            let at = section.address() + offset;
            let target = match relocation.target() {
                RelocationTarget::Symbol(index) => file
                    .symbol_by_index(index)
                    .ok()
                    .and_then(|symbol| symbol.name().ok().map(String::from)),
                RelocationTarget::Section(_) => {
                    // The slot holds the target's address: in its low 32
                    // bits when the pointer is authenticated.
                    let start = (offset) as usize;
                    let raw = u64::from_le_bytes(data[start..start + 8].try_into().unwrap());
                    let address = if raw >> 63 == 1 {
                        raw & 0xffff_ffff
                    } else {
                        raw
                    };
                    by_address.get(&address).cloned()
                }
                _ => None,
            };
            if let Some(target) = target {
                targets.insert(at, target);
            }
        }
        let slots = (0..count)
            .map(|index| {
                targets
                    .get(&(symbol.address() + 16 + 8 * index as u64))
                    .cloned()
            })
            .collect();
        tables.insert(name.to_string(), slots);
    }
    tables
}

/// Compiles `source` of the StelaVirtIO kext with its flags into `built`.
fn compile(built: &Path, source: &Path) -> Vec<u8> {
    let root = crate::tools::root();
    let xnu = root.join("third_party/xnu");
    let include = built.join("include");
    if !include.exists() {
        sdk::generate(&xnu, &include).unwrap();
    }
    let object = built.join(source.file_stem().unwrap()).with_extension("o");
    compile_kext_source(
        &root.join(COMPONENT),
        ("com.stela.StelaVirtIO", "1.0.0"),
        &xnu,
        &include,
        source,
        &object,
    )
    .unwrap();
    fs::read(object).unwrap()
}

fn vtable_symbol(class: &str) -> String {
    format!("__ZTV{}{class}", class.len())
}

#[test]
fn every_networking_header_slot_holds_what_the_boot_collection_does() {
    let manifest = manifest();
    let built = tempfile::tempdir().unwrap();
    let object = compile(built.path(), &networking().join("tests/probe.cpp"));
    let wanted = manifest
        .iter()
        .map(|class| {
            (
                vtable_symbol(&format!("StelaProbe{}", class.name)),
                class.slots.len(),
            )
        })
        .collect();
    let tables = vtables(&object, &wanted);
    let mut wrong = Vec::new();
    for class in &manifest {
        let probe = format!("StelaProbe{}", class.name);
        let slots = tables
            .get(&vtable_symbol(&probe))
            .unwrap_or_else(|| panic!("no vtable for {probe}"));
        wrong.extend(mismatches(class, &probe, slots));
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn stela_virtio_net_fills_its_parents_slots_with_its_overrides() {
    let manifest = manifest();
    let ethernet = manifest
        .iter()
        .find(|class| class.name == "IOEthernetController")
        .unwrap();
    let built = tempfile::tempdir().unwrap();
    let object = compile(
        built.path(),
        &crate::tools::root().join(COMPONENT).join("kext/Net.cpp"),
    );
    let symbol = vtable_symbol("StelaVirtIONet");
    let tables = vtables(&object, &[(symbol.clone(), ethernet.slots.len())].into());
    let slots = &tables[&symbol];
    let wrong = mismatches(ethernet, "StelaVirtIONet", slots);
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // Its overrides, by name, are where IOEthernetController has them.
    let overridden: Vec<String> = slots
        .iter()
        .flatten()
        .filter_map(|name| split(name))
        .filter(|(owner, _)| *owner == "StelaVirtIONet")
        .map(|(_, method)| method)
        .collect();
    for method in [
        "5startEP9IOService",
        "12outputPacketEP6__mbufPv",
        "18getHardwareAddressEP17IOEthernetAddress",
        "6enableEP18IONetworkInterface",
        "17createOutputQueueEv",
    ] {
        assert!(overridden.iter().any(|name| name == method), "{method}");
    }
}

#[test]
fn each_class_has_its_parent_and_size_in_the_headers() {
    let mut headers = String::new();
    for entry in fs::read_dir(networking()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "h") {
            headers.push_str(&fs::read_to_string(path).unwrap());
        }
    }
    for class in manifest() {
        let declaration = format!("class {} : public {} {{", class.name, class.superclass);
        assert!(headers.contains(&declaration), "{declaration}");
        let size = format!("static_assert(sizeof({}) == {:#x},", class.name, class.size);
        assert!(headers.contains(&size), "{size}");
    }
}

#[test]
fn a_method_in_the_wrong_slot_or_with_other_parameters_is_caught() {
    let class = Class {
        name: "IOThing".into(),
        superclass: "OSObject".into(),
        size: 0x20,
        slots: vec![
            vec!["__ZN7IOThingD1Ev".into()],
            vec![],
            vec!["__ZN7IOThing5startEP9IOService".into()],
            vec!["__ZNK7IOThing4sizeEv".into()],
            vec!["___cxa_pure_virtual".into()],
        ],
    };
    let named = |names: &[&str]| -> Vec<Option<String>> {
        names.iter().map(|name| Some(name.to_string())).collect()
    };
    let right = named(&[
        "__ZN5ProbeD1Ev",
        "__ZN8anything3eltEv",
        "__ZN5Probe5startEP9IOService",
        "__ZNK7IOThing4sizeEv",
        "___cxa_pure_virtual",
    ]);
    assert_eq!(mismatches(&class, "Probe", &right), Vec::<String>::new());
    // Two slots swapped.
    let mut swapped = right.clone();
    swapped.swap(2, 3);
    assert_eq!(mismatches(&class, "Probe", &swapped).len(), 2);
    // An override with other parameters, or of another class's method.
    let mut other = right.clone();
    other[2] = Some("__ZN5Probe5startEPv".into());
    assert_eq!(mismatches(&class, "Probe", &other).len(), 1);
    other[2] = Some("__ZN5Other5startEP9IOService".into());
    assert_eq!(mismatches(&class, "Probe", &other).len(), 1);
    // A const method is not its non-const namesake.
    let mut constant = right.clone();
    constant[3] = Some("__ZN5Probe4sizeEv".into());
    assert_eq!(mismatches(&class, "Probe", &constant).len(), 1);
    // A missing slot.
    assert_eq!(mismatches(&class, "Probe", &right[..4]).len(), 1);
    // A slot xnu's headers still reserve where the kernel's class has a
    // method passes; another class's reserved slot does not.
    let mut reserved = right.clone();
    reserved[3] = Some("__ZN7IOThing17_RESERVEDIOThing0Ev".into());
    assert_eq!(mismatches(&class, "Probe", &reserved), Vec::<String>::new());
    reserved[3] = Some("__ZN7IOOther17_RESERVEDIOOther0Ev".into());
    assert_eq!(mismatches(&class, "Probe", &reserved).len(), 1);
    // An override of a pure virtual slot, by the method declared there.
    let mut pure = class.clone();
    pure.slots[4].push("__ZN7IOThing4pureEv".into());
    let mut overridden = right.clone();
    overridden[4] = Some("__ZN5Probe4pureEv".into());
    assert_eq!(
        mismatches(&pure, "Probe", &overridden),
        Vec::<String>::new()
    );
    assert_eq!(mismatches(&class, "Probe", &overridden).len(), 1);
    assert_eq!(
        split("__ZNK15IONetworkMedium7getTypeEv"),
        Some(("IONetworkMedium", "K7getTypeEv".into()))
    );
    assert_eq!(split("___cxa_pure_virtual"), None);
}

#[test]
fn the_manifest_reads_as_it_is_written() {
    let text = "# a comment\nclass A B 0x88 3\n0 0x10 __ZN1AD1Ev\n1 0x18 ?\n\
                2 0x20 ___cxa_pure_virtual = __ZN1A1fEv\n";
    assert_eq!(
        parse_manifest(text),
        [Class {
            name: "A".into(),
            superclass: "B".into(),
            size: 0x88,
            slots: vec![
                vec!["__ZN1AD1Ev".into()],
                vec![],
                vec!["___cxa_pure_virtual".into(), "__ZN1A1fEv".into()]
            ],
        }]
    );
    let manifest = manifest();
    assert_eq!(manifest.len(), 10);
    let ethernet = manifest
        .iter()
        .find(|class| class.name == "IOEthernetController")
        .unwrap();
    assert_eq!((ethernet.size, ethernet.slots.len()), (0x118, 400));
    assert_eq!(
        ethernet.slots[362],
        [
            "___cxa_pure_virtual",
            "__ZN20IOEthernetController18getHardwareAddressEP17IOEthernetAddress"
        ]
    );
}
