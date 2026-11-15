use super::*;
use stela_contracts::dma::test_support::Ram;
use stela_contracts::interrupt::InterruptLatch;

/// Each window's guest PA in these tests: apart, and ECAM above the rest.
fn base(index: usize) -> u64 {
    if index == 0 {
        0x10_0000_0000
    } else {
        (index as u64) << 24
    }
}

fn at(window: Window, offset: u64) -> u64 {
    base(window.index()) + offset
}

fn probe() -> ApcieProbe {
    let windows: Vec<_> = (0..50)
        .map(|index| {
            let bytes = if index == 0 { 0x1000_0000 } else { 0x10_0000 };
            (index, base(index), bytes)
        })
        .collect();
    ApcieProbe::new("apcie", 0, &[0, 1], &windows)
}

#[test]
fn windows_follow_the_drivers_reg_indices() {
    for index in 0..21 {
        assert_eq!(Window::of(index).index(), index);
    }
    assert_eq!(Window::of(0), Window::Config);
    assert_eq!(Window::of(1), Window::Common);
    assert_eq!(Window::of(5), Window::Fuse);
    assert_eq!(Window::of(6), Window::Port(0));
    assert_eq!(Window::of(8), Window::PhyGlue(0));
    assert_eq!(Window::of(11), Window::Port(1));
    assert_eq!(Window::of(15), Window::Intr2Axi(1));
}

fn r(stub: &mut ApcieProbe, access: &mut Access<'_>, window: Window, offset: u64) -> u64 {
    stub.read(at(window, offset), Width::Word, access).unwrap()
}

fn w(stub: &mut ApcieProbe, access: &mut Access<'_>, window: Window, offset: u64, value: u64) {
    stub.write(at(window, offset), Width::Word, value, access)
        .unwrap();
}

#[test]
fn the_handshakes_the_driver_waits_for_answer_at_once() {
    let mut latch = InterruptLatch::new();
    let access = &mut Access::new(0, 0, &mut latch);
    let stub = &mut probe();
    // _initializePhy: REFCLK0 then REFCLK1 request and acknowledge.
    w(stub, access, Window::Phy, 0, 1);
    assert_eq!(r(stub, access, Window::Phy, 0) & 4, 4);
    w(stub, access, Window::Phy, 0, 3);
    assert_eq!(r(stub, access, Window::Phy, 0) & 0xc, 0xc);
    assert_eq!(r(stub, access, Window::Common, 0x58) & 1, 1);
    assert_eq!(r(stub, access, Window::Common, 0x28) & 0x10, 0x10);
    // Port ready follows the port's enable, both ways.
    assert_eq!(r(stub, access, Window::Port(1), 0x804) & 1, 0);
    w(stub, access, Window::Port(1), 0x800, 0x100101);
    assert_eq!(r(stub, access, Window::Port(1), 0x804) & 1, 1);
    w(stub, access, Window::Port(1), 0x800, 0x100100);
    assert_eq!(r(stub, access, Window::Port(1), 0x804) & 1, 0);
    assert_eq!(stub.disables, 1);
    // No link, nothing busy, nothing outstanding.
    w(stub, access, Window::Port(0), 0x208, 0xffff);
    assert_eq!(r(stub, access, Window::Port(0), 0x208) & 0b101, 0);
    w(stub, access, Window::Port(0), 0x80c, 0x7f);
    assert_eq!(r(stub, access, Window::Port(0), 0x80c), 0);
    // The per-port PHY glue answers like the PHY.
    w(stub, access, Window::PhyGlue(1), 0, 0x603);
    assert_eq!(r(stub, access, Window::PhyGlue(1), 0), 0x60f);
}

#[test]
fn registers_store_writes_and_interrupt_status_clears_by_writing_ones() {
    let mut latch = InterruptLatch::new();
    let mut access = Access::new(0, 0, &mut latch);
    let mut stub = probe();
    stub.write(
        at(Window::Axi2Af, 0x400),
        Width::Word,
        0x1_0000,
        &mut access,
    )
    .unwrap();
    assert_eq!(
        stub.read(at(Window::Axi2Af, 0x400), Width::Word, &mut access),
        Ok(0x1_0000)
    );
    stub.write(at(Window::PhyIp, 0x522e), Width::Half, 0xbeef, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(at(Window::PhyIp, 0x522c), Width::Word, &mut access),
        Ok(0xbeef_0000)
    );
    stub.write(at(Window::Port(0), 0x100), Width::Word, 0, &mut access)
        .unwrap();
    stub.store(Window::Port(0), 0x100, 0x1000 | 0x4);
    stub.write(at(Window::Port(0), 0x100), Width::Word, 0x4, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(at(Window::Port(0), 0x100), Width::Word, &mut access),
        Ok(0x1000)
    );
    // A window the stub never saw reads its stored zero; nothing refuses.
    assert_eq!(
        stub.read(at(Window::Fuse, 0x490), Width::Word, &mut access),
        Ok(0)
    );
    assert_eq!(
        stub.read(at(Window::Intr2Axi(7), 0x80), Width::Double, &mut access),
        Ok(0)
    );
}

