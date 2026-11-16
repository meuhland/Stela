// SPDX-License-Identifier: GPL-2.0-only
//! A GPU audio function's NVIDIA coherence bytes, which it does not
//! implement and VFIO does not forward.
use super::*;
use crate::bus::pci::ConfigError;
use core::cell::RefCell;

/// 256 bytes of configuration whose 0x4c..0x4e drop every write, as a GPU
/// audio function's do behind VFIO; `twisted` makes them read a value
/// that is neither the old one nor the one written.
struct Dropping {
    bytes: RefCell<[u8; 256]>,
    twisted: bool,
}
impl ConfigSpace for Dropping {
    fn read8(&self, offset: u16) -> Result<u8, ConfigError> {
        Ok(self.bytes.borrow()[usize::from(offset)])
    }
    fn read16(&self, offset: u16) -> Result<u16, ConfigError> {
        Ok(u16::from(self.read8(offset)?) | u16::from(self.read8(offset + 1)?) << 8)
    }
    fn read32(&self, offset: u16) -> Result<u32, ConfigError> {
        Ok(u32::from(self.read16(offset)?) | u32::from(self.read16(offset + 2)?) << 16)
    }
    fn write8(&mut self, offset: u16, value: u8) -> Result<(), ConfigError> {
        let mut bytes = self.bytes.borrow_mut();
        match offset {
            0x4c..=0x4e if self.twisted => bytes[usize::from(offset)] = 0x80,
            0x4c..=0x4e => {}
            _ => bytes[usize::from(offset)] = value,
        }
        Ok(())
    }
    fn write16(&mut self, offset: u16, value: u16) -> Result<(), ConfigError> {
        self.write8(offset, value as u8)?;
        self.write8(offset + 1, (value >> 8) as u8)
    }
    fn write32(&mut self, offset: u16, value: u32) -> Result<(), ConfigError> {
        self.write16(offset, value as u16)?;
        self.write16(offset + 2, (value >> 16) as u16)
    }
}

fn dropping(twisted: bool) -> Dropping {
    Dropping {
        bytes: RefCell::new([0; 256]),
        twisted,
    }
}

#[test]
fn a_dropped_coherence_write_is_rejected_by_name_and_value() {
    let mut config = dropping(false);
    let transreg = Setup::Nvidia.controls()[0];
    assert_eq!(
        transreg.apply(&mut config),
        Err(AttachError::Rejected(Rejection {
            field: ConfigField::VendorControl,
            offset: NVIDIA_TRANSREG,
            wrote: Some(u32::from(NVIDIA_ENABLE_COHBITS)),
            read: 0,
        }))
    );
    // The log names the register in hex.
    let Err(AttachError::Rejected(rejection)) = transreg.apply(&mut config) else {
        panic!("rejected");
    };
    assert_eq!(
        alloc::format!("{rejection:?}"),
        "Rejection { field: VendorControl, offset: 0x4e, wrote: Some(0xf), read: 0x0 }"
    );
}

#[test]
fn a_gpu_audio_function_keeps_dropped_coherence_writes_as_linux_does() {
    let mut config = dropping(false);
    for control in Setup::Nvidia.controls() {
        assert_eq!(control.apply_with(&mut config, true), Ok(false));
    }
    // A register that takes the write still reports it taken.
    let mut taking = dropping(false);
    let snoop = Control::Byte {
        offset: 0x42,
        mask: 7,
        value: 2,
    };
    assert_eq!(snoop.apply_with(&mut taking, true), Ok(true));
    assert_eq!(taking.read8(0x42), Ok(2));
    // A value that is neither the old one nor the written one is no
    // dropped write: refused even so.
    let mut twisted = dropping(true);
    assert_eq!(
        Setup::Nvidia.controls()[0].apply_with(&mut twisted, true),
        Err(AttachError::Rejected(Rejection {
            field: ConfigField::VendorControl,
            offset: NVIDIA_TRANSREG,
            wrote: Some(0x0f),
            read: 0x80,
        }))
    );
}
