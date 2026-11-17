//! Where an AMD GPU's video BIOS lies in the firmware's ACPI `VFCT` table.
//!
//! An integrated AMD GPU has no option ROM of its own: the firmware
//! publishes its video BIOS, with the tables amdgpu reads its display
//! outputs, clocks and power limits from, as an image in the `VFCT` table.
//! Linux reads it through its ACPI tables (`amdgpu_acpi_vfct_bios`), which
//! Stela's Linux profile does not build. The table is plain firmware memory,
//! so the boot finds the image and gives its place to Linux as the
//! function's platform ROM (`pci_dev::rom`), which the driver reads when the
//! other places hold none (`amdgpu_read_platform_bios`) and checks again in
//! full. The rules here are `amdgpu_acpi_vfct_bios`'s and
//! `amdgpu_acpi_vfct_match`'s.
use core::ops::Range;

/// The table's signature in the ACPI root tables.
pub const SIGNATURE: [u8; 4] = *b"VFCT";

/// The table header's length field (`acpi_table_header::length`).
const LENGTH: usize = 4;
/// `UEFI_ACPI_VFCT`: the ACPI header, a UUID, the offsets of the first video
/// BIOS and library images, and four reserved words.
const TABLE: usize = 0x4c;
const VBIOS_IMAGE_OFFSET: usize = 0x34;
/// `VFCT_IMAGE_HEADER`: bus, device, function, vendor and device IDs, the
/// subsystem's, a revision and the image's length; the image follows.
const HEADER: usize = 0x1c;
const BUS: usize = 0;
const DEVICE: usize = 4;
const FUNCTION: usize = 8;
const VENDOR_ID: usize = 0xc;
const DEVICE_ID: usize = 0xe;
const IMAGE_LENGTH: usize = 0x18;

/// The PCI function an image is looked up for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Function {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
}

/// Why the table gives the function no image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The bytes are shorter than the table's fixed part or than its own
    /// length field says.
    TooShort,
    /// An image's header or bytes run past the table's end.
    Truncated,
    /// The table ends without an image for the function.
    NoImage,
    /// The function's image does not hold an ATOM BIOS.
    NotAtom,
}

