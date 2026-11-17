//! A GPU's ACPI data for GSP-RM.
//!
//! Upstream nouveau reads, under `CONFIG_ACPI`, what GSP-RM takes from the
//! firmware about a GPU: the backlight table of its NBCI or NVHG `_DSM`
//! function 0x14 (`NV2080_CTRL_CMD_INTERNAL_INIT_BRIGHTC_STATE_LOAD`), and
//! its `_DOD` display list and JT and NVOP capabilities (the system info's
//! `ACPI_METHOD_DATA`). This kernel has no ACPI, so boot evaluates them with
//! its own interpreter before the GPU starts ([`provide`]) and nouveau reads
//! them back here. Without them GSP-RM knows no internal panel and refuses
//! every brightness control with NV_ERR_NOT_SUPPORTED, and a laptop panel its
//! discrete GPU drives stays dark.
use alloc::{boxed::Box, vec::Vec};
use core::{
    ffi::c_int,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU32, Ordering},
};

/// What boot read from one GPU's ACPI companion.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Acpi {
    /// The NBCI or NVHG `_DSM` 0x14 package, flattened as nouveau copies it.
    pub backlight: Vec<u8>,
    /// The `_DOD` display IDs.
    pub dod: Option<Vec<u32>>,
    /// The JT `_DSM` function 1 capabilities.
    pub jt: Option<u32>,
    /// The NVOP `_DSM` function 0x1a Optimus capabilities.
    pub nvop: Option<u32>,
}

/// No address: `u32::MAX` is not a PCI key [`key`] makes.
const NONE: u32 = u32::MAX;
/// The GPUs whose data is kept, each in its own place.
const PLACES: usize = 8;

/// Each place's function and its data; a key is stored after its data.
static KEYS: [AtomicU32; PLACES] = [const { AtomicU32::new(NONE) }; PLACES];
static DATA: [AtomicPtr<Acpi>; PLACES] = [const { AtomicPtr::new(ptr::null_mut()) }; PLACES];

/// The segment, bus and devfn of a function as one word.
fn key(segment: u16, bus: u8, devfn: u8) -> u32 {
    (u32::from(segment) << 16) | (u32::from(bus) << 8) | u32::from(devfn)
}

/// Keep `acpi` for the function at `segment`, `bus`, `device`, `function`,
/// whose start reads it, beside each other GPU's. Called on one processor
/// before that GPU starts; a later call for the same function replaces its
/// data (the earlier stays allocated, as C may hold it). `false` when every
/// place holds another GPU's.
pub fn provide(segment: u16, bus: u8, device: u8, function: u8, acpi: Acpi) -> bool {
    let wanted = key(segment, bus, (device << 3) | (function & 7));
    let keys = || KEYS.iter().map(|key| key.load(Ordering::Acquire));
    let Some(place) = keys()
        .position(|key| key == wanted)
        .or_else(|| keys().position(|key| key == NONE))
    else {
        return false;
    };
    let acpi: &'static mut Acpi = Box::leak(Box::new(acpi));
    KEYS[place].store(NONE, Ordering::Release);
    DATA[place].store(acpi, Ordering::Release);
    KEYS[place].store(wanted, Ordering::Release);
    true
}

fn lookup(domain: u32, bus: u32, devfn: u32) -> Option<&'static Acpi> {
    let wanted = key(
        u16::try_from(domain).ok()?,
        u8::try_from(bus).ok()?,
        u8::try_from(devfn).ok()?,
    );
    let place = KEYS
        .iter()
        .position(|key| key.load(Ordering::Acquire) == wanted)?;
    // SAFETY: a pointer `provide` leaked, alive for the boot.
    unsafe { DATA[place].load(Ordering::Acquire).as_ref() }
}

/// The backlight table boot kept for the function at `domain`, `bus`,
/// `devfn`: 0 with `data` and `size` set, or -ENOENT (-2) with none.
///
/// # Safety
/// `data` and `size` are valid writable pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stela_linux_gpu_backlight_data(
    domain: u32,
    bus: u32,
    devfn: u32,
    data: *mut *const u8,
    size: *mut usize,
) -> c_int {
    if data.is_null() || size.is_null() {
        return -22;
    }
    // SAFETY: valid writable slots under the caller contract.
    unsafe {
        data.write(ptr::null());
        size.write(0);
    }
    match lookup(domain, bus, devfn) {
        Some(acpi) if !acpi.backlight.is_empty() => {
            // SAFETY: as above; the leaked bytes live for the boot.
            unsafe {
                data.write(acpi.backlight.as_ptr());
                size.write(acpi.backlight.len());
            }
            0
        }
        _ => -2,
    }
}

