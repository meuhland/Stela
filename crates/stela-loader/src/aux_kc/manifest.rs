//! The layout of a kernel class as a boot collection has it: each vtable
//! slot's method, named by the symbols exported at its address, and the
//! instance size the class's `MetaClass` constructor gives
//! `OSMetaClass::OSMetaClass`. A kext deriving from a family class whose
//! header Stela writes by hand checks that header against this
//! ([`ClassLayout`]): a virtual method in the wrong slot, or a wrong parent
//! size, would link and dispatch wrongly or corrupt memory at run time.
//!
//! The size comes from the code: `MetaClass::MetaClass()` (`C2`) loads the
//! class's name into `x1`, its superclass's `gMetaClass` into `x2` and its
//! size into `w3`, then branches to `OSMetaClass::OSMetaClass`. The decoder
//! reads the instructions up to that branch and takes what `w3` holds; the
//! name in `x1` must be the class's, or nothing is answered.
use alloc::{
    borrow::ToOwned,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt::Write as _;

use super::boot::{BootCollection, KERNEL};
use crate::Result;
use stela_contracts::{refuse, require};

/// Slots read at most: no IOKit class has this many.
const MOST_SLOTS: usize = 1024;
/// Instructions of a `MetaClass` constructor read at most before its call.
const MOST_INSTRUCTIONS: usize = 32;

/// One class of a boot collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassLayout {
    pub class: String,
    /// Its superclass, through its `superClass` member.
    pub superclass: String,
    /// The bytes of an instance, from its `MetaClass` constructor.
    pub size: u64,
    /// Each vtable slot's method: the symbols exported at its address, in
    /// name order; empty where no exported symbol names it.
    pub slots: Vec<Vec<String>>,
}

/// `class`'s layout in `boot`, its slots named from the symbols of the
/// kernel and of `entries` (the libraries a kext lists).
pub fn class_layout(boot: &[u8], class: &str, entries: &[&str]) -> Result<ClassLayout> {
    let boot = BootCollection::parse(boot)?;
    let mut names: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    let mut addresses: BTreeMap<&str, u64> = BTreeMap::new();
    for entry in core::iter::once(&KERNEL).chain(entries) {
        for (name, address) in boot.exports(entry)? {
            names.entry(address).or_default().push(name);
            addresses.insert(name, address);
        }
    }
    let mangled = format!("{}{class}", class.len());
    let symbol = |name: String| {
        addresses
            .get(name.as_str())
            .copied()
            .ok_or_else(|| refuse!("the boot collection exports no {name}", name))
    };
    let vtable = symbol(format!("__ZTV{mangled}"))?;
    let mut slots = Vec::new();
    for index in 0..MOST_SLOTS {
        let at = vtable + 16 + 8 * index as u64;
        // The next vtable, or what follows, starts at an exported name or a null.
        if index > 0 && names.contains_key(&at) {
            break;
        }
        let Some(target) = boot.pointer(at)? else {
            break;
        };
        let mut named: Vec<String> = names
            .get(&target)
            .map(|named| named.iter().map(|name| (*name).to_owned()).collect())
            .unwrap_or_default();
        named.sort();
        slots.push(named);
    }
    require!(!slots.is_empty(), "{class}'s vtable has no slot", class);
    let parent_meta = boot
        .pointer(symbol(format!("__ZN{mangled}10superClassE"))?)?
        .ok_or_else(|| refuse!("{class}'s superClass is null", class))?;
    let superclass = names
        .get(&parent_meta)
        .into_iter()
        .flatten()
        .find_map(|name| class_of_meta(name))
        .ok_or_else(|| refuse!("{class}'s superClass names no gMetaClass", class))?;
    let constructor = symbol(format!("__ZN{mangled}9MetaClassC2Ev"))?;
    let call = metaclass_call(&boot, constructor)?;
    let name = c_string(&boot, call.name)?;
    require!(
        name == class,
        "{class}'s MetaClass constructor names {name}",
        class,
        name
    );
    Ok(ClassLayout {
        class: class.into(),
        superclass,
        size: call.size,
        slots,
    })
}