#[test]
fn root_ports_are_bridges_with_a_pcie_capability_and_nothing_is_behind_them() {
    let mut latch = InterruptLatch::new();
    let mut access = Access::new(0, 0, &mut latch);
    let mut stub = probe();
    let config = |bus: u64, device: u64, function: u64, register: u64| {
        at(
            Window::Config,
            bus << 20 | device << 15 | function << 12 | register,
        )
    };
    let mut read =
        |stub: &mut ApcieProbe, address, width| stub.read(address, width, &mut access).unwrap();
    let port = config(0, 1, 0, 0);
    assert_eq!(read(&mut stub, port, Width::Half), 0x106b);
    assert_eq!(read(&mut stub, port + 0x0a, Width::Half), 0x0604);
    assert_eq!(read(&mut stub, port + 0x0e, Width::Byte), 1);
    // findPCICapability: status bit 4, then the list from 0x34.
    assert_eq!(read(&mut stub, port + 0x06, Width::Half) & 0x10, 0x10);
    let cap = read(&mut stub, port + 0x34, Width::Byte);
    assert_eq!(cap, 0x70);
    assert_eq!(read(&mut stub, port + cap, Width::Byte), 0x10);
    assert_eq!(read(&mut stub, port + cap + 1, Width::Byte), 0);
    // Root port type, and LNKCAP names port 1.
    assert_eq!(read(&mut stub, port + cap + 2, Width::Half) >> 4 & 0xf, 4);
    assert_eq!(read(&mut stub, port + cap + 0xc, Width::Word) >> 24, 1);
    // The errata #55 wait reads an LTSSM state other than S_L1_IDLE.
    assert_ne!(read(&mut stub, port + 0x728, Width::Word) & 0x1f, 0x14);

    // No device on the secondary bus, another function or another device.
    for address in [
        config(1, 0, 0, 0),
        config(0, 1, 1, 0),
        config(0, 2, 0, 0),
        config(0x10, 3, 0, 0x100),
    ] {
        assert_eq!(read(&mut stub, address, Width::Word), 0xffff_ffff);
        assert_eq!(read(&mut stub, address, Width::Half), 0xffff);
        assert_eq!(read(&mut stub, address, Width::Byte), 0xff);
    }
    let mut access = Access::new(0, 0, &mut latch);
    stub.write(config(1, 0, 0, 0), Width::Word, 0, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(config(1, 0, 0, 0), Width::Word, &mut access),
        Ok(0xffff_ffff)
    );
}

#[test]
fn root_port_identity_and_capabilities_change_only_through_dbi() {
    let mut latch = InterruptLatch::new();
    let mut access = Access::new(0, 0, &mut latch);
    let mut stub = probe();
    let port = at(Window::Config, 0);
    // A BAR sizing write reads back no BAR; bus numbers and LNKCTL store.
    stub.write(port + 0x10, Width::Word, 0xffff_ffff, &mut access)
        .unwrap();
    assert_eq!(stub.read(port + 0x10, Width::Word, &mut access), Ok(0));
    stub.write(port + 0x18, Width::Word, 0x0001_0100, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(port + 0x18, Width::Word, &mut access),
        Ok(0x0001_0100)
    );
    stub.write(port + 0x80, Width::Word, 0x400, &mut access)
        .unwrap();
    assert_eq!(stub.read(port + 0x80, Width::Word, &mut access), Ok(0x400));
    // The class changes only with DBI_RO_WR_EN set.
    stub.write(port + 0x08, Width::Word, 0x0108_0200, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(port + 0x08, Width::Word, &mut access),
        Ok(0x0604_0000)
    );
    stub.write(port + 0x8bc, Width::Word, 1, &mut access)
        .unwrap();
    stub.write(port + 0x08, Width::Word, 0x0604_0001, &mut access)
        .unwrap();
    stub.write(port + 0x8bc, Width::Word, 0, &mut access)
        .unwrap();
    assert_eq!(
        stub.read(port + 0x08, Width::Word, &mut access),
        Ok(0x0604_0001)
    );
}

