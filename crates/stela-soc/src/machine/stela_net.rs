//! The StelaNet adapter's placement on the board and its DMA step. With it
//! placed, the board's network calls (`network_*`, `usb.rs`) reach it
//! instead of a USB Ethernet adapter.

use super::*;
use crate::stela_net::Net;

impl AppleDevices {
    /// The StelaNet adapter with station address `mac`, its register page at
    /// `physical` and its interrupt at `irq`. One per board.
    pub fn add_stela_net(&mut self, physical: u64, mac: Mac, irq: AicInput) -> Result<(), String> {
        if self.stela_net.is_some() {
            return Err(String::from("the board has a StelaNet adapter already"));
        }
        let device = self.bus.add(Box::new(Net::new(mac.0, irq)));
        self.bus
            .map(device, physical, stela_contracts::net::window::BYTES, 0)
            .map_err(|error| format!("StelaNet placement: {error:?}"))?;
        self.stela_net = Some(device);
        Ok(())
    }

    /// The StelaNet adapter, if the board has one.
    pub(super) fn stela_net(&mut self) -> Option<&mut Net> {
        let device = self.stela_net?;
        Some(
            self.bus
                .device_mut::<Net>(device)
                .expect("the StelaNet adapter is mapped"),
        )
    }

    fn stela_net_ref(&self) -> Option<&Net> {
        let device = self.stela_net?;
        Some(
            self.bus
                .device::<Net>(device)
                .expect("the StelaNet adapter is mapped"),
        )
    }

    /// Why the StelaNet adapter last stopped itself, for the boot log.
    pub fn stela_net_error(&self) -> Option<&'static str> {
        self.stela_net_ref()?.error()
    }

    pub(super) fn stela_net_due(&self) -> bool {
        self.stela_net_ref().is_some_and(Net::due)
    }

    pub(super) fn stela_net_take_service_due(&mut self) -> bool {
        self.stela_net().is_some_and(Net::take_service_due)
    }

    /// The adapter's DMA step, in a transfer of the home machine's own, after
    /// a write that reached it, or when it has work ([`takes_step`]).
    pub(super) fn transfer_stela_net(&mut self, cpu: usize, memory: &mut dyn Memory, now: u64) {
        let Some(device) = self.stela_net else {
            return;
        };
        let written = self.written_device();
        let net = self
            .bus
            .device_mut::<Net>(device)
            .expect("the StelaNet adapter is mapped");
        if !takes_step(written, &[device], || net.due()) {
            return;
        }
        let mut access = Access::new(cpu, now, &mut self.latch);
        net.step(memory, &mut access);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stela_contracts::dma::test_support::Ram;
    use stela_contracts::net::{features, ring, window::*};

    const PAGE: u64 = 0x2_0ffe_c000;
    const RAM: u64 = 0x8_0000_0000;
    const MAC_ADDRESS: [u8; 6] = [0x02, 1, 2, 3, 4, 5];

    fn write(devices: &mut AppleDevices, register: u64, value: u64) {
        Devices::write(devices, 0, PAGE + register, 8, value, 1).unwrap();
    }

    /// The board routes the network calls to the adapter and its DMA step
    /// runs in the board transfer: a frame each way through one-entry rings.
    #[test]
    fn the_board_network_calls_reach_the_adapter() {
        let mut devices = AppleDevices::for_boot_processor(0);
        devices
            .add_stela_net(PAGE, Mac(MAC_ADDRESS), AicInput::new(0, 0x2a0))
            .unwrap();
        assert!(
            devices
                .add_stela_net(PAGE, Mac(MAC_ADDRESS), AicInput::new(0, 0x2a1))
                .is_err()
        );
        assert_eq!(devices.network_status().map(|(up, _)| up), Some(false));
        let mut ram = Ram::new(RAM, 0x4_0000);
        let (rx, tx, buffers) = (RAM, RAM + 0x1000, RAM + 0x2_0000);
        for (register, value) in [
            (ACCEPT, features::OFFERED),
            (RX_RING, rx),
            (RX_SIZE, 1),
            (TX_RING, tx),
            (TX_SIZE, 1),
            (ENABLE, 1),
        ] {
            write(&mut devices, register, value);
        }
        assert_eq!(devices.network_status().map(|(up, _)| up), Some(true));
        // The first step checks the rings.
        assert_eq!(devices.dma_deadline(), Some(0));
        Devices::transfer(&mut devices, 0, &mut ram, 2).unwrap();
        assert_eq!(devices.dma_deadline(), None);

        // Transmit: a 64-byte frame after its header, then a kick.
        let mut sent = vec![0x5au8; 12 + 64];
        sent[12..18].copy_from_slice(&[0xff; 6]);
        ram.write(buffers, &sent);
        let descriptor = [
            buffers.to_le_bytes().as_slice(),
            &76u32.to_le_bytes(),
            &[0; 4],
        ]
        .concat();
        ram.write(tx, &descriptor);
        ram.write(tx + ring::avail(1) + 2, &1u16.to_le_bytes());
        // No interrupt asked for (`used_event` far ahead), either way: this
        // board has no AIC to take it.
        ram.write(tx + ring::avail(1) + 6, &0x8000u16.to_le_bytes());
        write(&mut devices, KICK, KICK_TX);
        // The engine settles the kick's store with a transfer
        // (`settle_write`); the kick also makes the step due on its own.
        assert_eq!(devices.dma_deadline(), Some(0));
        Devices::transfer(&mut devices, 0, &mut ram, 2).unwrap();
        assert!(devices.take_host_service());
        assert!(!devices.take_host_service());
        assert_eq!(devices.network_take().as_deref(), Some(&sent[12..]));

        // Receive: a buffer posted, a frame from the host.
        let descriptor = [
            (buffers + 0x1000).to_le_bytes().as_slice(),
            &2048u32.to_le_bytes(),
            &ring::DESC_F_WRITE.to_le_bytes(),
            &[0; 2],
        ]
        .concat();
        ram.write(rx, &descriptor);
        ram.write(rx + ring::avail(1) + 2, &1u16.to_le_bytes());
        ram.write(rx + ring::avail(1) + 6, &0x8000u16.to_le_bytes());
        assert!(devices.network_room());
        let mut frame = vec![0x33u8; 60];
        frame[..6].copy_from_slice(&MAC_ADDRESS);
        devices.network_deliver_owned(frame.clone()).unwrap();
        assert_eq!(devices.dma_deadline(), Some(0));
        Devices::transfer(&mut devices, 0, &mut ram, 3).unwrap();
        let mut landed = vec![0u8; 60];
        ram.read(buffers + 0x1000 + 12, &mut landed);
        assert_eq!(landed, frame);
        let (_, counters) = devices.network_status().unwrap();
        assert_eq!((counters.to_host, counters.to_guest), (1, 1));
        assert_eq!(devices.stela_net_error(), None);
    }
}
