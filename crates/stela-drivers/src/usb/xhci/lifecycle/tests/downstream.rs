// SPDX-License-Identifier: GPL-2.0-only
//! Real command/context/port model checks; source-only cut, UNRUN.
use super::super::*;
use crate::bus::dma::TestRegion;
use alloc::collections::BTreeMap;
use hub::model::Node;

#[test]
fn exclusive_downstream_leaf_reset_preserves_its_parent_and_sibling() {
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        for context in [32, 64] {
            let mut dma = TestRegion::new(512 * 1024 * 8);
            let nodes = BTreeMap::from([
                (0, Node::hub(speed, 2)),
                (1, Node::leaf(speed)),
                (2, Node::leaf(speed)),
            ]);
            let (mut controller, hardware) = hub::tests::controller(&mut dma, context, nodes);
            let mut failures = Vec::new();
            let devices = controller.enumerate(&mut dma, &mut failures);
            assert!(failures.is_empty(), "{failures:?}");
            let old = devices.iter().find(|d| d.location.ends_with(".1")).unwrap();
            let sibling = devices.iter().find(|d| d.location.ends_with(".2")).unwrap();
            let parent = devices
                .iter()
                .find(|d| d.descriptor[4] == HUB_CLASS)
                .unwrap();
            let endpoints = interfaces(&old.configuration)[0].endpoints.clone();
            controller
                .set_configuration(old.slot, old.configuration_value())
                .unwrap();
            controller
                .configure(old.slot, &endpoints, None, &mut dma)
                .unwrap();
            let sibling_output = controller.slot(sibling.slot).unwrap()._output.physical();
            let before = hardware.borrow().nodes[&0].ports.as_ref().unwrap()[1].reset_count;
            let requests = hardware.borrow().requests.len();
            // SAFETY: no class owner was bound; endpoints are the full
            // configured DCI inventory of this one downstream leaf.
            let fresh =
                unsafe { controller.reset_and_identify(old, &endpoints, &mut dma) }.unwrap();
            assert_ne!(fresh.id, old.id);
            assert_eq!(fresh.location, old.location);
            assert_eq!(fresh.speed, old.speed);
            let health: Vec<_> = hardware.borrow().requests[requests..]
                .iter()
                .filter(|(_, _, request)| request[1] == GET_STATUS)
                .map(|(slot, _, request)| (*slot, request[0]))
                .collect();
            assert!(health.contains(&(parent.slot, 0x80)));
            assert!(health.contains(&(parent.slot, 0xa0)));
            assert!(health.iter().all(|(slot, _)| *slot != old.slot));
            assert!(controller.device(parent.id).is_ok());
            assert!(controller.device(sibling.id).is_ok());
            assert_eq!(
                controller.slot(sibling.slot).unwrap()._output.physical(),
                sibling_output
            );
            assert_eq!(
                hardware.borrow().nodes[&0].ports.as_ref().unwrap()[1].reset_count,
                before
            );
            let before = (hardware.borrow().mmio_reads, hardware.borrow().mmio_writes);
            assert!(controller.device(old.id).is_err());
            assert_eq!(
                (hardware.borrow().mmio_reads, hardware.borrow().mmio_writes),
                before
            );
        }
    }
}

#[test]
fn downstream_reset_failure_never_addresses_a_replacement() {
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        let mut dma = TestRegion::new(512 * 1024 * 8);
        let (mut controller, hardware) = hub::tests::controller(
            &mut dma,
            64,
            BTreeMap::from([(0, Node::hub(speed, 1)), (1, Node::leaf(speed))]),
        );
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let old = devices
            .iter()
            .find(|d| d.descriptor[4] != HUB_CLASS)
            .unwrap();
        let endpoints = interfaces(&old.configuration)[0].endpoints.clone();
        controller
            .set_configuration(old.slot, old.configuration_value())
            .unwrap();
        controller
            .configure(old.slot, &endpoints, None, &mut dma)
            .unwrap();
        hardware.borrow_mut().fail = Some((STOP_ENDPOINT, old.slot));
        let before = hardware.borrow().commands.len();
        // SAFETY: no class owner exists and the complete endpoint set is held.
        let error =
            unsafe { controller.reset_and_identify(old, &endpoints, &mut dma) }.unwrap_err();
        assert_eq!(error.0, "retire every endpoint before Disable Slot");
        assert!(controller.device(old.id).is_err());
        assert!(
            hardware.borrow().commands[before..]
                .iter()
                .all(|(kind, _)| !matches!(*kind, ADDRESS_DEVICE | reset::DISABLE_SLOT))
        );
    }
}