/// `Base` of `__ZN<n>Base10gMetaClassE`.
fn class_of_meta(symbol: &str) -> Option<String> {
    let inner = symbol.strip_prefix("__ZN")?.strip_suffix("10gMetaClassE")?;
    let digits = inner.bytes().take_while(u8::is_ascii_digit).count();
    let length: usize = inner[..digits].parse().ok()?;
    (inner.len() == digits + length).then(|| inner[digits..].to_string())
}

fn c_string(boot: &BootCollection<'_>, address: u64) -> Result<String> {
    let mut name = Vec::new();
    for at in address..address + 256 {
        let byte = boot.read(at, 1)?[0];
        if byte == 0 {
            return String::from_utf8(name)
                .map_err(|_| refuse!("a class name is not UTF-8").into());
        }
        name.push(byte);
    }
    Err(refuse!("a class name past 256 bytes").into())
}

/// What a `MetaClass` constructor hands `OSMetaClass::OSMetaClass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaClassCall {
    /// `x1`: the class's name.
    pub name: u64,
    /// `w3`: the instance size.
    pub size: u64,
}

/// Reads the constructor at `address` up to its first call.
fn metaclass_call(boot: &BootCollection<'_>, address: u64) -> Result<MetaClassCall> {
    let code = boot.read(address, 4 * MOST_INSTRUCTIONS)?;
    let words: Vec<u32> = code
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_le_bytes(*word))
        .collect();
    decode_call(address, &words).ok_or_else(|| {
        refuse!(
            "the MetaClass constructor at {address:#x} sets no name and size before its call",
            address
        )
        .into()
    })
}

/// Follows `adrp`/`add` into `x1` and `movz`/`movk`/`movn`/`orr` into
/// `w3`/`x3` from `pc` to the first `bl`, `b` or return.
pub fn decode_call(pc: u64, words: &[u32]) -> Option<MetaClassCall> {
    let mut registers = [None::<u64>; 32];
    for (index, &word) in words.iter().enumerate() {
        let at = pc + 4 * index as u64;
        let rd = (word & 31) as usize;
        let rn = (word >> 5 & 31) as usize;
        let wide = word >> 31 == 1;
        let width_mask = if wide { u64::MAX } else { 0xffff_ffff };
        if word & 0xfc00_0000 == 0x9400_0000 || word & 0xfc00_0000 == 0x1400_0000 {
            // bl, b: the call.
            return Some(MetaClassCall {
                name: registers[1]?,
                size: registers[3]?,
            });
        }
        if word == 0xd65f_03c0 || word == 0xd65f_0fff {
            // ret, retab: no call.
            return None;
        }
        if word & 0x9f00_0000 == 0x9000_0000 {
            // adrp
            let immediate = (u64::from(word >> 5 & 0x7_ffff) << 2) | u64::from(word >> 29 & 3);
            let offset = ((immediate << 43) as i64 >> 43) << 12;
            registers[rd] = Some((at & !0xfff).wrapping_add_signed(offset));
        } else if word & 0x7f80_0000 == 0x1100_0000 {
            // add (immediate)
            let shift = if word >> 22 & 1 == 1 { 12 } else { 0 };
            let immediate = u64::from(word >> 10 & 0xfff) << shift;
            registers[rd] = registers[rn].map(|base| base.wrapping_add(immediate) & width_mask);
        } else if word & 0x7f80_0000 == 0x5280_0000 {
            // movz
            let shift = 16 * (word >> 21 & 3);
            registers[rd] = Some(u64::from(word >> 5 & 0xffff) << shift);
        } else if word & 0x7f80_0000 == 0x7280_0000 {
            // movk
            let shift = 16 * (word >> 21 & 3);
            registers[rd] = registers[rd]
                .map(|old| old & !(0xffff << shift) | u64::from(word >> 5 & 0xffff) << shift);
        } else if word & 0x7f80_0000 == 0x1280_0000 {
            // movn
            let shift = 16 * (word >> 21 & 3);
            registers[rd] = Some(!(u64::from(word >> 5 & 0xffff) << shift) & width_mask);
        } else if word & 0x7f80_0000 == 0x3200_0000 && rn == 31 {
            // orr (immediate) from the zero register: mov of a bitmask.
            let n = word >> 22 & 1;
            let immr = word >> 16 & 0x3f;
            let imms = word >> 10 & 0x3f;
            registers[rd] = bit_mask(n, imms, immr, if wide { 64 } else { 32 });
        } else if rd < 32 && writes_register(word) {
            // Anything else that writes rd leaves it unknown.
            registers[rd] = None;
        }
    }
    None
}