#[test]
fn repeats_are_counted_and_the_log_is_capped() {
    let mut latch = InterruptLatch::new();
    let mut access = Access::new(0, 0, &mut latch);
    let mut stub = probe();
    for _ in 0..5000 {
        stub.read(at(Window::Common, 0x58), Width::Word, &mut access)
            .unwrap();
    }
    assert_eq!(stub.lines, 2, "the first read and the 1000th reminder");
    stub.read(at(Window::Common, 0x28), Width::Word, &mut access)
        .unwrap();
    assert_eq!(stub.lines, 4, "the poll line, then the new read");
    let stat = stub.stats[&Place(Window::Common, 0x58)];
    assert_eq!((stat.reads, stat.writes, stat.last), (5000, 0, 1));
    for offset in 0..(2 * LINE_CAP as u64) {
        stub.write(at(Window::PhyIp, offset * 4), Width::Word, 1, &mut access)
            .unwrap();
    }
    assert_eq!(stub.lines, LINE_CAP);
    assert!(stub.capped && stub.unlogged > 0);
    assert!(stub.stats.len() <= TRACKED && stub.untracked > 0);
}

#[test]
fn every_lifecycle_state_is_accepted() {
    let stub = probe();
    for bits in 0..32u8 {
        let state = LifecycleState {
            powered: bits & 1 != 0,
            clocked: bits & 2 != 0,
            quiesced: bits & 4 != 0,
            reset: bits & 8 != 0,
            retention: bits & 16 != 0,
        };
        assert!(stub.check_lifecycle(state).is_ok());
    }
}

#[test]
fn an_access_belongs_to_the_smallest_window_holding_it() {
    // J473AP: the PHY window holds the PHY IP and each port's PHY glue.
    let phy = 0x6_8008_4000;
    let windows = [
        (2, phy, 0x6_0000),
        (3, phy + 0x4_0000, 0x1_0000),
        (8, phy + 0x4000, 0x4000),
        (13, phy + 0x8000, 0x4000),
    ];
    let mut stub = ApcieProbe::new("apcie", 0, &[0, 1], &windows);
    assert_eq!(stub.place(phy), Place(Window::Phy, 0));
    assert_eq!(stub.place(phy + 0x4004), Place(Window::PhyGlue(0), 4));
    assert_eq!(stub.place(phy + 0x8000), Place(Window::PhyGlue(1), 0));
    assert_eq!(stub.place(phy + 0x4_6238), Place(Window::PhyIp, 0x6238));
    assert_eq!(stub.place(phy + 0xc000), Place(Window::Phy, 0xc000));
    assert_eq!(stub.place(0x1000), Place(Window::Outside, 0x1000));
    // Probe2's hang: port 0's refclk request at phy+0x4000 is acknowledged.
    let mut latch = InterruptLatch::new();
    let access = &mut Access::new(0, 0, &mut latch);
    stub.write(phy + 0x4004, Width::Word, 0x8000, access)
        .unwrap();
    stub.write(phy + 0x4000, Width::Word, 1, access).unwrap();
    assert_eq!(stub.read(phy + 0x4000, Width::Word, access), Ok(0x5));
    stub.write(phy + 0x4000, Width::Word, 3, access).unwrap();
    assert_eq!(stub.read(phy + 0x4000, Width::Word, access), Ok(0xf));
    assert_eq!(stub.read(phy + 0x4004, Width::Word, access), Ok(0x8000));
}

#[test]
fn the_ltssm_tracer_snapshot_is_ready_and_empty() {
    // probe3: _captureLTSSMDebugTracer polled ltssm1+0x30 bit 12 forever.
    let mut latch = InterruptLatch::new();
    let access = &mut Access::new(0, 0, &mut latch);
    let stub = &mut probe();
    w(stub, access, Window::Ltssm(1), 0x38, 1);
    let status = r(stub, access, Window::Ltssm(1), 0x30);
    assert_eq!(status & 0x1000, 0x1000);
    assert_eq!(status & 0x7f, 0);
}

const MEMORY: u64 = 0x20_0000_0000;
const PCI: u64 = 0x8000_0000;
const AIC: u16 = 787;

/// The probe with port 1 modeled and one 256 MiB memory window.
fn model() -> ApcieProbe {
    probe().with_model(
        1,
        Some(AicInput::new(0, AIC)),
        &[(MEMORY, 0x1000_0000, PCI)],
    )
}

fn config(bus: u64, device: u64, function: u64, register: u64) -> u64 {
    at(
        Window::Config,
        bus << 20 | device << 15 | function << 12 | register,
    )
}