#[test]
fn downstream_reset_reobserves_root_and_hub_power_before_endpoint_retirement() {
    for root_lost in [false, true] {
        let mut dma = TestRegion::new(512 * 1024 * 8);
        let (mut controller, hardware) = hub::tests::controller(
            &mut dma,
            64,
            BTreeMap::from([(0, Node::hub(HIGH_SPEED, 1)), (1, Node::leaf(HIGH_SPEED))]),
        );
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let old = devices
            .iter()
            .find(|d| d.descriptor[4] != HUB_CLASS)
            .unwrap();
        let endpoints = interfaces(&old.configuration)[0].endpoints.clone();
        controller
            .set_configuration(old.slot, old.configuration_value())
            .unwrap();
        controller
            .configure(old.slot, &endpoints, None, &mut dma)
            .unwrap();
        let output = controller.slot(old.slot).unwrap()._output.physical();
        if root_lost {
            hardware.borrow_mut().root_status = Some(CCS | PED | u32::from(HIGH_SPEED) << 10);
        } else {
            // USB hub class status bit0 reports loss of the local supply.
            hardware.borrow_mut().nodes.get_mut(&0).unwrap().hub_status = 1;
        }
        let commands = hardware.borrow().commands.len();
        // SAFETY: this is the exclusive complete leaf endpoint inventory.
        let error =
            unsafe { controller.reset_and_identify(old, &endpoints, &mut dma) }.unwrap_err();
        assert_eq!(error.0, "observe reset device physical ancestry");
        assert_eq!(hardware.borrow().commands.len(), commands);
        assert!(controller.device(old.id).is_err());
        assert_eq!(
            controller.slot(old.slot).unwrap()._output.physical(),
            output
        );
        assert_eq!(
            controller.slot_ownership[usize::from(old.slot)],
            reset::Ownership::Enabled
        );
        assert!(controller.power.retained(old.slot));
    }
}

/// A stick behind a hub that hangs after its configuration: its first
/// command never completes. Its owner resets the hub port at once, reusing
/// the old binding's storage as on a root port, and the stick is found on
/// the same port.
#[test]
fn a_hung_stick_behind_a_hub_is_reset_in_place_and_found() {
    use crate::block::BlockDevice;
    let _clock = crate::test_clock::virtualize();
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (mut controller, hardware) = hub::tests::controller(
            &mut dma,
            64,
            BTreeMap::from([(0, Node::hub(speed, 1)), (1, Node::bot_disk(speed))]),
        );
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let stick = devices
            .iter()
            .find(|d| d.descriptor[4] != HUB_CLASS)
            .unwrap()
            .clone();
        let shared = Rc::new(RefCell::new(controller));
        hardware.borrow_mut().nodes.get_mut(&1).unwrap().wedge_bot();
        let resets = hardware.borrow().nodes[&0].ports.as_ref().unwrap()[0].reset_count;
        let mut found = Found::default();
        bind::bind(&shared, stick.clone(), &mut dma, &mut found);
        // The first INQUIRY timed out; the stick came back after one hub
        // port reset and was found on the same port.
        assert!(!hardware.borrow().nodes[&1].bot_wedged());
        assert_eq!(
            hardware.borrow().nodes[&0].ports.as_ref().unwrap()[0].reset_count,
            resets + 1
        );
        assert!(
            found.failures.iter().all(|(_, error)| *error
                != BlockError::Unsupported("xHCI device generation is permanently retired")),
            "{:?}",
            found.failures
        );
        assert_eq!(found.disks.len(), 1, "{:?}", found.failures);
        assert_eq!(found.storage.len(), 1);
        assert!(shared.borrow_mut().device(stick.id).is_err());
        // The timed-out command's line names the hub's root port as the
        // transfer failed.
        let (timed_out, _) = found
            .failures
            .iter()
            .find(|(_, error)| *error == BlockError::Timeout)
            .unwrap();
        assert!(
            timed_out.contains("LUN 0 root port 1 PORTSC=") && timed_out.ends_with("route=0x1"),
            "{timed_out}"
        );
        let payload = [0x3c; 512];
        found.disks[0].write(4, &payload).unwrap();
        let mut read = [0; 512];
        found.disks[0].read(4, &mut read).unwrap();
        assert_eq!(read, payload);
    }
}

