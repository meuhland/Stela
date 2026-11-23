use super::*;

fn exception(pe: u8, elr: u64) -> Entry {
    Entry {
        kind: Kind::Exception,
        pe,
        from_el: 1,
        to_el: 1,
        vector: 1,
        detail: 0x5600_0000,
        pc: elr,
        other: 0,
        retired: elr,
    }
}

#[test]
fn entries_round_trip_through_their_words() {
    let entry = Entry {
        kind: Kind::Recovery,
        pe: 7,
        from_el: 2,
        to_el: 2,
        vector: RECOVERY_WRITE | RECOVERY_HAZARD,
        detail: 0x1234,
        pc: 0xffff_fe00_3a6e_2a10,
        other: 0xffff_fe18_8664_8c22,
        retired: u64::MAX,
    };
    assert_eq!(Entry::unpack(entry.pack()), Some(entry));
    assert!(entry.hazard());
    assert_eq!(Entry::unpack([0; WORDS]), None);
}

#[test]
fn the_ring_keeps_its_last_entries_oldest_first_after_wrapping() {
    let ring = EventRing::new(3, 8, 2);
    for elr in 0..20u64 {
        ring.record(&exception(3, elr));
    }
    assert_eq!(ring.count(), 20);
    let recent = ring.recent(usize::MAX);
    assert_eq!(recent.len(), 8);
    assert_eq!(
        recent.iter().map(|(sequence, _)| *sequence).collect::<Vec<_>>(),
        (12..20).collect::<Vec<_>>()
    );
    assert!(recent.iter().all(|(sequence, entry)| entry.pc == *sequence));
    let last = ring.recent(3);
    assert_eq!(last.iter().map(|(_, entry)| entry.pc).collect::<Vec<_>>(), [17, 18, 19]);
    assert!(ring.flagged().is_empty());
}

#[test]
fn hazards_also_go_to_the_flagged_ring_which_ordinary_traffic_leaves() {
    let ring = EventRing::new(0, 4, 2);
    let hazard = Entry {
        kind: Kind::Recovery,
        vector: RECOVERY_WRITE | RECOVERY_HAZARD,
        ..exception(0, 0x40)
    };
    ring.record(&hazard);
    for elr in 0..10 {
        ring.record(&exception(0, elr));
    }
    assert!(ring.recent(usize::MAX).iter().all(|(_, entry)| !entry.hazard()));
    assert_eq!(ring.flagged(), [(0, hazard)]);
    assert_eq!(ring.flagged_count(), 1);
}

#[test]
fn the_registry_gives_one_ring_per_pe() {
    let first = ring_for(MAX_PES - 1).unwrap();
    let again = ring_for(MAX_PES - 1).unwrap();
    assert!(core::ptr::eq(first, again));
    assert_eq!(first.pe(), MAX_PES - 1);
    assert!(ring_for(MAX_PES).is_none());
    assert!(registered().any(|ring| core::ptr::eq(ring, first)));
}

#[test]
fn entries_format_one_line_each() {
    let irq = Entry {
        retired: 1234,
        ..exception(2, 0xffff_fe00_3a85_5b7c)
    };
    assert_eq!(
        format(5, &irq),
        "EVENT RING pe=2 #5 EXC irq el1->el1 elr=0xfffffe003a855b7c esr=0x56000000 far=0x0 \
         retired=1234"
    );
    let eret = Entry {
        kind: Kind::Return,
        from_el: 1,
        to_el: 0,
        vector: 0,
        detail: 0x0,
        pc: 0xffff_fe00_3a67_3e6c,
        other: 0x1_86e5_7c20,
        retired: 9,
        pe: 0,
    };
    assert_eq!(
        format(1, &eret),
        "EVENT RING pe=0 #1 ERET el1->el0 at=0xfffffe003a673e6c to=0x186e57c20 spsr=0x0 retired=9"
    );
    let recovery = Entry {
        kind: Kind::Recovery,
        pe: 4,
        from_el: 1,
        to_el: 1,
        vector: RECOVERY_WRITE | RECOVERY_HAZARD,
        detail: 0x3c,
        pc: 0xffff_fe00_3a6e_2a10,
        other: 0xffff_fe18_8664_8c22,
        retired: 77,
    };
    assert_eq!(
        format(2, &recovery),
        "EVENT RING pe=4 #2 RECOVER REPLAY-HAZARD el1 settled=0xfffffe003a6e2a10 \
         fault=0xfffffe1886648c22 write native+0x3c retired=77"
    );
}

