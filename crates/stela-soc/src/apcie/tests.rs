use super::*;
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