/// The same hung stick on a root port: its port reset reuses the old
/// binding's storage and runs at once from the storage owner. A device
/// whose firmware hung may drop its USB 3 link to SS.Inactive, showing a
/// connection or not, where a hot reset cannot retrain it: as Linux's
/// `hub_port_reset`, the reset is then a warm one, where the in-place reset
/// refused the port and the stick was lost for the boot.
#[test]
fn a_hung_stick_on_a_root_port_is_reset_in_place_and_found() {
    use crate::block::BlockDevice;
    let _clock = crate::test_clock::virtualize();
    for (speed, link) in [
        (HIGH_SPEED, None),
        (SUPER_SPEED, None),
        (SUPER_SPEED, Some(true)),
        (SUPER_SPEED, Some(false)),
    ] {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (mut controller, hardware) =
            hub::tests::controller(&mut dma, 64, BTreeMap::from([(0, Node::bot_disk(speed))]));
        hardware.borrow_mut().root_status =
            Some(PP | CCS | PED | u32::from(speed) << PORT_SPEED_SHIFT);
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let stick = devices[0].clone();
        let shared = Rc::new(RefCell::new(controller));
        hardware.borrow_mut().nodes.get_mut(&0).unwrap().wedge_bot();
        hardware.borrow_mut().wedge_drops_root_link = link;
        let mut found = Found::default();
        bind::bind(&shared, stick.clone(), &mut dma, &mut found);
        assert!(!hardware.borrow().nodes[&0].bot_wedged(), "{link:?}");
        assert_eq!(found.disks.len(), 1, "{link:?} {:?}", found.failures);
        assert_eq!(hardware.borrow().root_status.unwrap() & PLS, 0);
        // The timed-out command's line keeps the link state it failed in.
        let (timed_out, _) = found
            .failures
            .iter()
            .find(|(_, error)| *error == BlockError::Timeout)
            .unwrap();
        let link_state = if link.is_some() { "SS.Inactive" } else { "U0" };
        assert!(
            timed_out.ends_with(&format!("link={link_state}")),
            "{timed_out}"
        );
        let payload = [0x5d; 512];
        found.disks[0].write(4, &payload).unwrap();
        let mut read = [0; 512];
        found.disks[0].read(4, &mut read).unwrap();
        assert_eq!(read, payload);
    }
}

/// A port reset that cannot bring the hung stick back is reported with the
/// step that failed and the port's status after it, where its failure was
/// dropped and only the later refusal of the retired device was logged.
#[test]
fn a_failed_port_reset_of_a_hung_stick_is_reported_with_its_port_status() {
    let _clock = crate::test_clock::virtualize();
    let mut dma = TestRegion::new(1024 * 1024 * 8);
    let (mut controller, hardware) = hub::tests::controller(
        &mut dma,
        64,
        BTreeMap::from([(0, Node::bot_disk(SUPER_SPEED))]),
    );
    hardware.borrow_mut().root_status =
        Some(PP | CCS | PED | u32::from(SUPER_SPEED) << PORT_SPEED_SHIFT);
    let mut failures = Vec::new();
    let devices = controller.enumerate(&mut dma, &mut failures);
    assert!(failures.is_empty(), "{failures:?}");
    let shared = Rc::new(RefCell::new(controller));
    hardware.borrow_mut().nodes.get_mut(&0).unwrap().wedge_bot();
    hardware.borrow_mut().wedge_drops_root_link = Some(true);
    hardware.borrow_mut().root_untrainable = true;
    let mut found = Found::default();
    bind::bind(&shared, devices[0].clone(), &mut dma, &mut found);
    assert!(found.disks.is_empty());
    let (reset, error) = found
        .failures
        .iter()
        .find(|(location, _)| location.contains("port reset after a failed command"))
        .unwrap_or_else(|| panic!("{:?}", found.failures));
    assert_eq!(
        *error,
        BlockError::Unsupported("USB root reset left the port without its device")
    );
    assert!(
        reset.contains("acknowledge reusable root reset (root port 1 PORTSC="),
        "{reset}"
    );
}