#[test]
fn memory_effects_classify_loads_stores_and_plain_arithmetic() {
    // ldrb w8, [x0, #0xa]; add w8, w8, #1; stlurb w8, [x0, #0xa]
    assert_eq!(memory_effect(0x3940_2808), MemoryEffect::Reads);
    assert_eq!(memory_effect(0x1100_0508), MemoryEffect::None);
    assert_eq!(memory_effect(0x1900_a008), MemoryEffect::Writes);
    // ldadda w8, w8, [x1]; str wzr, [x0, #0xc]
    assert_eq!(memory_effect(0xb8a8_0028), MemoryEffect::Writes);
    assert_eq!(memory_effect(0xb900_0c1f), MemoryEffect::Writes);
    // ldxr w20, [x19]; ldar w0, [x1]; ldp x0, x1, [x2]: loads native code
    // may make atomic with a locked compare-exchange, which faults as a write.
    assert_eq!(memory_effect(0x885f_7e74), MemoryEffect::Unknown);
    assert_eq!(memory_effect(0x88df_fc20), MemoryEffect::Unknown);
    assert_eq!(memory_effect(0xa940_0440), MemoryEffect::Unknown);
    assert!(!replay_hazard(0x885f_7e74, true));
}

#[test]
fn a_write_fault_settled_at_a_load_or_arithmetic_is_a_replay_hazard() {
    // The unlock's store faulting but settled at its load replays the add.
    assert!(replay_hazard(0x3940_2808, true));
    assert!(!replay_hazard(0x3940_2808, false));
    assert!(replay_hazard(0x1100_0508, false));
    assert!(!replay_hazard(0x1900_a008, true));
    assert!(!replay_hazard(0xb8a8_0028, true));
    // An undecodable word draws no conclusion.
    assert!(!replay_hazard(0, true));
}

#[test]
fn the_ticket_panic_names_its_lock() {
    let message = "Ticket spinlock[0xfffffe1886648c20] timeout after 3197036 ticks; \
                   cticket: 0xbb, nticket: 0xbd, waiting for 0xf4";
    assert_eq!(ticket_lock_address(message), Some(0xffff_fe18_8664_8c20));
    assert_eq!(ticket_lock_address("Kernel data abort."), None);
    assert_eq!(ticket_lock_address("Ticket spinlock[0xzz]"), None);
}

#[test]
fn the_lock_line_shows_its_bytes_and_ticket_fields() {
    let mut bytes = [0u8; 64];
    bytes[0x20..0x28].copy_from_slice(&[0x11, 0x01, 0xbb, 0xbd, 0x34, 0x12, 0, 0]);
    let line = format_lock_line(0xffff_fe18_8664_8c20, 0xffff_fe18_8664_8c00, &bytes);
    assert!(line.starts_with(
        "PANIC LOCK va=0xfffffe1886648c20 line=0xfffffe1886648c00 bytes=0000"
    ));
    assert!(line.contains("1101bbbd34120000"));
    assert!(line.ends_with(
        " type=0x11 flags=0x1 cticket=0xbb nticket=0xbd owner_ctid=0x1234"
    ));
}

#[test]
fn a_machine_records_the_exceptions_and_returns_its_engine_runs() {
    use super::super::tests::{BASE, machine};
    use crate::machine::system::Register as SystemRegister;
    let mut machine = machine(&[]);
    machine.set_event_ring(true);
    assert!(machine.event_ring());
    machine
        .cpu
        .registers
        .set(SystemRegister::VBAR_EL1, 0, 0x1_0000);
    // svc #0x42 at EL1h, then ERET to EL0t at its ELR.
    machine.system_instruction(0xd400_0841).unwrap();
    let elr = machine.cpu.read(SystemRegister::ELR_EL1);
    assert_eq!(elr, BASE + 4);
    machine.cpu.registers.set(SystemRegister::SPSR_EL1, 0, 0);
    machine.system_instruction(0xd69f_03e0).unwrap();
    let recent = ring_for(0).unwrap().recent(ENTRIES);
    assert!(
        recent.iter().any(|(_, entry)| entry.kind == Kind::Exception
            && entry.vector == 0
            && (entry.from_el, entry.to_el) == (1, 1)
            && entry.detail == 0x5600_0042
            && entry.pc == elr),
        "{recent:x?}"
    );
    assert!(
        recent.iter().any(|(_, entry)| entry.kind == Kind::Return
            && (entry.from_el, entry.to_el) == (1, 0)
            && entry.pc == 0x1_0200
            && entry.other == elr),
        "{recent:x?}"
    );
    machine.set_event_ring(false);
    assert!(!machine.event_ring());
}