/// The `_DOD` IDs boot kept for the function: 0 with up to `max` of them in
/// `ids` and their number in `count`, or -ENOENT (-2), or -E2BIG (-7) when
/// they do not fit.
///
/// # Safety
/// `ids` is writable for `max` words and `count` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stela_linux_gpu_acpi_dod(
    domain: u32,
    bus: u32,
    devfn: u32,
    ids: *mut u32,
    max: u32,
    count: *mut u32,
) -> c_int {
    if ids.is_null() || count.is_null() {
        return -22;
    }
    // SAFETY: writable under the caller contract.
    unsafe { count.write(0) };
    let Some(dod) = lookup(domain, bus, devfn).and_then(|acpi| acpi.dod.as_ref()) else {
        return -2;
    };
    if dod.len() > max as usize {
        return -7;
    }
    // SAFETY: `ids` is writable for `max` words, and `dod` fits.
    unsafe {
        ptr::copy_nonoverlapping(dod.as_ptr(), ids, dod.len());
        count.write(dod.len() as u32);
    }
    0
}

/// One capability word boot kept for the function: `which` 0 is JT's, 1
/// NVOP's. 0 with `value` set, or -ENOENT (-2).
///
/// # Safety
/// `value` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn stela_linux_gpu_acpi_caps(
    domain: u32,
    bus: u32,
    devfn: u32,
    which: u32,
    value: *mut u32,
) -> c_int {
    if value.is_null() {
        return -22;
    }
    let acpi = lookup(domain, bus, devfn);
    let word = match which {
        0 => acpi.and_then(|acpi| acpi.jt),
        1 => acpi.and_then(|acpi| acpi.nvop),
        _ => return -22,
    };
    match word {
        Some(word) => {
            // SAFETY: writable under the caller contract.
            unsafe { value.write(word) };
            0
        }
        None => -2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn caps(domain: u32, bus: u32, devfn: u32, which: u32) -> Option<u32> {
        let mut value = 0;
        // SAFETY: a valid writable word.
        let found = unsafe { stela_linux_gpu_acpi_caps(domain, bus, devfn, which, &mut value) };
        (found == 0).then_some(value)
    }

    /// Each GPU reads back its own companion's data, a second GPU's
    /// leaving the first's in place, and a function given none reads none.
    #[test]
    fn each_gpu_keeps_its_own_acpi_data() {
        let first = Acpi {
            backlight: vec![1, 2, 3, 4],
            dod: Some(vec![0x8000_a450]),
            jt: Some(0x2001_6003),
            nvop: None,
        };
        let second = Acpi {
            jt: Some(7),
            ..Acpi::default()
        };
        assert!(provide(0, 0x41, 0, 0, first));
        assert!(provide(0, 0x42, 0, 0, second));
        assert_eq!(caps(0, 0x41, 0, 0), Some(0x2001_6003));
        assert_eq!(caps(0, 0x41, 0, 1), None);
        assert_eq!(caps(0, 0x42, 0, 0), Some(7));
        assert_eq!(caps(0, 0x43, 0, 0), None);
        let (mut data, mut size) = (ptr::null(), 0);
        // SAFETY: valid writable slots.
        let found = unsafe { stela_linux_gpu_backlight_data(0, 0x41, 0, &mut data, &mut size) };
        assert_eq!((found, size), (0, 4));
        // SAFETY: the kept table's four bytes.
        let table = unsafe { core::slice::from_raw_parts(data, size) };
        assert_eq!(table, [1, 2, 3, 4]);
        // SAFETY: valid writable slots.
        let found = unsafe { stela_linux_gpu_backlight_data(0, 0x42, 0, &mut data, &mut size) };
        assert_eq!((found, size), (-2, 0));
        let mut ids = [0u32; 16];
        let mut count = 0;
        // SAFETY: sixteen writable words and a writable count.
        let found =
            unsafe { stela_linux_gpu_acpi_dod(0, 0x41, 0, ids.as_mut_ptr(), 16, &mut count) };
        assert_eq!((found, count, ids[0]), (0, 1, 0x8000_a450));
        // Data given again for the same function replaces its own.
        assert!(provide(0, 0x41, 0, 0, Acpi::default()));
        assert_eq!(caps(0, 0x41, 0, 0), None);
        assert_eq!(caps(0, 0x42, 0, 0), Some(7));
    }
}