/// Whether `word` is a data-processing or load instruction writing its
/// `Rd`/`Rt` field (the decoder forgets the register then).
fn writes_register(word: u32) -> bool {
    let op0 = word >> 25 & 0xf;
    // Data processing (immediate or register) and loads.
    matches!(op0, 0b1000 | 0b1001 | 0b0101 | 0b1101)
        || (op0 & 0b0101 == 0b0100 && word >> 22 & 1 == 1)
}

/// AArch64 `DecodeBitMasks`' `wmask` for a logical immediate.
pub fn bit_mask(n: u32, imms: u32, immr: u32, width: u32) -> Option<u64> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let length = 31 - combined.leading_zeros();
    if length < 1 {
        return None;
    }
    let size = 1u32 << length;
    let levels = size - 1;
    let (s, r) = (imms & levels, immr & levels);
    if s == levels {
        return None;
    }
    let mask = |bits: u32| {
        if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        }
    };
    let ones = mask(s + 1);
    let element = if r == 0 {
        ones
    } else {
        ((ones >> r) | (ones << (size - r))) & mask(size)
    };
    let mut value = 0u64;
    let mut at = 0;
    while at < width {
        value |= element << at;
        at += size;
    }
    Some(value & mask(width))
}

/// A slot manifest: the classes a family header declares, as one boot
/// collection lays them out, as text the repository keeps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// What generated it, kept as comment lines.
    pub source: Vec<String>,
    pub classes: Vec<ManifestClass>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestClass {
    pub layout: ClassLayout,
    /// For a slot the collection names only `___cxa_pure_virtual`, the
    /// method its header declares there, from evidence the manifest's
    /// generator names (by slot index).
    pub expected: BTreeMap<usize, String>,
}