/// A warm reset may take far longer than a hot one: Linux's
/// `hub_port_wait_reset` waits `HUB_RESET_TIMEOUT`, 800 ms, for either. A
/// hung stick whose warm reset ends after 600 ms comes back.
#[test]
fn a_slow_warm_reset_of_a_hung_stick_is_waited_for() {
    let _clock = crate::test_clock::virtualize();
    let mut dma = TestRegion::new(1024 * 1024 * 8);
    let (mut controller, hardware) = hub::tests::controller(
        &mut dma,
        64,
        BTreeMap::from([(0, Node::bot_disk(SUPER_SPEED))]),
    );
    hardware.borrow_mut().root_status =
        Some(PP | CCS | PED | u32::from(SUPER_SPEED) << PORT_SPEED_SHIFT);
    let mut failures = Vec::new();
    let devices = controller.enumerate(&mut dma, &mut failures);
    assert!(failures.is_empty(), "{failures:?}");
    let shared = Rc::new(RefCell::new(controller));
    hardware.borrow_mut().nodes.get_mut(&0).unwrap().wedge_bot();
    hardware.borrow_mut().wedge_drops_root_link = Some(true);
    hardware.borrow_mut().root_reset_us = Some(600_000);
    let mut found = Found::default();
    bind::bind(&shared, devices[0].clone(), &mut dma, &mut found);
    assert_eq!(found.disks.len(), 1, "{:?}", found.failures);
}

/// A stick whose first command hangs and whose class reset is acknowledged
/// without bringing it back. As Linux's `usb_stor_invoke_transport`, the
/// timed-out command is recovered by a port reset first, where the class
/// reset ended the command, the unit failed, and nothing reset the port.
#[test]
fn a_timed_out_first_command_resets_a_root_stick_port_before_any_class_reset() {
    let _clock = crate::test_clock::virtualize();
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (mut controller, hardware) =
            hub::tests::controller(&mut dma, 64, BTreeMap::from([(0, Node::bot_disk(speed))]));
        hardware.borrow_mut().root_status =
            Some(PP | CCS | PED | u32::from(speed) << PORT_SPEED_SHIFT);
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let shared = Rc::new(RefCell::new(controller));
        hardware
            .borrow_mut()
            .nodes
            .get_mut(&0)
            .unwrap()
            .wedge_bot_acking_reset();
        let mut found = Found::default();
        bind::bind(&shared, devices[0].clone(), &mut dma, &mut found);
        assert_eq!(found.disks.len(), 1, "{:?}", found.failures);
        assert!(!hardware.borrow().nodes[&0].bot_wedged());
        assert!(
            !hardware
                .borrow()
                .requests
                .iter()
                .any(|(_, _, request)| *request == setup(0x21, 0xff, 0, 0, 0))
        );
    }
}

/// The same during the first scan of a stick behind a hub: its owner resets
/// the hub port in place, and no class reset runs first.
#[test]
fn a_timed_out_first_command_resets_a_hub_stick_port_before_any_class_reset() {
    let _clock = crate::test_clock::virtualize();
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (mut controller, hardware) = hub::tests::controller(
            &mut dma,
            64,
            BTreeMap::from([(0, Node::hub(speed, 1)), (1, Node::bot_disk(speed))]),
        );
        let mut failures = Vec::new();
        let devices = controller.enumerate(&mut dma, &mut failures);
        assert!(failures.is_empty(), "{failures:?}");
        let stick = devices
            .iter()
            .find(|d| d.descriptor[4] != HUB_CLASS)
            .unwrap()
            .clone();
        let shared = Rc::new(RefCell::new(controller));
        hardware
            .borrow_mut()
            .nodes
            .get_mut(&1)
            .unwrap()
            .wedge_bot_acking_reset();
        let mut found = Found::default();
        bind::bind(&shared, stick, &mut dma, &mut found);
        assert_eq!(found.disks.len(), 1, "{:?}", found.failures);
        assert!(!hardware.borrow().nodes[&1].bot_wedged());
        assert!(
            !hardware
                .borrow()
                .requests
                .iter()
                .any(|(_, _, request)| *request == setup(0x21, 0xff, 0, 0, 0))
        );
    }
}

/// A published stick behind one hub or two, in high-speed and SuperSpeed
/// topologies: the fixture's nodes, the route of the hub whose first port
/// holds the stick, and the stick's route.
fn sticks_behind_hubs() -> Vec<(BTreeMap<u32, Node>, u32, u32)> {
    let mut topologies = Vec::new();
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        topologies.push((
            BTreeMap::from([(0, Node::hub(speed, 1)), (1, Node::bot_disk(speed))]),
            0,
            1,
        ));
        topologies.push((
            BTreeMap::from([
                (0, Node::hub(speed, 1)),
                (1, Node::hub(speed, 1)),
                (0x11, Node::bot_disk(speed)),
            ]),
            1,
            0x11,
        ));
    }
    topologies
}