fn levels(latch: &mut InterruptLatch) -> Vec<bool> {
    latch
        .take_aic_inputs()
        .into_iter()
        .filter(|(input, _)| *input == AicInput::new(0, AIC))
        .map(|(_, level)| level)
        .collect()
}

#[test]
fn link_status_is_x1_at_the_target_speed_within_the_capability() {
    // LNKCAP 8 GT/s; LNKCTL2 target 2.5 GT/s (lan-1gb's maximum-link-speed).
    assert_eq!(link::link_status(0x0010_0013, 1), 0x3011);
    assert_eq!(link::link_status(0x0010_0013, 3), 0x3013);
    // A target above the capability, or none, trains at the capability.
    assert_eq!(link::link_status(0x0010_0012, 3), 0x3012);
    assert_eq!(link::link_status(0x0010_0012, 0), 0x3012);
    // Width is always 1: handleLinkUp panics on anything else.
    assert_eq!(link::link_status(0x0010_0013, 2) >> 4 & 0x3f, 1);
}

#[test]
fn the_modeled_port_trains_after_ltssm_start_and_raises_its_interrupt() {
    let mut latch = InterruptLatch::new();
    let stub = &mut model();
    assert_eq!(stub.modeled_port(), Some(1));
    {
        let access = &mut Access::new(0, 0, &mut latch);
        // The reset burst masks everything; enableInterrupts unmasks LINK_UP.
        w(stub, access, Window::Port(1), 0x104, 0x0fff_ffff);
        w(stub, access, Window::Port(1), 0x800, 0x100101);
        w(stub, access, Window::Port(1), 0x814, 1);
        w(stub, access, Window::Port(1), 0x104, 0xfb51_27ff);
        // enableLinkTraining: LTSSM start. Nothing yet.
        w(stub, access, Window::Port(1), 0x80, 1);
        assert_eq!(r(stub, access, Window::Port(1), 0x208) & 1, 0);
        assert_eq!(r(stub, access, Window::Port(1), 0x100), 0);
    }
    assert!(levels(&mut latch).is_empty());
    // The device asks to be advanced when training ends.
    let deadline = {
        let access = &mut Access::new(0, 1, &mut latch);
        stub.advance(access).unwrap()
    };
    assert_eq!(deadline, Some(link::TRAINING_TICKS));
    {
        let access = &mut Access::new(0, link::TRAINING_TICKS, &mut latch);
        stub.advance(access).unwrap();
        assert!(stub.link_up());
        // LINKSTS up, INTSTAT LINK_UP; other ports stay down.
        assert_eq!(r(stub, access, Window::Port(1), 0x208) & 1, 1);
        assert_eq!(r(stub, access, Window::Port(0), 0x208) & 1, 0);
        assert_eq!(
            r(stub, access, Window::Port(1), 0x100),
            u64::from(link::LINK_UP)
        );
    }
    assert_eq!(levels(&mut latch), [true]);
    {
        let access = &mut Access::new(0, link::TRAINING_TICKS, &mut latch);
        // handleInterrupt clears LINK_UP (write one to clear): the line drops.
        w(
            stub,
            access,
            Window::Port(1),
            0x100,
            u64::from(link::LINK_UP),
        );
        assert_eq!(r(stub, access, Window::Port(1), 0x100), 0);
    }
    assert_eq!(levels(&mut latch), [false]);
    {
        let access = &mut Access::new(0, link::TRAINING_TICKS, &mut latch);
        // The root port's LNKSTA: data link active, x1, at LNKCTL2's 2.5 GT/s.
        stub.write(config(0, 1, 0, 0xa0), Width::Half, 1, access)
            .unwrap();
        let lnksta = stub
            .read(config(0, 1, 0, 0x82), Width::Half, access)
            .unwrap();
        assert_eq!(lnksta & 0x2000, 0x2000);
        assert_eq!(lnksta >> 4 & 0x3f, 1);
        assert_eq!(lnksta & 0xf, 1);
        // The port logic's LTSSM state is L0, not the errata's S_L1_IDLE.
        let ltssm = stub
            .read(config(0, 1, 0, 0x728), Width::Word, access)
            .unwrap();
        assert_eq!(ltssm & 0x3f, u64::from(link::LTSSM_L0));
        // Port 0's root port has no link.
        assert_eq!(stub.read(config(0, 0, 0, 0x82), Width::Half, access), Ok(0));
        // The disable path's PERST# assert drops the link.
        w(stub, access, Window::Port(1), 0x814, 0);
        assert!(!stub.link_up());
        assert_eq!(r(stub, access, Window::Port(1), 0x208) & 1, 0);
    }
}

