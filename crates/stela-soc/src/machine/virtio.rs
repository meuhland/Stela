//! The virtio-mmio devices' placement on the board and their DMA step, as
//! StelaNet's (`machine::stela_net`): any number, each its own register
//! page and AIC input.

use super::*;
use crate::virtio::{
    VirtioDevice,
    mmio::{Event, Mmio, Stats},
};
use core::sync::atomic::AtomicBool;

/// One virtio device's state for the boot log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtioReport {
    /// Its name, such as `virtio-rng`.
    pub name: &'static str,
    pub status: u32,
    pub accepted: u64,
    pub stats: Stats,
    /// Whether `stats` changed since the last report that asked.
    pub fresh: bool,
    pub error: Option<&'static str>,
    /// What the driver did since the last report, and how many more
    /// events there were.
    pub events: Vec<Event>,
    pub dropped_events: u64,
    /// The device's own lines since the last report, such as what the
    /// guest wrote to its console.
    pub lines: Vec<String>,
    /// The device's own notes since the last report, such as the sound
    /// stream's state changes, logged as they are.
    pub notes: Vec<String>,
    /// The device's own counters, by name.
    pub counters: Vec<(&'static str, u64)>,
}

impl AppleDevices {
    /// A virtio device on the virtio-mmio transport, its register page at
    /// `physical` and its interrupt at `irq`. `notice`, if given, is set
    /// whenever the device has events for [`Self::virtio_reports`].
    pub fn add_virtio_mmio(
        &mut self,
        physical: u64,
        irq: AicInput,
        device: Box<dyn VirtioDevice>,
        notice: Option<&'static AtomicBool>,
    ) -> Result<(), String> {
        let name = device.name();
        let model = self.bus.add(Box::new(Mmio::new(device, irq, notice)));
        self.bus
            .map(model, physical, stela_contracts::virtio::mmio::BYTES, 0)
            .map_err(|error| format!("{name} placement: {error:?}"))?;
        self.virtio.push(model);
        Ok(())
    }

    fn virtio_model(&self, device: DeviceId) -> &Mmio {
        self.bus
            .device::<Mmio>(device)
            .expect("a virtio device is mapped")
    }

    /// Each virtio device's state, with its events since the last call;
    /// with `counters`, whether its stats changed since the last such call.
    pub fn virtio_reports(&mut self, counters: bool) -> Vec<VirtioReport> {
        let devices = self.virtio.clone();
        devices
            .into_iter()
            .map(|device| {
                let model = self
                    .bus
                    .device_mut::<Mmio>(device)
                    .expect("a virtio device is mapped");
                let (events, dropped_events) = model.take_events();
                VirtioReport {
                    name: model.device_name(),
                    status: model.status(),
                    accepted: model.accepted(),
                    stats: model.stats(),
                    fresh: counters && model.stats_changed(),
                    error: model.error(),
                    events,
                    dropped_events,
                    lines: model.take_lines(),
                    notes: model.take_notes(),
                    counters: model.device_counters(),
                }
            })
            .collect()
    }

    /// Whether the board has virtio devices.
    pub fn has_virtio(&self) -> bool {
        !self.virtio.is_empty()
    }

    /// The guest counter tick a virtio device's DMA step is next due at:
    /// now for buffers to move, or a device's own timed work.
    pub(super) fn virtio_deadline(&self) -> Option<u64> {
        self.virtio
            .iter()
            .filter_map(|&device| self.virtio_model(device).deadline())
            .min()
    }

    /// Whether a virtio device handed the host work for a service turn,
    /// such as the sound device's frames in the guest-audio ring; each
    /// device's flag is cleared by asking.
    pub(super) fn virtio_take_host_due(&mut self) -> bool {
        let mut due = false;
        for index in 0..self.virtio.len() {
            let device = self.virtio[index];
            due |= self
                .bus
                .device_mut::<Mmio>(device)
                .is_some_and(Mmio::take_host_due);
        }
        due
    }