/// The function's image in the table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Image {
    /// The image's bytes within the table.
    pub bytes: Range<usize>,
    /// The image is for the function's device and vendor, device number and
    /// function number, but names another bus: firmware numbered the bus
    /// differently when it wrote the table. Linux takes it and says so.
    pub renumbered_from: Option<u32>,
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn half(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

/// The length the table's header gives, for a caller that has mapped only
/// the header so far.
pub fn length(header: &[u8]) -> Option<usize> {
    (header.len() >= LENGTH + 4 && header[..4] == SIGNATURE).then(|| word(header, LENGTH) as usize)
}

/// Whether `image` is an ATOM BIOS, as `check_atom_bios` decides: the option
/// ROM signature, then "ATOM" or "MOTA" four bytes into the header the word
/// at 0x48 points at.
pub fn is_atom(image: &[u8]) -> bool {
    if image.len() < 0x4a || image[..2] != [0x55, 0xaa] {
        return false;
    }
    let header = usize::from(half(image, 0x48));
    header != 0
        && image
            .get(header + 4..header + 8)
            .is_some_and(|name| name == b"ATOM" || name == b"MOTA")
}

/// The first image of `table` for `function`: one with the function's vendor
/// and device IDs and its device and function numbers. Images of zero length
/// are passed over.
pub fn image(table: &[u8], function: Function) -> Result<Image, Error> {
    if table.len() < TABLE {
        return Err(Error::TooShort);
    }
    let size = word(table, LENGTH) as usize;
    if size < TABLE || size > table.len() {
        return Err(Error::TooShort);
    }
    let mut offset = word(table, VBIOS_IMAGE_OFFSET) as usize;
    while offset < size {
        let header = offset;
        offset += HEADER;
        if offset > size {
            return Err(Error::Truncated);
        }
        let length = word(table, header + IMAGE_LENGTH) as usize;
        let bytes = offset..offset.checked_add(length).ok_or(Error::Truncated)?;
        offset = bytes.end;
        if offset > size {
            return Err(Error::Truncated);
        }
        if length == 0
            || half(table, header + VENDOR_ID) != function.vendor
            || half(table, header + DEVICE_ID) != function.device_id
            || word(table, header + DEVICE) != u32::from(function.device)
            || word(table, header + FUNCTION) != u32::from(function.function)
        {
            continue;
        }
        if !is_atom(&table[bytes.clone()]) {
            return Err(Error::NotAtom);
        }
        let bus = word(table, header + BUS);
        return Ok(Image {
            bytes,
            renumbered_from: (bus != u32::from(function.bus)).then_some(bus),
        });
    }
    Err(Error::NoImage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};

    const GPU: Function = Function {
        bus: 0x10,
        device: 0,
        function: 0,
        vendor: 0x1002,
        device_id: 0x13c0,
    };

    /// An ATOM BIOS of `length` bytes whose header lies at 0x60.
    fn atom(length: usize) -> Vec<u8> {
        let mut image = vec![0; length];
        image[..2].copy_from_slice(&[0x55, 0xaa]);
        image[0x48..0x4a].copy_from_slice(&0x60_u16.to_le_bytes());
        image[0x64..0x68].copy_from_slice(b"ATOM");
        image
    }

    /// A table of images, each for a bus, device, function and device ID.
    fn table(images: &[(u32, u32, u32, u16, &[u8])]) -> Vec<u8> {
        let mut table = vec![0; TABLE];
        table[..4].copy_from_slice(&SIGNATURE);
        table[VBIOS_IMAGE_OFFSET..VBIOS_IMAGE_OFFSET + 4]
            .copy_from_slice(&(TABLE as u32).to_le_bytes());
        for (bus, device, function, id, image) in images {
            let mut header = [0; HEADER];
            header[BUS..BUS + 4].copy_from_slice(&bus.to_le_bytes());
            header[DEVICE..DEVICE + 4].copy_from_slice(&device.to_le_bytes());
            header[FUNCTION..FUNCTION + 4].copy_from_slice(&function.to_le_bytes());
            header[VENDOR_ID..VENDOR_ID + 2].copy_from_slice(&0x1002_u16.to_le_bytes());
            header[DEVICE_ID..DEVICE_ID + 2].copy_from_slice(&id.to_le_bytes());
            header[IMAGE_LENGTH..].copy_from_slice(&(image.len() as u32).to_le_bytes());
            table.extend_from_slice(&header);
            table.extend_from_slice(image);
        }
        let length = table.len() as u32;
        table[LENGTH..LENGTH + 4].copy_from_slice(&length.to_le_bytes());
        table
    }

    #[test]
    fn the_functions_image_is_found_among_others() {
        let (other, mine) = (atom(0x200), atom(0x400));
        let table = table(&[
            (3, 0, 0, 0x7480, &other),
            (0x10, 0, 0, 0x13c0, &[]),
            (0x10, 0, 0, 0x13c0, &mine),
        ]);
        assert_eq!(length(&table[..8]), Some(table.len()));
        let found = image(&table, GPU).unwrap();
        assert_eq!(found.renumbered_from, None);
        assert_eq!(found.bytes.len(), 0x400);
        assert_eq!(found.bytes.end, table.len());
        assert!(is_atom(&table[found.bytes]));
        // Another function of the same device, or another device ID, has none.
        for function in [
            Function { function: 1, ..GPU },
            Function { device: 2, ..GPU },
            Function {
                device_id: 0x164e,
                ..GPU
            },
            Function {
                vendor: 0x1022,
                ..GPU
            },
        ] {
            assert_eq!(image(&table, function), Err(Error::NoImage));
        }
    }

    #[test]
    fn an_image_written_under_another_bus_number_is_taken_and_named() {
        let mine = atom(0x100);
        let table = table(&[(0x0c, 0, 0, 0x13c0, &mine)]);
        assert_eq!(image(&table, GPU).unwrap().renumbered_from, Some(0x0c));
    }

    #[test]
    fn a_broken_table_or_image_is_refused() {
        let mine = atom(0x100);
        let whole = table(&[(0x10, 0, 0, 0x13c0, &mine)]);
        assert_eq!(image(&whole[..TABLE - 1], GPU), Err(Error::TooShort));
        // The mapping is shorter than the table says it is.
        assert_eq!(image(&whole[..whole.len() - 1], GPU), Err(Error::TooShort));
        assert_eq!(length(b"SSDT\x10\0\0\0"), None);
        // The table's length ends inside the image, then inside its header.
        for shorter in [1, 0x100 + 1] {
            let mut cut = whole.clone();
            let length = (whole.len() - shorter) as u32;
            cut[LENGTH..LENGTH + 4].copy_from_slice(&length.to_le_bytes());
            assert_eq!(image(&cut, GPU), Err(Error::Truncated));
        }
        // No image at all, then one that is not an ATOM BIOS.
        assert_eq!(image(&table(&[]), GPU), Err(Error::NoImage));
        let mut legacy = mine.clone();
        legacy[0x64..0x68].copy_from_slice(b"VGA ");
        assert_eq!(
            image(&table(&[(0x10, 0, 0, 0x13c0, &legacy)]), GPU),
            Err(Error::NotAtom)
        );
        assert!(!is_atom(&mine[..0x49]));
        let mut unsigned = mine;
        unsigned[0] = 0;
        assert!(!is_atom(&unsigned));
    }
}