/// A modeled probe whose link is up, with bus 1 behind port 1.
fn linked(latch: &mut InterruptLatch) -> ApcieProbe {
    let mut stub = model();
    let access = &mut Access::new(0, 0, latch);
    w(&mut stub, access, Window::Port(1), 0x80, 1);
    let access = &mut Access::new(0, link::TRAINING_TICKS, latch);
    stub.advance(access).unwrap();
    stub.write(config(0, 1, 0, 0x18), Width::Word, 0x0001_0100, access)
        .unwrap();
    stub
}

#[test]
fn the_endpoint_answers_on_the_secondary_bus_with_a_header_macos_enumerates() {
    let mut latch = InterruptLatch::new();
    let stub = &mut linked(&mut latch);
    let access = &mut Access::new(0, link::TRAINING_TICKS, &mut latch);
    let mut read =
        |stub: &mut ApcieProbe, address, width| stub.read(address, width, access).unwrap();
    let endpoint = config(1, 0, 0, 0);
    assert_eq!(read(stub, endpoint, Width::Half), u64::from(link::VENDOR));
    assert_eq!(
        read(stub, endpoint + 2, Width::Half),
        u64::from(link::DEVICE)
    );
    assert_eq!(
        read(stub, endpoint + 8, Width::Word),
        u64::from(link::CLASS_REVISION)
    );
    // Type 0, single function, no interrupt pin.
    assert_eq!(read(stub, endpoint + 0x0e, Width::Byte), 0);
    assert_eq!(read(stub, endpoint + 0x3d, Width::Byte), 0);
    // A PCIe v2 endpoint capability, x1 at 2.5 GT/s, then MSI (64-bit,
    // one vector, off), then no more.
    assert_eq!(read(stub, endpoint + 6, Width::Half) & 0x10, 0x10);
    let cap = read(stub, endpoint + 0x34, Width::Byte);
    assert_eq!(read(stub, endpoint + cap, Width::Word), 0x0002_8010);
    assert_eq!(read(stub, endpoint + cap + 0x12, Width::Half) & 0x3ff, 0x11);
    let msi = read(stub, endpoint + cap + 1, Width::Byte);
    assert_eq!(read(stub, endpoint + msi, Width::Word), 0x0080_0005);
    assert_eq!(read(stub, endpoint + 0x100, Width::Word), 0);
    // Only device 0, function 0, only on the secondary bus.
    for address in [config(1, 1, 0, 0), config(1, 0, 1, 0), config(2, 0, 0, 0)] {
        assert_eq!(read(stub, address, Width::Word), 0xffff_ffff);
    }
}

#[test]
fn the_endpoint_bar_sizes_assigns_and_decodes_through_the_root_port() {
    let mut latch = InterruptLatch::new();
    let stub = &mut linked(&mut latch);
    let access = &mut Access::new(0, link::TRAINING_TICKS, &mut latch);
    let endpoint = config(1, 0, 0, 0);
    let port = config(0, 1, 0, 0);
    // Sizing: a 4 KiB 32-bit memory BAR; the others are absent.
    stub.write(endpoint + 0x10, Width::Word, 0xffff_ffff, access)
        .unwrap();
    assert_eq!(
        stub.read(endpoint + 0x10, Width::Word, access),
        Ok(0xffff_f000)
    );
    stub.write(endpoint + 0x14, Width::Word, 0xffff_ffff, access)
        .unwrap();
    assert_eq!(stub.read(endpoint + 0x14, Width::Word, access), Ok(0));
    // Assigned at the window's PCI base, but no decoding yet: all ones.
    stub.write(endpoint + 0x10, Width::Word, PCI | 0x123, access)
        .unwrap();
    assert_eq!(stub.read(endpoint + 0x10, Width::Word, access), Ok(PCI));
    assert_eq!(stub.read(MEMORY, Width::Word, access), Ok(0xffff_ffff));
    // Memory decoding on the endpoint, then the root port's window and
    // memory space: the BAR is RAM.
    stub.write(endpoint + 4, Width::Half, 0x6, access).unwrap();
    assert_eq!(stub.read(MEMORY, Width::Word, access), Ok(0xffff_ffff));
    stub.write(port + 0x20, Width::Word, 0x8000_8000, access)
        .unwrap();
    stub.write(port + 4, Width::Half, 0x6, access).unwrap();
    stub.write(MEMORY + 0xff8, Width::Double, 0x0123_4567_89ab_cdef, access)
        .unwrap();
    assert_eq!(
        stub.read(MEMORY + 0xff8, Width::Double, access),
        Ok(0x0123_4567_89ab_cdef)
    );
    // Past the BAR nothing decodes; writes there go nowhere.
    stub.write(MEMORY + 0x1000, Width::Word, 0, access).unwrap();
    assert_eq!(
        stub.read(MEMORY + 0x1000, Width::Word, access),
        Ok(0xffff_ffff)
    );
    // Memory decoding off on the endpoint hides the BAR again.
    stub.write(endpoint + 4, Width::Half, 0, access).unwrap();
    assert_eq!(
        stub.read(MEMORY + 0xff8, Width::Word, access),
        Ok(0xffff_ffff)
    );
}

