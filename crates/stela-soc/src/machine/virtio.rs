//! The virtio-mmio devices' placement on the board and their DMA step, as
//! StelaNet's (`machine::stela_net`): any number, each its own register
//! page and AIC input.
//!
//! A network device among them is the guest's second network interface on
//! the one host link the board's network calls (`network_*`, `usb.rs`)
//! carry, beside StelaNet or the USB Ethernet adapter: [`route`] switches
//! each frame from the host by its destination, as a learning switch with
//! two known stations would, and what either interface sends goes out on
//! the host link.

use super::*;
use crate::virtio::{
    VirtioDevice,
    mmio::{Event, Mmio, Stats},
    net::VirtioNet,
};
use core::sync::atomic::AtomicBool;

/// Which of the guest's two network interfaces a frame from the host is
/// for: the first (StelaNet or the USB Ethernet adapter), the second (the
/// virtio network device), or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    First,
    Second,
    Both,
}

/// Where a frame from the host goes when the second interface has station
/// address `second`: a unicast frame to that address to it alone, every
/// other unicast frame to the first (whose filter keeps those for its own
/// address, the host adapter's), and broadcast and multicast frames to
/// both. A frame too short for a destination is the first's to drop.
pub fn route(frame: &[u8], second: [u8; 6]) -> Route {
    let Some(destination) = frame.get(..6) else {
        return Route::First;
    };
    if destination[0] & 1 != 0 {
        Route::Both
    } else if destination == second {
        Route::Second
    } else {
        Route::First
    }
}

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
        let net = device.net_ref().is_some();
        if net && self.virtio_net.is_some() {
            return Err(String::from(
                "the board has a virtio network device already",
            ));
        }
        let model = self.bus.add(Box::new(Mmio::new(device, irq, notice)));
        self.bus
            .map(model, physical, stela_contracts::virtio::mmio::BYTES, 0)
            .map_err(|error| format!("{name} placement: {error:?}"))?;
        self.virtio.push(model);
        if net {
            self.virtio_net = Some(model);
        }
        Ok(())
    }

    /// The guest's second network interface, a virtio network device at
    /// `physical` with its interrupt at `irq`, offering StelaNet's
    /// `offloads` it can (`virtio::net::OFFLOADS`). Its station address
    /// derives from the board's ECID and is never `avoid`, the first
    /// interface's; the address is the answer. One per board.
    pub fn add_virtio_net(
        &mut self,
        physical: u64,
        irq: AicInput,
        offloads: u64,
        avoid: Option<[u8; 6]>,
        notice: Option<&'static AtomicBool>,
    ) -> Result<[u8; 6], String> {
        let mac = crate::virtio::net::station_address(self.ecid, avoid);
        self.add_virtio_mmio(
            physical,
            irq,
            Box::new(VirtioNet::new(mac, offloads)),
            notice,
        )?;
        Ok(mac)
    }

    /// The virtio network device, if the board has one.
    pub(super) fn virtio_net(&mut self) -> Option<&mut VirtioNet> {
        let device = self.virtio_net?;
        self.bus
            .device_mut::<Mmio>(device)
            .expect("the virtio network device is mapped")
            .device_mut()
            .net()
    }

    fn virtio_net_ref(&self) -> Option<&VirtioNet> {
        self.virtio_model(self.virtio_net?).device().net_ref()
    }

    /// Whether the board has a virtio network device.
    pub fn has_virtio_net(&self) -> bool {
        self.virtio_net.is_some()
    }

    /// Which interface a frame from the host is for ([`route`]): the first
    /// alone without a virtio network device.
    pub(super) fn virtio_net_route(&self, frame: &[u8]) -> Route {
        self.virtio_net_ref()
            .map_or(Route::First, |net| route(frame, net.mac()))
    }

    /// The host adapter's carrier, for the virtio network device's `STATUS`.
    pub fn virtio_net_set_link(&mut self, up: bool) {
        if let Some(net) = self.virtio_net() {
            net.set_link(up);
        }
    }

    pub(super) fn virtio_net_take_service_due(&mut self) -> bool {
        self.virtio_net().is_some_and(VirtioNet::take_service_due)
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

    #[test]
    fn frames_route_by_destination_with_group_frames_to_both() {
        let second = [0x02, 0x53, 1, 2, 3, 4];
        let to = |destination: [u8; 6]| {
            let mut frame = vec![0u8; 60];
            frame[..6].copy_from_slice(&destination);
            route(&frame, second)
        };
        assert_eq!(to(second), Route::Second);
        assert_eq!(to([0xbc, 0x24, 0x11, 0xbc, 0xf8, 0x0b]), Route::First);
        assert_eq!(to([0x02, 0x53, 1, 2, 3, 5]), Route::First);
        assert_eq!(to([0xff; 6]), Route::Both);
        assert_eq!(to([0x01, 0x00, 0x5e, 0, 0, 0xfb]), Route::Both);
        assert_eq!(to([0x33, 0x33, 0, 0, 0, 1]), Route::Both);
        assert_eq!(route(&[0x02, 0x53], second), Route::First, "no destination");
    }

    mod two_interfaces {
        use super::*;
        use stela_contracts::net;

        const NET_PAGE: u64 = 0x2_0ffe_c000;
        const VIRTIO_PAGE: u64 = 0x2_0ffe_8000;
        /// StelaNet's address: the host adapter's.
        const FIRST: [u8; 6] = [0xbc, 0x24, 0x11, 0xbc, 0xf8, 0x0b];
        const SIZE: u64 = 8;
        /// StelaNet's rings, each one block.
        const NET_RX: u64 = RAM;
        const NET_TX: u64 = RAM + 0x1000;
        /// The virtio device's queues: descriptors, available and used.
        const VIRTIO_RX: [u64; 3] = [RAM + 0x2000, RAM + 0x2800, RAM + 0x2c00];
        const VIRTIO_TX: [u64; 3] = [RAM + 0x3000, RAM + 0x3800, RAM + 0x3c00];
        const BUFFERS: u64 = RAM + 0x1_0000;

        fn write64(devices: &mut AppleDevices, register: u64, value: u64) {
            Devices::write(devices, 0, NET_PAGE + register, 8, value, 1).unwrap();
        }

        fn write32(devices: &mut AppleDevices, register: u64, value: u64) {
            Devices::write(devices, 0, VIRTIO_PAGE + register, 4, value, 1).unwrap();
        }

        /// Makes `chains` (address, bytes) available on the ring whose
        /// descriptors are at `desc` and available ring at `avail`, each
        /// one buffer, the device `write`ing them or not, with no
        /// interrupt wanted (this board has no AIC to take one).
        fn offer(ram: &mut Ram, desc: u64, avail: u64, chains: &[(u64, u32)], write: bool) {
            let flags = if write { ring::DESC_F_WRITE } else { 0 };
            for (index, &(address, bytes)) in chains.iter().enumerate() {
                let entry = [
                    address.to_le_bytes().as_slice(),
                    &bytes.to_le_bytes(),
                    &flags.to_le_bytes(),
                    &[0; 2],
                ]
                .concat();
                assert!(ram.write(desc + 16 * index as u64, &entry));
                assert!(ram.write(avail + 4 + 2 * index as u64, &(index as u16).to_le_bytes()));
            }
            assert!(ram.write(avail + 2, &(chains.len() as u16).to_le_bytes()));
            assert!(ram.write(avail + 4 + 2 * SIZE, &0x8000u16.to_le_bytes()));
        }

        fn frame(to: [u8; 6], fill: u8) -> Vec<u8> {
            let mut frame = vec![fill; 60];
            frame[..6].copy_from_slice(&to);
            frame
        }

        /// A board with StelaNet and a virtio network device, each running
        /// on rings in `ram` with every receive buffer posted.
        fn board(ram: &mut Ram) -> (AppleDevices, [u8; 6]) {
            let mut devices = AppleDevices::for_boot_processor(0);
            devices
                .add_stela_net(NET_PAGE, Mac(FIRST), AicInput::new(0, 0x2a0), 0)
                .unwrap();
            let second = devices
                .add_virtio_net(VIRTIO_PAGE, AicInput::new(0, 0x2a1), 0, Some(FIRST), None)
                .unwrap();
            assert_ne!(second, FIRST);
            assert!(devices.has_virtio_net());
            assert!(
                devices
                    .add_virtio_net(0x2_0ffe_4000, AicInput::new(0, 0x2a2), 0, None, None)
                    .is_err(),
                "one per board"
            );
            for (register, value) in [
                (net::window::ACCEPT, net::features::OFFERED),
                (net::window::RX_RING, NET_RX),
                (net::window::RX_SIZE, SIZE),
                (net::window::TX_RING, NET_TX),
                (net::window::TX_SIZE, SIZE),
                (net::window::ENABLE, 1),
            ] {
                write64(&mut devices, register, value);
            }
            write32(&mut devices, STATUS, status::ACKNOWLEDGE | status::DRIVER);
            write32(&mut devices, DRIVER_FEATURES_SEL, 1);
            write32(&mut devices, DRIVER_FEATURES, features::VERSION_1 >> 32);
            write32(&mut devices, DRIVER_FEATURES_SEL, 0);
            write32(&mut devices, DRIVER_FEATURES, features::EVENT_IDX);
            write32(
                &mut devices,
                STATUS,
                status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK,
            );
            for (queue, areas) in [VIRTIO_RX, VIRTIO_TX].into_iter().enumerate() {
                write32(&mut devices, QUEUE_SEL, queue as u64);
                write32(&mut devices, QUEUE_NUM, SIZE);
                for (low, address) in [QUEUE_DESC_LOW, QUEUE_AVAIL_LOW, QUEUE_USED_LOW]
                    .into_iter()
                    .zip(areas)
                {
                    write32(&mut devices, low, address & 0xffff_ffff);
                    write32(&mut devices, low + 4, address >> 32);
                }
                write32(&mut devices, QUEUE_READY, 1);
            }
            write32(
                &mut devices,
                STATUS,
                status::ACKNOWLEDGE | status::DRIVER | status::FEATURES_OK | status::DRIVER_OK,
            );
            let buffers = |first: u64| -> Vec<(u64, u32)> {
                (0..SIZE).map(|slot| (first + 0x800 * slot, 2048)).collect()
            };
            offer(
                ram,
                NET_RX,
                NET_RX + net::ring::avail(SIZE),
                &buffers(BUFFERS),
                true,
            );
            offer(
                ram,
                VIRTIO_RX[0],
                VIRTIO_RX[1],
                &buffers(BUFFERS + 0x8000),
                true,
            );
            write32(&mut devices, QUEUE_NOTIFY, 0);
            Devices::transfer(&mut devices, 0, ram, 2).unwrap();
            assert_eq!(devices.dma_deadline(), None);
            (devices, second)
        }

        fn received(devices: &mut AppleDevices) -> (u64, u64) {
            let first = devices.network_status().unwrap().1.to_guest;
            let second = devices.virtio_net().unwrap().counters().to_guest;
            (first, second)
        }

        /// Unicast by address, broadcast and multicast to both, and the
        /// first interface's frames as they were without the second.
        #[test]
        fn each_interface_gets_its_own_frames_and_both_the_group_frames() {
            let mut ram = Ram::new(RAM, 0x2_0000);
            let (mut devices, second) = board(&mut ram);
            for (to, routed) in [
                (FIRST, (1, 0)),
                (second, (1, 1)),
                ([0xff; 6], (2, 2)),
                ([0x01, 0x00, 0x5e, 0, 0, 0xfb], (3, 3)),
                ([0x33, 0x33, 0, 0, 0, 1], (4, 4)),
                // Another station's unicast: the first's filter drops it.
                ([0x02, 0, 0, 0, 0, 9], (4, 4)),
            ] {
                assert!(devices.network_room());
                devices.network_deliver_owned(frame(to, 0x33)).unwrap();
                Devices::transfer(&mut devices, 0, &mut ram, 3).unwrap();
                assert_eq!(received(&mut devices), routed, "{to:02x?}");
            }
            assert_eq!(devices.network_status().unwrap().1.filtered, 1);
            assert_eq!(devices.virtio_net().unwrap().counters().filtered, 0);
            // The borrowed form routes the same way.
            devices.network_deliver(&frame(second, 0x44)).unwrap();
            Devices::transfer(&mut devices, 0, &mut ram, 4).unwrap();
            assert_eq!(received(&mut devices), (4, 5));
            // The second interface's frame landed after its header.
            let mut landed = vec![0u8; 60];
            assert!(ram.read(BUFFERS + 0x8000 + 0x800 * 4 + 12, &mut landed));
            assert_eq!(landed, frame(second, 0x44));
        }

        /// What either interface sends goes out on the host link: the
        /// first's frames, then the second's.
        #[test]
        fn both_interfaces_send_on_the_host_link() {
            let mut ram = Ram::new(RAM, 0x2_0000);
            let (mut devices, second) = board(&mut ram);
            let header = [0u8; 12];
            let mut sent = Vec::new();
            for (index, from) in [FIRST, second].into_iter().enumerate() {
                let mut buffer = header.to_vec();
                let mut body = frame([0xff; 6], index as u8);
                body[6..12].copy_from_slice(&from);
                buffer.extend_from_slice(&body);
                sent.push(body);
                let address = BUFFERS + 0x1_0000 - 0x800 * (index as u64 + 1);
                assert!(ram.write(address, &buffer));
                let chain = [(address, buffer.len() as u32)];
                if index == 0 {
                    offer(
                        &mut ram,
                        NET_TX,
                        NET_TX + net::ring::avail(SIZE),
                        &chain,
                        false,
                    );
                    write64(&mut devices, net::window::KICK, net::window::KICK_TX);
                } else {
                    offer(&mut ram, VIRTIO_TX[0], VIRTIO_TX[1], &chain, false);
                    write32(&mut devices, QUEUE_NOTIFY, 1);
                }
            }
            Devices::transfer(&mut devices, 0, &mut ram, 3).unwrap();
            assert!(devices.take_host_service());
            assert_eq!(devices.network_take(), Some(sent[0].clone()));
            assert_eq!(devices.network_take(), Some(sent[1].clone()));
            assert_eq!(devices.network_take(), None);
            // The link reaches the second interface's STATUS.
            devices.virtio_net_set_link(false);
            assert!(!devices.virtio_net().unwrap().link());
            let reports = devices.virtio_reports(true);
            let counters = &reports[0].counters;
            assert!(
                counters.contains(&("link_up", 0)) && counters.contains(&("to_host", 1)),
                "{counters:?}"
            );
        }
    }
}