/// Enumerate `nodes` and bind its stick: the controller's hardware model
/// and the stick's one published disk.
fn published_stick(
    dma: &mut TestRegion,
    nodes: BTreeMap<u32, Node>,
) -> (
    Rc<RefCell<hub::model::Hardware>>,
    crate::usb::storage::UsbDisk,
) {
    let (mut controller, hardware) = hub::tests::controller(dma, 64, nodes);
    let mut failures = Vec::new();
    let devices = controller.enumerate(dma, &mut failures);
    assert!(failures.is_empty(), "{failures:?}");
    let stick = devices
        .iter()
        .find(|d| d.descriptor[4] != HUB_CLASS)
        .unwrap()
        .clone();
    let shared = Rc::new(RefCell::new(controller));
    let mut found = Found::default();
    bind::bind(&shared, stick, dma, &mut found);
    assert!(found.failures.is_empty(), "{:?}", found.failures);
    assert_eq!(found.disks.len(), 1);
    (hardware, found.disks.pop().unwrap())
}

fn hub_port(hardware: &Rc<RefCell<hub::model::Hardware>>, hub: u32) -> hub::model::PortModel {
    hardware.borrow().nodes[&hub].ports.as_ref().unwrap()[0].clone()
}

/// A stick behind hubs whose link broke after it was published (every
/// bulk transfer and the class reset end with a USB transaction error, as a
/// stick's link may after thousands of seconds of reads): as Linux's
/// usb-storage, the failed class reset is followed by a reset of the stick's
/// port on its parent hub, and the read runs again and completes, where the
/// owner declined the reset for a device behind a hub and failed the read.
#[test]
fn a_stick_behind_hubs_whose_link_broke_is_reset_on_its_hub_port_and_the_read_runs_again() {
    use crate::block::BlockDevice;
    let _clock = crate::test_clock::virtualize();
    for (nodes, hub, route) in sticks_behind_hubs() {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (hardware, mut disk) = published_stick(&mut dma, nodes);
        let payload = [0x6e; 512];
        disk.write(4, &payload).unwrap();
        crate::usb::storage::take_recoveries();
        let resets = hub_port(&hardware, hub).reset_count;
        hardware
            .borrow_mut()
            .nodes
            .get_mut(&route)
            .unwrap()
            .break_bot_link();
        let mut read = [0; 512];
        assert_eq!(disk.read(4, &mut read), Ok(()), "route {route:#x}");
        assert_eq!(read, payload);
        assert!(!hardware.borrow().nodes[&route].bot_link_broken());
        assert_eq!(hub_port(&hardware, hub).reset_count, resets + 1);
        let records = crate::usb::storage::take_recoveries().0;
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            (records[0].first, records[0].step, records[0].end),
            (
                crate::usb::storage::Cause::Device(transfer::USB_TRANSACTION_ERROR),
                crate::usb::storage::Step::PortReset,
                crate::usb::storage::End::Completed
            )
        );
        // The disk goes on: later commands run without another reset.
        disk.write(5, &payload).unwrap();
        disk.read(5, &mut read).unwrap();
        assert_eq!(read, payload);
        assert_eq!(hub_port(&hardware, hub).reset_count, resets + 1);
    }
}

/// The same broken stick whose hub port reset fails (`fault`): the read
/// ends with one recovery record that names the reset's cause, and later
/// commands fail at once with no further reset or record.
fn a_failed_hub_port_reset_is_reported_once(fault: hub::model::Fault, cause: &str) {
    use crate::block::BlockDevice;
    let _clock = crate::test_clock::virtualize();
    for (nodes, hub, route) in sticks_behind_hubs() {
        let mut dma = TestRegion::new(1024 * 1024 * 8);
        let (hardware, mut disk) = published_stick(&mut dma, nodes);
        crate::usb::storage::take_recoveries();
        let resets = hub_port(&hardware, hub).reset_count;
        {
            let mut hardware = hardware.borrow_mut();
            hardware.nodes.get_mut(&route).unwrap().break_bot_link();
            hardware
                .nodes
                .get_mut(&hub)
                .unwrap()
                .ports
                .as_mut()
                .unwrap()[0]
                .fault = fault;
        }
        let mut read = [0; 512];
        assert!(disk.read(4, &mut read).is_err(), "route {route:#x}");
        assert!(hub_port(&hardware, hub).reset_count > resets);
        let records = crate::usb::storage::take_recoveries().0;
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].end, crate::usb::storage::End::ResetFailed);
        let line = alloc::format!("{}", records[0]);
        assert!(
            line.ends_with(&alloc::format!(
                "the port reset failed with {cause}: the disk fails until it is bound again"
            )),
            "{line}"
        );
        let attempts = hub_port(&hardware, hub).reset_count;
        assert!(disk.read(4, &mut read).is_err());
        assert_eq!(hub_port(&hardware, hub).reset_count, attempts);
        assert!(crate::usb::storage::take_recoveries().0.is_empty());
    }
}