#[test]
fn without_the_model_no_link_comes_up_and_the_memory_window_is_unknown() {
    let mut latch = InterruptLatch::new();
    let stub = &mut probe();
    let access = &mut Access::new(0, 0, &mut latch);
    w(stub, access, Window::Port(1), 0x80, 1);
    let access = &mut Access::new(0, 10 * link::TRAINING_TICKS, &mut latch);
    assert_eq!(
        stub.advance(access).unwrap(),
        Some(10 * link::TRAINING_TICKS + BEAT_SECONDS * TICKS_PER_SECOND)
    );
    assert!(!stub.link_up());
    assert_eq!(r(stub, access, Window::Port(1), 0x208) & 1, 0);
    assert_eq!(
        stub.place(MEMORY),
        Place(Window::Outside, MEMORY & ((1 << WINDOW_SHIFT) - 1))
    );
    // A port with no root port header cannot be modeled.
    assert_eq!(probe().with_model(7, None, &[]).modeled_port(), None);
}

#[test]
fn msi_ranges_decode_as_config_msi_range_programs_them() {
    // probe3's port 2: MSICFG 0x31 (eight vectors), MSIBASE 0x00100010
    // (base 16, and 16 mod 32 in [20:16]), MSIADDR msi-address & ~0xf.
    let window = dma::MsiWindow::of(0x31, 0x0010_0010, 0xffff_f000).unwrap();
    assert_eq!(
        window,
        dma::MsiWindow {
            base: 16,
            count: 8,
            doorbell: 0xffff_f000,
        }
    );
    // The data is the complex's vector: 16..23 are port 2's, 8 is port 1's.
    assert_eq!(window.vector(16), Some(16));
    assert_eq!(window.vector(23), Some(23));
    assert_eq!(window.vector(24), None);
    assert_eq!(window.vector(8), None);
    assert!(window.is_doorbell(0xffff_f000));
    assert!(window.is_doorbell(0xffff_f004));
    assert!(!window.is_doorbell(0xffff_e000));
    // Disabled (the reset burst's 0) or no doorbell: no window.
    assert_eq!(dma::MsiWindow::of(0, 0x0010_0010, 0xffff_f000), None);
    assert_eq!(dma::MsiWindow::of(0x31, 0, 0), None);
    // Port 0: base 0.
    assert_eq!(
        dma::MsiWindow::of(0x31, 0, 0xffff_f000).unwrap().vector(0),
        Some(0)
    );
}

#[test]
fn rid2sid_entries_name_a_requesters_stream_only_when_valid() {
    // _updateRIDToSIDMappings: RID [15:0], SID [19:16], VALID 31.
    let entries = [0, 0x8001_0100, 0x0002_0200, 0x8003_0300];
    assert_eq!(dma::sid_for(&entries, 0x0100), Some(1));
    assert_eq!(dma::sid_for(&entries, 0x0300), Some(3));
    // Written but not valid, or no entry at all.
    assert_eq!(dma::sid_for(&entries, 0x0200), None);
    assert_eq!(dma::sid_for(&entries, 0x0400), None);
}

const ROOT: u64 = 0x1_0000_0000;
const DART_PAGE: u64 = 0x4000;
const LEAVES: u64 = ROOT + DART_PAGE;
const FRAME: u64 = ROOT + 4 * DART_PAGE;
/// IOVA 0x200_4000: first-level entry 1, page entry 1 (16 KiB pages).
const IOVA: u64 = 0x200_4000;
const MSI_OFFSET: u32 = 0x380;
const SID: u64 = 1;

fn table_entry(address: u64) -> u64 {
    (address >> 4) & 0x3f_ffff_fc00 | 1
}

