//! IOStorageFamily, for K4's block device: `IOBlockStorageDevice`, which
//! StelaVirtIOBlock derives from, declared in
//! `guest/stela-virtio/family/storage/IOStorageFamily.h` and checked
//! against `storage/IOStorageFamily.slots`.
//!
//! Its fourteen pure virtual slots have no name in the kernel. Block storage
//! devices of other families name them: the manifest records, for each pure
//! slot, the method every class of [`EVIDENCE`] puts there, and generating
//! it refuses a kernel where they disagree or one leaves the slot pure.
use super::{Family, Manifest, directory};
use anyhow::{Context, Result, ensure};
use stela_loader::aux_kc::manifest;

/// IOStorageFamily's class StelaVirtIOBlock derives from. Its pure slots'
/// methods come from [`EVIDENCE`], not from one class's slots.
pub const FAMILY: Family = Family {
    header: "storage/IOStorageFamily.h",
    manifest: "storage/IOStorageFamily.slots",
    entry: "com.apple.iokit.IOStorageFamily",
    classes: &["IOBlockStorageDevice"],
    pure: &[],
};

/// Block storage devices of other families in the boot collection, each
/// with its fileset entry, that name IOBlockStorageDevice's pure slots.
pub const EVIDENCE: &[(&str, &str)] = &[
    (
        "IOBlockStorageServices",
        "com.apple.iokit.IOSCSIBlockCommandsDevice",
    ),
    (
        "IOAHCIBlockStorageDevice",
        "com.apple.iokit.IOAHCIBlockStorage",
    ),
    ("IONVMeBlockStorageDevice", "com.apple.iokit.IONVMeFamily"),
    ("AppleSDXCBlockStorageDevice", "com.apple.driver.AppleSDXC"),
];

/// The class whose pure slots [`EVIDENCE`] names.
const CLASS: &str = "IOBlockStorageDevice";

/// IOStorageFamily's manifest from the boot collection `boot`.
pub fn manifest(boot: &[u8]) -> Result<Manifest> {
    let mut manifest = super::manifest(boot, &FAMILY)?;
    let mut evidence = Vec::new();
    for (class, entry) in EVIDENCE {
        // Its superclass's gMetaClass is IOStorageFamily's.
        let layout = manifest::class_layout(boot, class, &[entry, FAMILY.entry])
            .with_context(|| format!("{class} in the boot collection"))?;
        evidence.push(layout);
    }
    let class = manifest
        .classes
        .iter_mut()
        .find(|class| class.layout.class == CLASS)
        .context("the family has no IOBlockStorageDevice")?;
    for (slot, names) in class.layout.slots.iter().enumerate() {
        if names.as_slice() != ["___cxa_pure_virtual"] {
            continue;
        }
        let mut agreed: Option<String> = None;
        for layout in &evidence {
            let named = layout
                .slots
                .get(slot)
                .and_then(|names| names.iter().find_map(|name| method(name, &layout.class)))
                .with_context(|| {
                    format!("{} names no method of its own in slot {slot}", layout.class)
                })?;
            let named = format!("{}{}{CLASS}{}", named.0, CLASS.len(), named.1);
            if let Some(agreed) = &agreed {
                ensure!(
                    *agreed == named,
                    "{CLASS} slot {slot}: {} has {named}, another implementer {agreed}",
                    layout.class
                );
            }
            agreed = Some(named);
        }
        class
            .expected
            .insert(slot, agreed.context("no evidence for a pure slot")?);
    }
    let names: Vec<&str> = EVIDENCE.iter().map(|(class, _)| *class).collect();
    manifest.source.push(format!(
        "`= name`: {CLASS}'s pure virtual slots declare the method {} all put there.",
        names.join(", ")
    ));
    Ok(manifest)
}

/// `symbol`'s mangling split around its class `class`: `__ZN` or `__ZNK`,
/// and the method after the class's name, when `symbol` is `class`'s own.
fn method(symbol: &str, class: &str) -> Option<(&'static str, String)> {
    let named = format!("{}{class}", class.len());
    for prefix in ["__ZNK", "__ZN"] {
        if let Some(method) = symbol
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix(named.as_str()))
        {
            return Some((prefix, method.to_owned()));
        }
    }
    None
}