#[test]
fn a_hub_port_reset_that_never_completes_is_reported_once() {
    a_failed_hub_port_reset_is_reported_once(hub::model::Fault::ResetForever, "a time-out");
}

/// A stick that left its hub port: the port reset finds no device, as
/// Linux's `usb_reset_device` answers `-ENODEV`.
#[test]
fn a_stick_gone_from_its_hub_port_is_reported_once() {
    a_failed_hub_port_reset_is_reported_once(
        hub::model::Fault::Disconnect,
        "the device gone from its port",
    );
}

/// A leaf behind one hub or two is reset on its parent's port and addressed
/// and configured again with its own retired storage alone: no allocator is
/// given, the old identity is refused, and the sibling keeps its slot.
#[test]
fn a_reusable_reset_behind_hubs_needs_no_allocation_and_keeps_the_sibling() {
    for speed in [HIGH_SPEED, SUPER_SPEED] {
        for (nodes, hub, route) in [
            (
                BTreeMap::from([
                    (0, Node::hub(speed, 2)),
                    (1, Node::leaf(speed)),
                    (2, Node::leaf(speed)),
                ]),
                0,
                1,
            ),
            (
                BTreeMap::from([
                    (0, Node::hub(speed, 2)),
                    (1, Node::hub(speed, 1)),
                    (0x11, Node::leaf(speed)),
                    (2, Node::leaf(speed)),
                ]),
                1,
                0x11,
            ),
        ] {
            let mut dma = TestRegion::new(512 * 1024 * 8);
            let (mut controller, hardware) = hub::tests::controller(&mut dma, 64, nodes);
            let mut failures = Vec::new();
            let devices = controller.enumerate(&mut dma, &mut failures);
            assert!(failures.is_empty(), "{failures:?}");
            let at = |route: u32| {
                devices
                    .iter()
                    .find(|d| {
                        controller.slots[usize::from(d.slot)]
                            .as_ref()
                            .unwrap()
                            .attachment
                            .route
                            == route
                    })
                    .unwrap()
                    .clone()
            };
            let (old, sibling) = (at(route), at(2));
            let old = &old;
            let endpoints = interfaces(&old.configuration)[0].endpoints.clone();
            controller
                .set_configuration(old.slot, old.configuration_value())
                .unwrap();
            controller
                .configure(old.slot, &endpoints, None, &mut dma)
                .unwrap();
            let sibling_output = controller.slot(sibling.slot).unwrap()._output.physical();
            let resets = hardware.borrow().nodes[&hub].ports.as_ref().unwrap()[0].reset_count;
            // SAFETY: the empty range hands out no memory.
            let mut none = unsafe { crate::bus::dma::RegionAllocator::new(0..0) };
            // SAFETY: no class owner was bound; endpoints are the full
            // configured DCI inventory of this one downstream leaf.
            let fresh = unsafe { controller.reset_and_reuse(old, &endpoints, &mut none) }
                .unwrap_or_else(|error| panic!("route {route:#x}: {error:?}"));
            assert_ne!(fresh.id, old.id);
            assert_eq!(fresh.location, old.location);
            assert_eq!(fresh.speed, old.speed);
            assert_eq!(
                hardware.borrow().nodes[&hub].ports.as_ref().unwrap()[0].reset_count,
                resets + 1
            );
            controller
                .set_configuration(fresh.slot, old.configuration_value())
                .unwrap();
            controller
                .configure(fresh.slot, &endpoints, None, &mut none)
                .unwrap();
            assert!(controller.device(old.id).is_err());
            assert!(controller.device(fresh.id).is_ok());
            assert!(controller.device(sibling.id).is_ok());
            assert_eq!(
                controller.slot(sibling.slot).unwrap()._output.physical(),
                sibling_output
            );
        }
    }
}