/// A t8110 DART as SPTM programs a stream for `NewIODARTMapper`'s mapper:
/// the stream enabled, translating, its TTBR at a two-level table that maps
/// `IOVA` to `FRAME`; then a TLB flush of the stream.
fn dart_with_stream(sid: u64) -> (crate::dart::Dart, Ram) {
    let mut dart = crate::dart::Dart::new(3, None);
    let mut ram = Ram::new(ROOT, 8 * DART_PAGE as usize);
    ram.put_u64(ROOT + 8, table_entry(LEAVES));
    ram.put_u64(LEAVES + 8, table_entry(FRAME) | 0xfff << 40);
    let mut latch = InterruptLatch::new();
    let access = &mut Access::new(0, 0, &mut latch);
    for (offset, value) in [
        (0xc00, 1 << sid),
        (0x1000 + 4 * sid, 1),
        (0x1400 + 4 * sid, (ROOT >> 14) << 2 | 1),
        (0x80, 1 << 8 | sid),
    ] {
        dart.write(offset, Width::Word, value, access).unwrap();
    }
    (dart, ram)
}

/// The linked model with MSI, its endpoint enumerated as IOPCIFamily would
/// leave it: BAR0 assigned and decoding, bus mastering on both sides, MSI
/// programmed with vector 9 (port 1's MSIBASE 8), and the port's MSI and
/// RID2SID registers as the driver writes them.
fn enumerated(latch: &mut InterruptLatch) -> ApcieProbe {
    let mut stub = linked(latch).with_msi(MSI_OFFSET);
    let access = &mut Access::new(0, link::TRAINING_TICKS, latch);
    let endpoint = config(1, 0, 0, 0);
    let port = config(0, 1, 0, 0);
    let mut put = |address, width, value| stub.write(address, width, value, access).unwrap();
    put(endpoint + 0x10, Width::Word, PCI);
    put(endpoint + 4, Width::Half, 0x6);
    put(port + 0x20, Width::Word, 0x8000_8000);
    put(port + 4, Width::Half, 0x6);
    put(endpoint + 0x84, Width::Word, 0xffff_f000);
    put(endpoint + 0x88, Width::Word, 0);
    put(endpoint + 0x8c, Width::Half, 9);
    put(endpoint + 0x82, Width::Half, 1);
    put(at(Window::Port(1), 0x124), Width::Word, 0x31);
    put(at(Window::Port(1), 0x128), Width::Word, 0x0008_0008);
    put(at(Window::Port(1), 0x168), Width::Word, 0xffff_f000);
    put(
        at(Window::Port(1), 0x828 + 4 * SID),
        Width::Word,
        0x8000_0100 | SID << 16,
    );
    stub
}

fn start_selftest(stub: &mut ApcieProbe, latch: &mut InterruptLatch, command: u64, iova: u64) {
    let access = &mut Access::new(0, link::TRAINING_TICKS, latch);
    for (offset, value) in [
        (0xf00, iova & 0xffff_ffff),
        (0xf04, iova >> 32),
        (0xf08, 64),
        (0xf0c, command),
    ] {
        stub.write(MEMORY + offset, Width::Word, value, access)
            .unwrap();
    }
}

fn bar(stub: &mut ApcieProbe, latch: &mut InterruptLatch, offset: u64) -> u64 {
    stub.read(
        MEMORY + offset,
        Width::Word,
        &mut Access::new(0, link::TRAINING_TICKS, latch),
    )
    .unwrap()
}