    /// Each device's DMA step, in a transfer of the home machine's own,
    /// after a write that reached it, or when it has work ([`takes_step`]).
    pub(super) fn transfer_virtio(&mut self, cpu: usize, memory: &mut dyn Memory, now: u64) {
        if self.virtio.is_empty() {
            return;
        }
        let written = self.written_device();
        let mut index = 0;
        while let Some(&device) = self.virtio.get(index) {
            index += 1;
            let model = self
                .bus
                .device_mut::<Mmio>(device)
                .expect("a virtio device is mapped");
            if !takes_step(written, &[device], || model.due_at(now)) {
                continue;
            }
            let mut access = Access::new(cpu, now, &mut self.latch);
            model.step(memory, &mut access);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::console::{Console, Silent};
    use crate::virtio::rng::Rng;
    use stela_contracts::dma::test_support::Ram;
    use stela_contracts::virtio::{features, mmio::*, ring, status};

    const PAGE: u64 = 0x2_0ffe_8000;
    const RAM: u64 = 0x8_0000_0000;

    fn write(devices: &mut AppleDevices, register: u64, value: u64) {
        Devices::write(devices, 0, PAGE + register, 4, value, 1).unwrap();
    }

    fn read(devices: &mut AppleDevices, register: u64) -> u64 {
        Devices::read(devices, 0, PAGE + register, 4, 1).unwrap()
    }

    /// The board maps the device's page and runs its step in the board
    /// transfer: a request filled through a one-entry queue.
    #[test]
    fn the_board_runs_an_entropy_device() {
        let mut devices = AppleDevices::for_boot_processor(0);
        assert!(!devices.has_virtio());
        let source = Box::new(|bytes: &mut [u8]| {
            bytes.fill(0xa5);
            true
        });
        devices
            .add_virtio_mmio(
                PAGE,
                AicInput::new(0, 0x2a2),
                Box::new(Rng::new(source)),
                None,
            )
            .unwrap();
        assert!(devices.has_virtio());
        // A second device on the same page is refused.
        assert!(
            devices
                .add_virtio_mmio(
                    PAGE,
                    AicInput::new(0, 0x2a3),
                    Box::new(Rng::new(Box::new(|_: &mut [u8]| true))),
                    None,
                )
                .is_err()
        );
        assert_eq!(read(&mut devices, MAGIC_VALUE), MAGIC);
        let mut ram = Ram::new(RAM, 0x4_0000);
        let (desc, avail, used, buffer) = (RAM, RAM + 0x1000, RAM + 0x2000, RAM + 0x1_0000);
        write(&mut devices, STATUS, 0);
        write(&mut devices, STATUS, status::ACKNOWLEDGE | status::DRIVER);
        write(&mut devices, DRIVER_FEATURES_SEL, 1);
        write(&mut devices, DRIVER_FEATURES, features::VERSION_1 >> 32);
        write(
            &mut devices,
            STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        write(&mut devices, QUEUE_SEL, 0);
        write(&mut devices, QUEUE_NUM, 1);
        for (low, address) in [
            (QUEUE_DESC_LOW, desc),
            (QUEUE_AVAIL_LOW, avail),
            (QUEUE_USED_LOW, used),
        ] {
            write(&mut devices, low, address & 0xffff_ffff);
            write(&mut devices, low + 4, address >> 32);
        }
        write(&mut devices, QUEUE_READY, 1);
        assert_eq!(read(&mut devices, QUEUE_READY), 1);
        write(
            &mut devices,
            STATUS,
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        // The first step checks the queue.
        assert_eq!(devices.dma_deadline(), Some(0));
        Devices::transfer(&mut devices, 0, &mut ram, 2).unwrap();
        assert_eq!(devices.dma_deadline(), None);

        let descriptor = [
            buffer.to_le_bytes().as_slice(),
            &16u32.to_le_bytes(),
            &ring::DESC_F_WRITE.to_le_bytes(),
            &[0; 2],
        ]
        .concat();
        ram.write(desc, &descriptor);
        ram.write(avail + 2, &1u16.to_le_bytes());
        // No interrupt asked for: this board has no AIC to take it.
        ram.write(avail, &ring::AVAIL_F_NO_INTERRUPT.to_le_bytes());
        write(&mut devices, QUEUE_NOTIFY, 0);
        assert_eq!(devices.dma_deadline(), Some(0));
        Devices::transfer(&mut devices, 0, &mut ram, 3).unwrap();
        let mut filled = [0u8; 16];
        ram.read(buffer, &mut filled);
        assert_eq!(filled, [0xa5; 16]);
        let mut index = [0u8; 2];
        ram.read(used + 2, &mut index);
        assert_eq!(u16::from_le_bytes(index), 1);

        let [report] = &devices.virtio_reports(true)[..] else {
            panic!("one device");
        };
        assert_eq!(report.name, "virtio-rng");
        assert_eq!(report.accepted, features::VERSION_1);
        assert_eq!((report.stats.chains, report.stats.bytes), (1, 16));
        assert!(report.fresh);
        assert_eq!(report.error, None);
        assert!(report.events.contains(&Event::Notified { index: 0 }));
        // The events were taken, and the stats seen.
        let again = &devices.virtio_reports(true)[0];
        assert!(again.events.is_empty() && !again.fresh);
    }

    /// Two devices, each on its own page and input, as a `--virtio` boot
    /// places the entropy device and the console: the console's output
    /// reaches its report, the other device untouched.
    #[test]
    fn two_devices_run_each_on_its_own_page() {
        const CONSOLE: u64 = PAGE + 0x4000;
        let mut devices = AppleDevices::for_boot_processor(0);
        let source = Box::new(|bytes: &mut [u8]| {
            bytes.fill(0);
            true
        });
        devices
            .add_virtio_mmio(
                PAGE,
                AicInput::new(0, 0x2a2),
                Box::new(Rng::new(source)),
                None,
            )
            .unwrap();
        devices
            .add_virtio_mmio(
                CONSOLE,
                AicInput::new(0, 0x2a3),
                Box::new(Console::new(Box::new(Silent))),
                None,
            )
            .unwrap();
        let console = |register| CONSOLE - PAGE + register;
        assert_eq!(
            read(&mut devices, console(DEVICE_ID)),
            stela_contracts::virtio::ids::CONSOLE
        );
        assert_eq!(
            read(&mut devices, DEVICE_ID),
            stela_contracts::virtio::ids::RNG
        );
        let mut ram = Ram::new(RAM, 0x4_0000);
        let (desc, avail, used, buffer) = (RAM, RAM + 0x1000, RAM + 0x2000, RAM + 0x1_0000);
        write(&mut devices, console(STATUS), 0);
        write(
            &mut devices,
            console(STATUS),
            status::ACKNOWLEDGE | status::DRIVER,
        );
        write(&mut devices, console(DRIVER_FEATURES_SEL), 1);
        write(
            &mut devices,
            console(DRIVER_FEATURES),
            features::VERSION_1 >> 32,
        );
        write(
            &mut devices,
            console(STATUS),
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
        );
        // Only the transmit queue: the guest writes, and reads nothing.
        write(&mut devices, console(QUEUE_SEL), 1);
        write(&mut devices, console(QUEUE_NUM), 1);
        for (low, address) in [
            (QUEUE_DESC_LOW, desc),
            (QUEUE_AVAIL_LOW, avail),
            (QUEUE_USED_LOW, used),
        ] {
            write(&mut devices, console(low), address & 0xffff_ffff);
            write(&mut devices, console(low + 4), address >> 32);
        }
        write(&mut devices, console(QUEUE_READY), 1);
        write(
            &mut devices,
            console(STATUS),
            status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
        );
        Devices::transfer(&mut devices, 0, &mut ram, 2).unwrap();
        ram.write(buffer, b"hi\n");
        let descriptor = [
            buffer.to_le_bytes().as_slice(),
            &3u32.to_le_bytes(),
            &[0; 4],
        ]
        .concat();
        ram.write(desc, &descriptor);
        ram.write(avail, &ring::AVAIL_F_NO_INTERRUPT.to_le_bytes());
        ram.write(avail + 2, &1u16.to_le_bytes());
        write(&mut devices, console(QUEUE_NOTIFY), 1);
        Devices::transfer(&mut devices, 0, &mut ram, 3).unwrap();
        let reports = devices.virtio_reports(true);
        let [rng, console] = &reports[..] else {
            panic!("two devices");
        };
        assert_eq!((rng.name, rng.status), ("virtio-rng", 0));
        assert!(rng.lines.is_empty() && rng.counters.is_empty());
        assert_eq!(console.name, "virtio-console");
        assert_eq!(console.lines, ["hi"]);
        assert!(console.counters.contains(&("tx_lines", 1)));
        assert_eq!(console.stats.chains, 1);
        // The lines were taken.
        assert!(devices.virtio_reports(false)[1].lines.is_empty());
    }
}
