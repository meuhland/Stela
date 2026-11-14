//! The PCIe root-complex probe's placement (`--probe-apcie`,
//! [`crate::apcie`]).

use super::*;
use crate::Result;
use crate::apcie::{ApcieProbe, WINDOW_SHIFT, Window};
use core::fmt::Write as _;
use stela_soc_data::board::apcie_probe::Apcie;

impl AppleDevices {
    /// Back every `reg` window of root complex `apcie` with one logging
    /// stub, and own its power resource (its first window's PA). A window
    /// that is untranslated, or that another model already owns (the PMGR
    /// eFuses, `reg[5]`, are the always-on fuse block's), stays as it is.
    /// Returns the `APCIE PROBE` description of what the guest is shown.
    pub fn add_apcie_probe(&mut self, apcie: &Apcie) -> Result<String> {
        let mut ports: Vec<u8> = apcie
            .ports
            .iter()
            .filter_map(|port| u8::try_from(port.index).ok())
            .filter(|&index| index < 32)
            .collect();
        if ports.is_empty() {
            // No `apcie-port` children: one root port per port window group.
            let groups = apcie.windows.len().saturating_sub(6) / 5;
            ports.extend((0..groups.min(32)).map(|index| index as u8));
        }
        let device = self.bus.add(Box::new(ApcieProbe::new(
            &apcie.name,
            apcie.first_bus,
            &ports,
        )));
        let mut ranges = String::new();
        let mut skipped = String::new();
        for window in &apcie.windows {
            let name = Window::of(window.index);
            let Some(physical) = window.physical else {
                let _ = write!(skipped, " {name}(untranslated)");
                continue;
            };
            if window.bytes == 0 || window.bytes >= 1 << WINDOW_SHIFT {
                let _ = write!(skipped, " {name}(size {:#x})", window.bytes);
                continue;
            }
            let offset = (window.index as u64) << WINDOW_SHIFT;
            match self
                .bus
                .check_free(physical, window.bytes)
                .and_then(|()| self.bus.map(device, physical, window.bytes, offset))
            {
                Ok(()) => {
                    let _ = write!(ranges, " {name}={physical:#x}+{:#x}", window.bytes);
                }
                Err(error) => {
                    let _ = write!(
                        skipped,
                        " {name}={physical:#x}+{:#x}({error:?})",
                        window.bytes
                    );
                }
            }
        }
        if let Some(physical) = apcie.windows.first().and_then(|window| window.physical) {
            self.register_power_device(physical, device, false)?;
        }
        let ports = apcie
            .ports
            .iter()
            .map(|port| format!("{}={}", port.name, port.index))
            .collect::<Vec<_>>()
            .join(",");
        let interrupts = apcie
            .interrupts
            .iter()
            .map(|irq| format!("{irq}"))
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!(
            "APCIE PROBE nodes=/arm-io/{} windows={} ports=[{ports}] root-ports={:?} \
             first-bus={} interrupts=[{interrupts}] ranges=[{} ] shared=[{} ]\n",
            apcie.name,
            apcie.windows.len(),
            self.bus
                .device::<ApcieProbe>(device)
                .map(ApcieProbe::root_ports)
                .unwrap_or_default(),
            apcie.first_bus,
            ranges,
            skipped
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stela_engine::machine::engine::Devices as _;
    use stela_soc_data::board::apcie_probe::{ApciePort, ApcieWindow};

    const FUSES: u64 = 0x2_3d2b_c000;

    fn window(index: usize, physical: Option<u64>, bytes: u64) -> ApcieWindow {
        ApcieWindow {
            index,
            physical,
            bytes,
        }
    }

    #[test]
    fn every_window_is_placed_except_those_another_model_owns() {
        let mut devices = AppleDevices::new();
        devices.add_aon_fuses(FUSES).unwrap();
        let ecam = 0x6_9000_0000;
        let rc = 0x6_8000_0000;
        let port0 = 0x6_8001_0000;
        let apcie = Apcie {
            name: "apcie".into(),
            windows: vec![
                window(0, Some(ecam), 0x100_0000),
                window(1, Some(rc), 0x4000),
                window(2, Some(0x6_8000_4000), 0x4000),
                window(3, Some(0x6_8000_8000), 0x4000),
                window(4, None, 0x4000),
                window(5, Some(FUSES), 0x4000),
                window(6, Some(port0), 0x8000),
            ],
            interrupts: vec![1061],
            ports: vec![ApciePort {
                name: "pci-bridge0".into(),
                index: 0,
            }],
            first_bus: 0,
        };
        let line = devices.add_apcie_probe(&apcie).unwrap();
        assert!(line.starts_with("APCIE PROBE nodes=/arm-io/apcie windows=7"));
        assert!(line.contains("rc=0x680000000+0x4000"), "{line}");
        assert!(line.contains("axi2af(untranslated)"), "{line}");
        assert!(line.contains("fuse=0x23d2bc000+0x4000("), "{line}");
        assert!(line.contains("root-ports=[0]"), "{line}");
        // The stub answers through the bus: the root complex is ready, the
        // root port's header is a bridge, and an empty bus has no device.
        assert_eq!(devices.read(0, rc + 0x58, 4, 0).map(|v| v & 1), Ok(1));
        assert_eq!(devices.read(0, ecam + 0xa, 2, 0), Ok(0x0604));
        assert_eq!(devices.read(0, ecam + (1 << 20), 4, 0), Ok(0xffff_ffff));
        devices.write(0, port0 + 0x800, 4, 1, 0).unwrap();
        assert_eq!(devices.read(0, port0 + 0x804, 4, 0), Ok(1));
        // The fuse block keeps its own model.
        assert!(devices.read(0, FUSES + 0x4c4, 4, 0).is_ok());
    }
}