#[test]
fn the_endpoints_dma_goes_by_rid2sid_through_the_dart_and_raises_its_msi() {
    let mut latch = InterruptLatch::new();
    let stub = &mut enumerated(&mut latch);
    let (mut dart, mut ram) = dart_with_stream(SID);
    latch.take_aic_inputs();
    // The MSI capability as IOPCIFamily programmed it.
    assert!(!stub.dma_pending());
    // Self-test: write the pattern at IOVA, through stream 1, then MSI.
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    assert!(stub.dma_pending());
    assert_eq!(
        bar(stub, &mut latch, 0xf10) & 0xffff,
        u64::from(dma::STATUS_BUSY)
    );
    stub.run_dma(
        Some(&mut dart),
        &mut ram,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    );
    assert!(!stub.dma_pending());
    let mut written = [0u8; 64];
    assert!(ram.read(FRAME, &mut written));
    assert!(
        written
            .iter()
            .enumerate()
            .all(|(index, &byte)| byte == dma::pattern(0xa5, index))
    );
    assert_eq!(
        bar(stub, &mut latch, 0xf10),
        1 << 16 | u64::from(dma::STATUS_DONE | dma::STATUS_MSI)
    );
    assert_eq!(bar(stub, &mut latch, 0xf1c), 64);
    // The MSI: vector 9 is AIC input msi-vector-offset + 9, one rise and
    // fall (the AIC latches the rise: an edge input).
    let msi = AicInput::new(0, (MSI_OFFSET + 9) as u16);
    assert_eq!(latch.take_aic_inputs(), [(msi, true), (msi, false)]);

    // Read back through the DART into BAR0.
    ram.write(FRAME + 0x10, &[0xde, 0xad, 0xbe, 0xef]);
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_READ), IOVA + 0x10);
    stub.run_dma(
        Some(&mut dart),
        &mut ram,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    );
    assert_eq!(bar(stub, &mut latch, 0), 0xefbe_adde);
    assert_eq!(bar(stub, &mut latch, 0xf10) >> 16, 2);

    // An address the stream does not map: the DART faults (latched in its
    // ERROR register) and the endpoint reports it; no MSI.
    latch.take_aic_inputs();
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), 0x400_0000);
    stub.run_dma(
        Some(&mut dart),
        &mut ram,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    );
    let status = bar(stub, &mut latch, 0xf10);
    assert_eq!(
        status & 0xf,
        u64::from(dma::STATUS_DONE | dma::STATUS_REFUSED)
    );
    assert_eq!(bar(stub, &mut latch, 0xf18), 4);
    let error = dart
        .read(0x100, Width::Word, &mut Access::new(0, 0, &mut latch))
        .unwrap();
    assert_eq!(error >> 31, 1);
    assert_eq!(error >> 20 & 0xff, SID);
    assert!(latch.take_aic_inputs().is_empty());
}

#[test]
fn a_requester_without_a_valid_rid2sid_entry_or_bus_mastering_moves_nothing() {
    let mut latch = InterruptLatch::new();
    let stub = &mut enumerated(&mut latch);
    let (mut dart, mut ram) = dart_with_stream(SID);
    let run = |stub: &mut ApcieProbe,
               dart: &mut crate::dart::Dart,
               ram: &mut Ram,
               latch: &mut InterruptLatch| {
        stub.run_dma(
            Some(dart),
            ram,
            &mut Access::new(0, link::TRAINING_TICKS, latch),
        );
    };
    // The entry names another requester: no SID.
    stub.write(
        at(Window::Port(1), 0x828 + 4 * SID),
        Width::Word,
        0x8000_0200 | SID << 16,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    )
    .unwrap();
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    run(stub, &mut dart, &mut ram, &mut latch);
    assert_eq!(bar(stub, &mut latch, 0xf18), 2);
    assert_eq!(bar(stub, &mut latch, 0xf1c), 0);
    let mut written = [0u8; 4];
    assert!(ram.read(FRAME, &mut written));
    assert_eq!(written, [0; 4]);
    // No DART at all.
    stub.write(
        at(Window::Port(1), 0x828 + 4 * SID),
        Width::Word,
        0x8000_0100 | SID << 16,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    )
    .unwrap();
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    stub.run_dma(
        None,
        &mut ram,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    );
    assert_eq!(bar(stub, &mut latch, 0xf18), 3);
    // Bus mastering off on the endpoint.
    stub.write(
        config(1, 0, 0, 4),
        Width::Half,
        0x2,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    )
    .unwrap();
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    run(stub, &mut dart, &mut ram, &mut latch);
    assert_eq!(bar(stub, &mut latch, 0xf18), 1);
}

#[test]
fn a_doorbell_write_outside_the_ports_vectors_is_no_msi() {
    let mut latch = InterruptLatch::new();
    let stub = &mut enumerated(&mut latch);
    let (mut dart, mut ram) = dart_with_stream(SID);
    // The endpoint's MSI data is port 2's vector 16, not one of port 1's.
    stub.write(
        config(1, 0, 0, 0x8c),
        Width::Half,
        16,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    )
    .unwrap();
    latch.take_aic_inputs();
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    stub.run_dma(
        Some(&mut dart),
        &mut ram,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    );
    // The DMA went; the MSI did not.
    assert_eq!(bar(stub, &mut latch, 0xf1c), 64);
    assert_eq!(bar(stub, &mut latch, 0xf18), 5);
    assert_eq!(bar(stub, &mut latch, 0xf10) & u64::from(dma::STATUS_MSI), 0);
    assert!(latch.take_aic_inputs().is_empty());
    // Nothing is pending once the link is down.
    start_selftest(stub, &mut latch, u64::from(dma::COMMAND_WRITE), IOVA);
    stub.write(
        at(Window::Port(1), 0x80),
        Width::Word,
        0,
        &mut Access::new(0, link::TRAINING_TICKS, &mut latch),
    )
    .unwrap();
    assert!(!stub.dma_pending());
}
