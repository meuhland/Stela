//! The PCIe root-complex probe's placement (`--probe-apcie`,
//! [`crate::apcie`]).

use super::*;
use crate::Result;
use crate::apcie::{ApcieProbe, Window};
use core::fmt::Write as _;
use stela_contracts::interrupt::AicInput;
use stela_soc_data::board::apcie_probe::Apcie;

impl AppleDevices {
    /// Back every `reg` window of root complex `apcie` with one logging
    /// stub, and own its power resource (its first window's PA). Windows
    /// overlap (J473AP's PHY window holds the PHY IP and the ports' PHY
    /// glue), so each run of overlapping windows is placed whole at device
    /// offsets equal to its guest PAs, and the stub names an access by the
    /// smallest window holding it. A run another model already owns (the
    /// PMGR eFuses, `reg[5]`, are the always-on fuse block's) falls back to
    /// its free windows one by one; an untranslated window stays as it is.
    /// With the root-complex model, `dart` is the modeled port's DART (its
    /// `function-dart_self`), through which its endpoint's DMA translates,
    /// and the complex's MSI vectors become edge inputs of the AIC.
    /// Returns the `APCIE PROBE` description of what the guest is shown.
    pub fn add_apcie_probe(&mut self, apcie: &Apcie, dart: Option<DartId>) -> Result<String> {
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
        let mut skipped = String::new();
        let mut windows = Vec::new();
        for window in &apcie.windows {
            let name = Window::of(window.index);
            match window.physical {
                Some(physical)
                    if window.bytes != 0 && physical.checked_add(window.bytes).is_some() =>
                {
                    windows.push((window.index, physical, window.bytes));
                }
                _ => {
                    let _ = write!(skipped, " {name}(untranslated)");
                }
            }
        }
        let mut probe = ApcieProbe::new(&apcie.name, apcie.first_bus, &ports, &windows);
        // The root-complex model's port (`--apcie`): its interrupt is the
        // complex's `interrupts[port]`, its endpoint's BAR decodes in the
        // complex's PCI memory windows.
        let memory: Vec<(u64, u64, u64)> = apcie
            .memory
            .iter()
            .filter_map(|window| {
                let physical = window.physical?;
                physical
                    .checked_add(window.bytes)
                    .map(|_| (physical, window.bytes, window.pci))
            })
            .collect();
        let mut model = String::new();
        if let Some(port) = apcie.model {
            let irq = apcie
                .interrupts
                .get(port as usize)
                .and_then(|&irq| u16::try_from(irq).ok())
                .map(|irq| AicInput::new(0, irq));
            probe = probe.with_model(port as usize, irq, &memory);
            let mut msi = String::from("none");
            if let Some(vectors) = apcie.msi
                && probe.modeled_port().is_some()
            {
                probe = probe.with_msi(vectors.offset);
                let edge = u16::try_from(vectors.offset)
                    .ok()
                    .zip(u16::try_from(vectors.vectors).ok())
                    .ok_or_else(|| String::from("vectors outside the AIC"))
                    .and_then(|(first, count)| {
                        self.set_edge_inputs(AicInput::new(0, first), count)
                    });
                msi = format!(
                    "aic {}+{} doorbell {:#x}{}",
                    vectors.offset,
                    vectors.vectors,
                    vectors.address,
                    match edge {
                        Ok(()) => String::new(),
                        Err(why) => format!(" (not edge: {why})"),
                    }
                );
            }
            let _ = write!(
                model,
                " model=port{port}(aic {}, memory windows {}, dart {}, msi {msi}{})",
                irq.map_or(-1, |irq| i64::from(irq.irq)),
                memory.len(),
                dart.map_or_else(|| String::from("none"), |dart| format!("{:?}", dart.0)),
                if probe.modeled_port().is_some() {
                    ""
                } else {
                    "; no root port header, not modeled"
                }
            );
        }
        let modeled = probe.modeled_port().is_some();
        let device = self.bus.add(Box::new(probe));
        if modeled {
            self.apcie_masters.push((device, dart.map(|dart| dart.0)));
        }
        // Runs of overlapping windows, by start.
        let mut sorted = windows.clone();
        sorted.sort_by_key(|&(_, physical, _)| physical);
        type Run = (u64, u64, Vec<(usize, u64, u64)>);
        let mut runs: Vec<Run> = Vec::new();
        for window in sorted {
            let (_, physical, bytes) = window;
            match runs.last_mut() {
                Some((_, end, members)) if physical < *end => {
                    *end = (*end).max(physical + bytes);
                    members.push(window);
                }
                _ => runs.push((physical, physical + bytes, vec![window])),
            }
        }
        let mut ranges = String::new();
        let mut place = |start: u64, bytes: u64| {
            self.bus
                .check_free(start, bytes)
                .and_then(|()| self.bus.map(device, start, bytes, start))
        };
        for (start, end, members) in runs {
            let names = members
                .iter()
                .map(|&(index, ..)| format!("{}", Window::of(index)))
                .collect::<Vec<_>>()
                .join("/");
            match place(start, end - start) {
                Ok(()) => {
                    let _ = write!(ranges, " {names}={start:#x}+{:#x}", end - start);
                }
                Err(error) => {
                    for (index, physical, bytes) in members {
                        let name = Window::of(index);
                        match place(physical, bytes) {
                            Ok(()) => {
                                let _ = write!(ranges, " {name}={physical:#x}+{bytes:#x}");
                            }
                            Err(own) => {
                                let _ = write!(
                                    skipped,
                                    " {name}={physical:#x}+{bytes:#x}({own:?}; run {error:?})"
                                );
                            }
                        }
                    }
                }
            }
        }
        // The endpoint's BAR space, when modeled: each memory window whole.
        // One another model owns stays unbacked; nothing reaches it before
        // a driver maps the endpoint's BAR.
        for (index, window) in apcie.memory.iter().enumerate() {
            let name = Window::Memory(index);
            let pci = window.pci;
            match window.physical {
                Some(physical) if modeled && memory.contains(&(physical, window.bytes, pci)) => {
                    match place(physical, window.bytes) {
                        Ok(()) => {
                            let _ = write!(
                                ranges,
                                " {name}={physical:#x}+{:#x}(pci {pci:#x})",
                                window.bytes
                            );
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
                Some(_) if !modeled => {}
                _ => {
                    let _ = write!(skipped, " {name}(pci {pci:#x}, untranslated)");
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
             first-bus={} interrupts=[{interrupts}] ranges=[{} ] shared=[{} ]{model}\n",
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

impl AppleDevices {
    /// Make `count` AIC inputs from `first` edge inputs (the complex's MSI
    /// vectors, `crate::aic`); why not, when it cannot.
    fn set_edge_inputs(&mut self, first: AicInput, count: u16) -> core::result::Result<(), String> {
        match self.aic {
            Some(Controller::V2(id)) => self
                .bus
                .device_mut::<Aic2>(id)
                .ok_or_else(|| String::from("the AIC id names no AICv2"))?
                .set_edge_inputs(first, count)
                .map_err(|error| format!("{error:?}")),
            Some(Controller::V1(_)) => Err(String::from("AICv1 edge inputs are not modeled")),
            None => Err(String::from("no AIC is mapped")),
        }
    }

    /// Each modeled root port's upstream step: its endpoint's DMA through
    /// the port's DART and its MSIs, when the endpoint has work. A refused
    /// transfer is the endpoint's status, never the machine's error.
    pub(super) fn transfer_apcie(
        &mut self,
        cpu: usize,
        memory: &mut dyn Memory,
        now: u64,
    ) -> core::result::Result<(), DeviceError> {
        for index in 0..self.apcie_masters.len() {
            let (probe, dart) = self.apcie_masters[index];
            if !self
                .bus
                .device::<ApcieProbe>(probe)
                .is_some_and(ApcieProbe::dma_pending)
            {
                continue;
            }
            let mut access = Access::new(cpu, now, &mut self.latch);
            match dart {
                Some(dart) => {
                    if let Some((probe, dart)) = self.bus.pair_mut::<ApcieProbe, Dart>(probe, dart)
                    {
                        probe.run_dma(Some(dart), memory, &mut access);
                    }
                }
                None => {
                    if let Some(probe) = self.bus.device_mut::<ApcieProbe>(probe) {
                        probe.run_dma(None, memory, &mut access);
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apcie::link;
    use stela_engine::machine::engine::Devices as _;
    use stela_soc_data::board::apcie_probe::{ApcieMemory, ApcieMsi, ApciePort, ApcieWindow};

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
            memory: vec![],
            model: None,
            dart: None,
            msi: None,
        };
        let line = devices.add_apcie_probe(&apcie, None).unwrap();
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

    #[test]
    fn overlapping_windows_are_placed_as_one_run() {
        let mut devices = AppleDevices::new();
        let phy = 0x6_8008_4000;
        let apcie = Apcie {
            name: "apcie".into(),
            windows: vec![
                window(2, Some(phy), 0x6_0000),
                window(3, Some(phy + 0x4_0000), 0x1_0000),
                window(8, Some(phy + 0x4000), 0x4000),
            ],
            interrupts: vec![],
            ports: vec![],
            first_bus: 0,
            memory: vec![],
            model: None,
            dart: None,
            msi: None,
        };
        let line = devices.add_apcie_probe(&apcie, None).unwrap();
        assert!(
            line.contains("phy/phy-glue0/phy-ip=0x680084000+0x60000"),
            "{line}"
        );
        devices.write(0, phy + 0x4000, 4, 1, 0).unwrap();
        assert_eq!(devices.read(0, phy + 0x4000, 4, 0), Ok(5));
    }

    #[test]
    fn the_modeled_port_trains_its_link_and_shows_an_endpoint_through_the_bus() {
        let mut devices = AppleDevices::new();
        let ecam = 0x6_9000_0000;
        let port0 = 0x6_8100_0000;
        let space = 0x6_c000_0000;
        let apcie = Apcie {
            name: "apcie".into(),
            windows: vec![
                window(0, Some(ecam), 0x1000_0000),
                window(1, Some(0x6_8000_0000), 0x4000),
                window(6, Some(port0), 0x8000),
            ],
            interrupts: vec![781],
            ports: vec![ApciePort {
                name: "pci-bridge0".into(),
                index: 0,
            }],
            first_bus: 0,
            memory: vec![ApcieMemory {
                pci: 0xc000_0000,
                physical: Some(space),
                bytes: 0x4000_0000,
                prefetchable: false,
            }],
            model: Some(0),
            dart: None,
            msi: Some(ApcieMsi {
                address: 0xffff_f000,
                vectors: 32,
                offset: 896,
            }),
        };
        let line = devices.add_apcie_probe(&apcie, None).unwrap();
        assert!(
            line.contains(
                "model=port0(aic 781, memory windows 1, dart none, msi aic 896+32 \
                 doorbell 0xfffff000 (not edge: no AIC is mapped))"
            ),
            "{line}"
        );
        assert!(
            line.contains("mem0=0x6c0000000+0x40000000(pci 0xc0000000)"),
            "{line}"
        );
        // Start the LTSSM: 5 ms later the link is up. (Interrupts stay
        // masked: this machine has no AIC to take the port's line.)
        devices.write(0, port0 + 0x104, 4, 0xffff_ffff, 0).unwrap();
        devices.write(0, port0 + 0x80, 4, 1, 0).unwrap();
        assert_eq!(
            devices.read(0, port0 + 0x208, 4, 1000).map(|v| v & 1),
            Ok(0)
        );
        let later = link::TRAINING_TICKS + 1;
        assert_eq!(
            devices.read(0, port0 + 0x208, 4, later).map(|v| v & 1),
            Ok(1)
        );
        assert_eq!(
            devices.read(0, port0 + 0x100, 4, later),
            Ok(u64::from(link::LINK_UP))
        );
        // Bus 1 behind the root port: the endpoint answers there.
        devices
            .write(0, ecam + 0x18, 4, 0x0001_0100, later)
            .unwrap();
        let endpoint = ecam + (1 << 20);
        assert_eq!(devices.read(0, endpoint, 4, later), Ok(0x0005_1b36));
        // The BAR, once assigned and decoded on both sides, is RAM.
        devices
            .write(0, endpoint + 0x10, 4, 0xc000_0000, later)
            .unwrap();
        devices.write(0, endpoint + 0x04, 2, 0x6, later).unwrap();
        devices
            .write(0, ecam + 0x20, 4, 0xc000_c000, later)
            .unwrap();
        devices.write(0, ecam + 0x04, 2, 0x6, later).unwrap();
        devices.write(0, space + 8, 4, 0x1234_5678, later).unwrap();
        assert_eq!(devices.read(0, space + 8, 4, later), Ok(0x1234_5678));
        assert_eq!(devices.read(0, space + 0x2000, 4, later), Ok(0xffff_ffff));
    }
}