impl Manifest {
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        for line in &self.source {
            writeln!(text, "# {line}").unwrap();
        }
        for class in &self.classes {
            let layout = &class.layout;
            writeln!(
                text,
                "class {} {} {:#x} {}",
                layout.class,
                layout.superclass,
                layout.size,
                layout.slots.len()
            )
            .unwrap();
            for (index, names) in layout.slots.iter().enumerate() {
                let named = if names.is_empty() {
                    String::from("?")
                } else {
                    names.join("|")
                };
                write!(text, "{index} {:#x} {named}", 16 + 8 * index).unwrap();
                if let Some(expected) = class.expected.get(&index) {
                    write!(text, " = {expected}").unwrap();
                }
                text.push('\n');
            }
        }
        text
    }

    /// Reads [`Self::to_text`]'s text back.
    pub fn parse(text: &str) -> Result<Self> {
        let mut source = Vec::new();
        let mut classes: Vec<ManifestClass> = Vec::new();
        for line in text.lines() {
            if let Some(comment) = line.strip_prefix("# ") {
                source.push(comment.into());
                continue;
            }
            let fields: Vec<&str> = line.split(' ').collect();
            if let ["class", class, superclass, size, _count] = fields[..] {
                let size = size
                    .strip_prefix("0x")
                    .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| refuse!("a manifest size {size}", size))?;
                classes.push(ManifestClass {
                    layout: ClassLayout {
                        class: class.into(),
                        superclass: superclass.into(),
                        size,
                        slots: Vec::new(),
                    },
                    expected: BTreeMap::new(),
                });
                continue;
            }
            let class = classes
                .last_mut()
                .ok_or_else(|| refuse!("a manifest slot before its class"))?;
            let (index, offset, named, expected) = match fields[..] {
                [index, offset, named] => (index, offset, named, None),
                [index, offset, named, "=", expected] => (index, offset, named, Some(expected)),
                _ => return Err(refuse!("a manifest line {line}", line).into()),
            };
            let index: usize = index
                .parse()
                .map_err(|_| refuse!("a manifest slot index {index}", index))?;
            require!(
                index == class.layout.slots.len() && offset == format!("{:#x}", 16 + 8 * index),
                "manifest slot {index} out of order",
                index
            );
            class.layout.slots.push(if named == "?" {
                Vec::new()
            } else {
                named.split('|').map(Into::into).collect()
            });
            if let Some(expected) = expected {
                class.expected.insert(index, expected.into());
            }
        }
        Ok(Self { source, classes })
    }

    pub fn class(&self, name: &str) -> Option<&ManifestClass> {
        self.classes.iter().find(|class| class.layout.class == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn the_size_is_what_w3_holds_at_the_call() {
        // IOSerialDriverSync::MetaClass::MetaClass() in 26B123, as llvm-mc
        // reads it: hint, pacibsp, stp, mov x29; adrp/add x1; adrp/ldr x2;
        // mov w3, #0x88; bl.
        let pc = 0xffff_fe00_0af9_6438;
        let words = [
            0xd503_245f,
            0xd503_237f,
            0xa9bf_7bfd,
            0x9100_03fd,
            0xb0fe_5ea1,
            0x913d_e821,
            0x90fe_c8c2,
            0xf942_4042,
            0x5280_1103,
            0x9400_0666,
        ];
        let call = decode_call(pc, &words).unwrap();
        assert_eq!(call.size, 0x88);
        // adrp x1, #-54702080; add x1, x1, #3962.
        assert_eq!(call.name, (pc & !0xfff).wrapping_sub(54_702_080) + 3962);
        // No call before a return; nothing in w3.
        assert_eq!(decode_call(pc, &[0x5280_1103, 0xd65f_0fff]), None);
        assert_eq!(decode_call(pc, &words[..8]), None);
    }

    #[test]
    fn sizes_come_through_every_way_to_move_an_immediate() {
        let adrp_add = [0x9000_0001, 0x9100_0021];
        let call = |mov: &[u32]| {
            let mut words = adrp_add.to_vec();
            words.extend_from_slice(mov);
            words.push(0x9400_0000);
            decode_call(0x1000, &words).map(|call| call.size)
        };
        // movz w3, #0x1, lsl #16; movk w3, #0x20.
        assert_eq!(call(&[0x52a0_0023, 0x7280_0403]), Some(0x1_0020));
        // movn w3, #0 (all ones in 32 bits).
        assert_eq!(call(&[0x1280_0003]), Some(0xffff_ffff));
        // orr w3, wzr, #0x100 (mov w3, #0x100).
        assert_eq!(call(&[0x3218_03e3]), Some(0x100));
        // Something else writing w3 forgets it.
        assert_eq!(call(&[0x5280_1103, 0x0b03_0063]), None);
        assert_eq!(bit_mask(0, 0b000111, 0, 32), Some(0xff));
        assert_eq!(bit_mask(1, 0b111111, 0, 64), None);
    }

    #[test]
    fn a_manifest_reads_back_what_it_wrote() {
        let manifest = Manifest {
            source: vec!["from a test".into()],
            classes: vec![ManifestClass {
                layout: ClassLayout {
                    class: "Driver".into(),
                    superclass: "IOService".into(),
                    size: 0x88,
                    slots: vec![
                        vec!["__ZN6DriverD1Ev".into()],
                        vec![],
                        vec!["__ZN1A1fEv".into(), "__ZN1B1gEv".into()],
                        vec!["___cxa_pure_virtual".into()],
                    ],
                },
                expected: [(3, "__ZN6Driver1hEPv".to_string())].into(),
            }],
        };
        let text = manifest.to_text();
        assert!(text.contains("class Driver IOService 0x88 4\n"));
        assert!(text.contains("1 0x18 ?\n"));
        assert!(text.contains("2 0x20 __ZN1A1fEv|__ZN1B1gEv\n"));
        assert!(text.contains("3 0x28 ___cxa_pure_virtual = __ZN6Driver1hEPv\n"));
        assert_eq!(Manifest::parse(&text).unwrap(), manifest);
        assert!(Manifest::parse("0 0x10 x\n").is_err());
        assert!(Manifest::parse("class A B 0x8 1\n1 0x18 x\n").is_err());
        assert_eq!(
            class_of_meta("__ZN9IOService10gMetaClassE").unwrap(),
            "IOService"
        );
        assert_eq!(class_of_meta("__ZN9IOService9metaClassE"), None);
    }
}