/// Writes the manifest from `boot` beside the header.
pub fn generate(boot: &[u8]) -> Result<()> {
    let manifest = manifest(boot)?;
    let path = directory().join(FAMILY.manifest);
    std::fs::write(&path, manifest.to_text())?;
    println!("{}: {} classes", path.display(), manifest.classes.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{check_probe, header_sizes, object_vtables, tests::compile};
    use super::*;
    use std::fs;

    fn committed() -> Manifest {
        let text = fs::read_to_string(directory().join(FAMILY.manifest)).unwrap();
        Manifest::parse(&text).unwrap()
    }

    fn probe_object(headers: &std::path::Path) -> Vec<u8> {
        compile(&directory().join("storage/tests/slot_probe.cpp"), headers)
    }

    #[test]
    fn the_storage_header_puts_every_method_in_the_kernels_slot() {
        let vtables = object_vtables(&probe_object(&directory())).unwrap();
        check_probe(&vtables, &committed(), "StelaSlotProbeBlock", CLASS).unwrap();
    }

    /// The manifest names every pure slot, and only those.
    #[test]
    fn every_pure_slot_has_its_method() {
        let manifest = committed();
        let class = manifest.class(CLASS).unwrap();
        let pure: Vec<usize> = class
            .layout
            .slots
            .iter()
            .enumerate()
            .filter(|(_, names)| names.as_slice() == ["___cxa_pure_virtual"])
            .map(|(slot, _)| slot)
            .collect();
        assert_eq!(pure.len(), 14);
        assert_eq!(pure, class.expected.keys().copied().collect::<Vec<_>>());
        assert_eq!(
            class.expected[&291],
            "__ZN20IOBlockStorageDevice16doAsyncReadWriteEP18IOMemoryDescriptoryyP19IOStorageAttributesP19IOStorageCompletion"
        );
        assert_eq!(
            class.expected[&274],
            "__ZNK20IOBlockStorageDevice21doGetFormatCapacitiesEPyj"
        );
    }

    /// Two pure virtuals swapped in the header: the probe's vtable shows it.
    #[test]
    fn a_storage_header_with_two_methods_swapped_is_caught() {
        let header = fs::read_to_string(directory().join(FAMILY.header)).unwrap();
        let vendor = "\tvirtual char *getVendorString(void) = 0;\n";
        let product = "\tvirtual char *getProductString(void) = 0;\n";
        assert!(header.contains(&format!("{vendor}{product}")));
        let swapped = header.replace(&format!("{vendor}{product}"), &format!("{product}{vendor}"));
        let headers = tempfile::tempdir().unwrap();
        fs::create_dir_all(headers.path().join("storage")).unwrap();
        fs::write(headers.path().join(FAMILY.header), swapped).unwrap();
        let vtables = object_vtables(&probe_object(headers.path())).unwrap();
        let error = check_probe(&vtables, &committed(), "StelaSlotProbeBlock", CLASS)
            .unwrap_err()
            .to_string();
        assert!(
            error
                .contains("slot 277: the header has __ZN19StelaSlotProbeBlock16getProductStringEv")
                && error.contains(
                    "slot 278: the header has __ZN19StelaSlotProbeBlock15getVendorStringEv"
                ),
            "{error}"
        );
    }

    #[test]
    fn the_storage_header_asserts_the_kernels_size() {
        let manifest = committed();
        let header = fs::read_to_string(directory().join(FAMILY.header)).unwrap();
        let sizes = header_sizes(&header);
        let layout = &manifest.class(CLASS).unwrap().layout;
        assert_eq!(sizes.get(CLASS), Some(&layout.size));
        assert_eq!(layout.superclass, "IOService");
    }

    #[test]
    fn a_method_splits_from_its_class() {
        assert_eq!(
            method(
                "__ZNK24IOAHCIBlockStorageDevice21doGetFormatCapacitiesEPyj",
                "IOAHCIBlockStorageDevice"
            ),
            Some(("__ZNK", "21doGetFormatCapacitiesEPyj".to_owned()))
        );
        assert_eq!(
            method(
                "__ZN22IOBlockStorageServices12doEjectMediaEv",
                "IOBlockStorageServices"
            ),
            Some(("__ZN", "12doEjectMediaEv".to_owned()))
        );
        // Another class's method, inherited, is not the class's own.
        assert_eq!(
            method(
                "__ZN20IOBlockStorageDevice11requestIdleEv",
                "IOBlockStorageServices"
            ),
            None
        );
    }

    /// The committed manifest is what the reference kernelcache gives:
    /// `STELA_BOOT_KC=<kernelcache> cargo test -p xtask -- --ignored
    /// the_storage_manifest_is`.
    #[test]
    #[ignore = "oracle: STELA_BOOT_KC"]
    fn the_storage_manifest_is_the_reference_kernelcaches() {
        let Ok(kc) = std::env::var("STELA_BOOT_KC") else {
            eprintln!("STELA_BOOT_KC unset: nothing compared");
            return;
        };
        let boot = fs::read(kc).unwrap();
        let generated = manifest(&boot).unwrap().to_text();
        let committed = fs::read_to_string(directory().join(FAMILY.manifest)).unwrap();
        assert_eq!(generated, committed, "{}", FAMILY.manifest);
    }
}
