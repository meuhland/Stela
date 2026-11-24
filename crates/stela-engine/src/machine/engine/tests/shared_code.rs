//! Host model of a launch storm's shared code: processes, each its own
//! ASID, map the same frames (the dyld shared cache, one copy of every
//! system framework) at the same VAs, as every process of one boot does,
//! and run the same blocks. A block's key is its PC, its frame and its
//! variant, with no address space in it, so the board compiles each block
//! once for all of them. The same frames mapped at another VA are compiled
//! again: a block's meaning depends on its PC (ADR, ADRP, BL targets and
//! return addresses), so that is required, not lost sharing.

use super::*;

const MSR_TTBR0_EL1_X0: u32 = 0xd518_2000;
const RET: u32 = 0xd65f_03c0;
const PAGE: u64 = 0x1000;
/// Shared pages every process runs, and blocks in each.
const PAGES: u64 = 4;
const BLOCKS: u64 = 8;
const PROCESSES: u64 = 32;

/// Fetch every block of the shared pages from `base`.
fn run_shared(machine: &mut Machine<FakeHost, NoDevices>, base: u64) {
    for page in 0..PAGES {
        for block in 0..BLOCKS {
            let pc = base + page * PAGE + 4 * block;
            assert!(matches!(
                machine.fetch_block(pc, false),
                Ok(Fetched::Block(_))
            ));
        }
    }
}

/// Blocks compiled so far: fetches that found no block.
fn compiled(machine: &Machine<FakeHost, NoDevices>) -> u64 {
    machine.statistics.fetches[4]
}

#[test]
fn processes_mapping_shared_code_at_one_address_compile_it_once() {
    let mut machine = machine(&[RET; (PAGES * PAGE / 4) as usize]);
    let frames: Vec<u64> = (0..PAGES).map(|page| BASE + page * PAGE).collect();
    paged_space(&mut machine, 0, &frames);
    let ttbr0 = machine.cpu.read(SystemRegister::TTBR0_EL1);
    let mut per_process = Vec::new();
    for process in 0..PROCESSES {
        // Another process: another ASID, the same mappings.
        machine.cpu.state.x[0] = ttbr0 & !(0xff << 48) | process << 48;
        machine.system_instruction(MSR_TTBR0_EL1_X0).unwrap();
        let before = compiled(&machine);
        run_shared(&mut machine, BASE);
        per_process.push(compiled(&machine) - before);
    }
    assert_eq!(per_process[0], PAGES * BLOCKS, "the first process compiles");
    assert!(
        per_process[1..].iter().all(|&count| count == 0),
        "later processes compile nothing: {per_process:?}"
    );
    assert_eq!(machine.code.blocks.live() as u64, PAGES * BLOCKS);

    // The same frames at another VA (2 MiB up) in another table: each
    // block is compiled again for its own PC, and only once.
    let [_, l2, l3] = SPACES_TABLES[1];
    paged_space(&mut machine, 1, &frames);
    write_entry(&mut machine, l2 + 8, (BASE + l3) | 0b11);
    let moved = BASE + (1 << 21);
    let before = compiled(&machine);
    run_shared(&mut machine, moved);
    assert_eq!(compiled(&machine) - before, PAGES * BLOCKS);
    let before = compiled(&machine);
    run_shared(&mut machine, moved);
    run_shared(&mut machine, BASE);
    assert_eq!(compiled(&machine), before);
    assert_eq!(machine.code.blocks.live() as u64, 2 * PAGES * BLOCKS);
}

/// `words` from BASE, with the frames of [`PAGES`] pages mapped at BASE and
/// again 2 MiB up (another process's slide of the same binary), on a
/// machine that compiles page-relative code and shares it among the pages
/// that map a frame (`--shared-aliases`) when `shared`: the machine, and
/// the second mapping's base.
fn aliased(words: &[u32], shared: bool) -> (Machine<FakeHost, NoDevices>, u64) {
    let mut padded = words.to_vec();
    padded.resize((PAGES * PAGE / 4) as usize, RET);
    let mut machine = machine(&padded);
    machine.set_page_relative(true);
    machine.set_shared_aliases(shared);
    let frames: Vec<u64> = (0..PAGES).map(|page| BASE + page * PAGE).collect();
    let [_, l2, l3] = SPACES_TABLES[0];
    paged_space(&mut machine, 0, &frames);
    write_entry(&mut machine, l2 + 8, (BASE + l3) | 0b11);
    (machine, BASE + (1 << 21))
}

/// The block a fetch at `pc` runs.
fn fetched(machine: &mut Machine<FakeHost, NoDevices>, pc: u64) -> usize {
    match machine.fetch_block(pc, false) {
        Ok(Fetched::Block(index)) => index,
        other => panic!("a block at {pc:#x}: {:?}", other.is_ok()),
    }
}

/// The block table entry of `pc`, as its page's key finds it in either
/// place of its set: its PC and block.
fn table_entry(machine: &Machine<FakeHost, NoDevices>, pc: u64) -> (u64, u32) {
    let key = machine.current_key(pc).expect("a translated page");
    let table = &machine.code.table;
    let entry = table.find(table.slot(pc, key), pc, key);
    (entry.pc, entry.block)
}

/// Shared aliases are off unless asked for, and need page-relative code:
/// the same frame at another page then compiles a block of its own.
#[test]
fn shared_aliases_are_off_by_default_and_need_page_relative_code() {
    let plain = machine(&[RET]);
    assert!(!plain.shared_aliases());
    let mut alone = machine(&[RET]);
    alone.set_shared_aliases(true);
    assert!(!alone.shared_aliases(), "only with page-relative code");

    let (mut machine, moved) = aliased(&[], false);
    assert!(!machine.shared_aliases());
    let first = fetched(&mut machine, BASE + 8);
    let second = fetched(&mut machine, moved + 8);
    assert_ne!(first, second);
    assert!(!machine.code.blocks[first].shared && !machine.code.blocks[second].shared);
    assert_eq!(compiled(&machine), 2);
    assert_eq!(machine.statistics.aliases, [0; Alias::COUNT]);
}

/// Two guest pages that map one frame run one block: the second page's
/// fetch finds the block the first installed, at its own PC's offset, and
/// takes a block table entry of its own PC naming the same slot; a run
/// there enters with that page's base.
#[test]
fn two_aliases_of_a_frame_resolve_to_one_shared_block() {
    let (mut machine, moved) = aliased(&[], true);
    assert!(machine.shared_aliases());
    let first = fetched(&mut machine, BASE + 8);
    let second = fetched(&mut machine, moved + 8);
    assert_eq!(first, second, "one body for both pages");
    let block = &machine.code.blocks[first];
    assert!(block.shared);
    assert_eq!(block.pc, BASE + 8, "installed for the first page");
    assert_eq!(block.start_at(moved + 8), moved + 8);
    assert_eq!(block.start_at(BASE + 8), BASE + 8);
    assert_eq!(compiled(&machine), 1, "the second page installs nothing");
    assert_eq!(machine.statistics.aliases[Alias::Installs as usize], 1);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], 1);
    for pc in [BASE + 8, moved + 8] {
        assert_eq!(table_entry(&machine, pc), (pc, first as u32));
    }
    // Another offset of the frame is another block; the same offset of
    // the frame in the other half of the address space is not served.
    let other = fetched(&mut machine, moved + 16);
    assert_ne!(other, first);
    assert!(!machine.code.blocks[first].serves(moved + 8 + (1 << 55), true));
    assert!(!machine.code.blocks[first].serves(moved + 8, false));

    // A run at the second page enters with its page base, and its exit is
    // the second page's.
    machine.cpu.pc = moved + 8;
    machine.cpu.state.x[30] = moved + 16;
    machine.host.exits = vec![NativeExit::Exit {
        next_pc: moved + 16,
        kind: ExitKind::Return as u64,
    }];
    machine.step().unwrap();
    assert_eq!(machine.cpu.state.page_base.0, moved);
    assert_eq!(machine.running_pc(first), moved + 8);
    assert_eq!(machine.cpu.pc, moved + 16);
}

/// A shared block's link in its page enters a shared block, and the
/// retirement of a shared block forgets it at every page that ran it: the
/// block table entries of each PC, its index entry, and the links into it
/// from blocks installed for another page.
#[test]
fn retiring_a_shared_block_forgets_it_at_every_page() {
    const B_8: u32 = 0x1400_0002; // b .+8
    let (mut machine, moved) = aliased(&[B_8, 0, RET], true);
    // The RET's block, installed for the first page.
    let ret = fetched(&mut machine, BASE + 8);
    assert!(machine.code.blocks[ret].shared);
    // The branch's block, installed for the second page, exits to its
    // page's RET and links into the shared block there: the one installed
    // for the first page.
    machine.cpu.pc = moved;
    machine.host.exits = vec![NativeExit::Exit {
        next_pc: moved + 8,
        kind: ExitKind::Direct as u64,
    }];
    machine.step().unwrap();
    assert_eq!(machine.cpu.pc, moved + 8);
    let branch = fetched(&mut machine, moved);
    assert_eq!(machine.code.blocks[branch].pc, moved);
    assert!(machine.code.blocks[branch].shared);
    assert_eq!(machine.statistics.links, 1);
    assert!(machine.code.blocks.record(branch).chained(0));
    // Each page runs both, through entries of its own PCs.
    assert_eq!(fetched(&mut machine, moved + 8), ret);
    assert_eq!(fetched(&mut machine, BASE), branch);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], 2);
    for pc in [BASE + 8, moved + 8] {
        assert_eq!(table_entry(&machine, pc), (pc, ret as u32));
    }
    let (physical, variant) = {
        let block = &machine.code.blocks[ret];
        (block.physical, block.variant)
    };

    // The RET's code goes: the branch's link into it, made at another page
    // than the RET's own, is cut by its offset.
    let code = machine.code.blocks[ret].code.0;
    machine.evict_code(code..code + 1);
    assert!(!machine.code.blocks.record(ret).live());
    assert!(
        !machine.code.blocks.record(branch).chained(0),
        "the link into it goes"
    );
    for pc in [BASE + 8, moved + 8] {
        assert_ne!(table_entry(&machine, pc), (pc, ret as u32), "{pc:#x}");
        assert_eq!(machine.code.find(pc, physical, variant, true), None);
    }
    assert!(
        !machine
            .code
            .cache()
            .index
            .contains(&machine.code.blocks, ret)
    );
    // The branch stays, at both pages.
    assert_eq!(fetched(&mut machine, BASE), branch);
    assert_eq!(fetched(&mut machine, moved), branch);
}

/// A block that ends in an exit an install-time thunk completes for its
/// own PC (here SVC's transition) stays one PC's, as does a block of a
/// page with a host breakpoint.
#[test]
fn an_excluded_block_stays_one_pcs() {
    const SVC: u32 = 0xd400_0001;
    let (mut machine, moved) = aliased(&[SVC], true);
    let first = fetched(&mut machine, BASE);
    let second = fetched(&mut machine, moved);
    assert_ne!(first, second);
    assert!(!machine.code.blocks[first].shared);
    assert!(!machine.code.blocks[second].shared);
    assert_eq!(compiled(&machine), 2);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], 0);

    let (mut machine, moved) = aliased(&[], true);
    machine.breakpoints = vec![BASE + 0x40];
    let first = fetched(&mut machine, BASE + 8);
    assert!(!machine.code.blocks[first].shared, "a breakpoint's page");
    let second = fetched(&mut machine, moved + 8);
    assert_ne!(first, second);
    assert!(machine.code.blocks[second].shared);
    // The breakpoint's page takes no shared block either.
    assert_ne!(
        fetched(&mut machine, BASE + 12),
        fetched(&mut machine, moved + 12)
    );
}

/// [`aliased`], with sharing narrowed by `limits`.
fn limited(words: &[u32], limits: SharedLimits) -> (Machine<FakeHost, NoDevices>, u64) {
    let (mut machine, moved) = aliased(words, true);
    machine.set_shared_limits(limits);
    (machine, moved)
}

/// The shared diagnostics `machine` reported.
fn shared_events(machine: &Machine<FakeHost, NoDevices>) -> Vec<SharedEvent> {
    machine
        .host
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Shared(shared) => Some(*shared),
            _ => None,
        })
        .collect()
}

/// `--shared-verify` checks an alias PC once before the shared block runs
/// there: the same bytes and code there let it run.
#[test]
fn a_verified_alias_runs_the_shared_block() {
    let limits = SharedLimits {
        verify: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[0x1000_0200, RET], limits); // ADR X0,#0x40
    let first = fetched(&mut machine, BASE);
    assert_eq!(fetched(&mut machine, moved), first);
    assert_eq!(machine.statistics.aliases[Alias::Verified as usize], 1);
    assert_eq!(machine.statistics.aliases[Alias::Mismatched as usize], 0);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], 1);
    assert!(shared_events(&machine).is_empty());
}

/// An alias whose frame no longer holds the block's bytes (written with no
/// IC IVAU) is refused: reported once, and the PC compiles a block of its
/// own, which serves it from then on.
#[test]
fn a_mismatched_alias_is_reported_and_gets_its_own_block() {
    const ADD: u32 = 0x9100_0400; // add x0, x0, #1
    let limits = SharedLimits {
        verify: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[ADD, RET], limits);
    let first = fetched(&mut machine, BASE);
    assert!(machine.code.blocks[first].shared);
    // The frame's first word changes behind the block's back.
    let at = (machine.code.blocks[first].physical - BASE) as usize;
    machine.host.ram[at..at + 4].copy_from_slice(&0x9100_0800u32.to_le_bytes());
    let second = fetched(&mut machine, moved);
    assert_ne!(second, first);
    assert!(!machine.code.blocks[second].shared);
    let events = shared_events(&machine);
    assert_eq!(events.len(), 1);
    assert_eq!(
        (events[0].pc, events[0].installed_pc, events[0].mismatch),
        (moved, BASE, Some("bytes"))
    );
    assert_eq!(machine.statistics.aliases[Alias::Mismatched as usize], 1);
    assert_eq!(fetched(&mut machine, moved), second);
    assert_eq!(shared_events(&machine).len(), 1, "reported once");
}

/// `--shared-own-pc` installs shared blocks that serve their own PC alone;
/// `--shared-el0` shares nothing fetched above EL0; `--shared-log` reports
/// alias hits at EL0 only.
#[test]
fn bisection_limits_narrow_sharing() {
    let own = SharedLimits {
        own_pc: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[], own);
    let first = fetched(&mut machine, BASE + 8);
    let second = fetched(&mut machine, moved + 8);
    assert_ne!(first, second);
    assert!(machine.code.blocks[first].shared && machine.code.blocks[second].shared);
    assert_eq!(fetched(&mut machine, BASE + 8), first);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], 0);

    let el0 = SharedLimits {
        el0_only: true,
        log: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[], el0);
    assert_ne!(machine.cpu.pstate.el, 0);
    let first = fetched(&mut machine, BASE + 8);
    assert!(!machine.code.blocks[first].shared);
    assert_ne!(fetched(&mut machine, moved + 8), first);

    let log = SharedLimits {
        log: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[], log);
    let first = fetched(&mut machine, BASE + 8);
    assert_eq!(fetched(&mut machine, moved + 8), first);
    assert!(shared_events(&machine).is_empty(), "above EL0");
}

/// `--shared-no-table` leaves an alias PC no block table entry, and
/// `--shared-no-links` links no shared block in its page but through a
/// cell.
#[test]
fn table_and_link_limits_keep_aliases_in_the_engine() {
    let limits = SharedLimits {
        no_table: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[], limits);
    let first = fetched(&mut machine, BASE + 8);
    assert_eq!(fetched(&mut machine, moved + 8), first);
    assert_eq!(fetched(&mut machine, moved + 8), first);
    assert_eq!(table_entry(&machine, BASE + 8), (BASE + 8, first as u32));
    assert_ne!(table_entry(&machine, moved + 8), (moved + 8, first as u32));

    const B_8: u32 = 0x1400_0002; // b .+8
    let limits = SharedLimits {
        no_links: true,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&[B_8, 0, RET], limits);
    let ret = fetched(&mut machine, BASE + 8);
    machine.cpu.pc = moved;
    machine.host.exits = vec![NativeExit::Exit {
        next_pc: moved + 8,
        kind: ExitKind::Direct as u64,
    }];
    machine.step().unwrap();
    assert_eq!(fetched(&mut machine, moved + 8), ret);
    // Its exit goes through a cell instead of a direct link.
    let branch = fetched(&mut machine, moved);
    let record = machine.code.blocks.record(branch);
    assert!(record.linked(0) && !record.chained(0));
}

/// A shared block's exit in its page into a block of one PC (here one
/// ending in SVC) cannot be a direct link, which would enter that PC's
/// body at every page: it goes through a cell, once, and is settled, so
/// its later exits take no lock to try again (shared1 stalled so). With
/// `--shared-own-pc`, which runs no block at another page, it links
/// directly, as a block of one PC does.
#[test]
fn a_shared_exit_into_a_block_of_one_pc_goes_through_a_cell_once() {
    const B_8: u32 = 0x1400_0002; // b .+8
    const SVC: u32 = 0xd400_0001;
    for own_pc in [false, true] {
        let limits = SharedLimits {
            own_pc,
            ..SharedLimits::default()
        };
        let (mut machine, _) = limited(&[B_8, 0, SVC], limits);
        let target = fetched(&mut machine, BASE + 8);
        assert!(!machine.code.blocks[target].shared);
        machine.cpu.pc = BASE;
        machine.host.exits = vec![NativeExit::Exit {
            next_pc: BASE + 8,
            kind: ExitKind::Direct as u64,
        }];
        machine.step().unwrap();
        let branch = fetched(&mut machine, BASE);
        assert!(machine.code.blocks[branch].shared);
        let record = machine.code.blocks.record(branch);
        assert_eq!(machine.statistics.links, 1, "own_pc={own_pc}");
        assert!(record.linked(0), "settled or chained: no retry");
        assert_eq!(record.chained(0), own_pc, "a direct link only own-pc");
        assert_eq!(
            machine
                .code
                .cache()
                .targets
                .site_cell(chain::direct_patch(
                    machine.code.blocks[branch].successors[0].site
                ))
                .is_some(),
            !own_pc,
            "a cell otherwise"
        );
    }
}

const RETAA: u32 = 0xd65f_0bff;
const SVC: u32 = 0xd400_0001; // svc #0
const ERET: u32 = 0xd69f_03e0;
const MRS_GXF_STATUS: u32 = 0xd53e_f808; // mrs x8, gxf_status_el1
const BLRAA_X8_X16: u32 = 0xd73f_0910;

/// Blocks a thunk completes (`--shared-thunks`), one instruction each, by
/// their offset in the frames: RETAA, SVC, ERET and MRS of a mirrored
/// register, then MRS as the last instruction of the first page, whose
/// next instruction is the next page's first, and BLRAA as the second
/// page's last, whose link is the third page's first.
const THUNKED: [(u64, u32); 6] = [
    (0x0, RETAA),
    (0x4, SVC),
    (0x8, ERET),
    (0xc, MRS_GXF_STATUS),
    (0xffc, MRS_GXF_STATUS),
    (0x1ffc, BLRAA_X8_X16),
];

/// [`aliased`] with the [`THUNKED`] blocks, sharing them with
/// `shared_thunks` (`--shared-thunks`), on a host whose counter native
/// code reads, as ERET's thunk needs.
fn thunked(shared_thunks: bool) -> (Machine<FakeHost, NoDevices>, u64) {
    let mut words = vec![RET; (2 * PAGE / 4) as usize];
    for (offset, word) in THUNKED {
        words[(offset / 4) as usize] = word;
    }
    let limits = SharedLimits {
        thunks: shared_thunks,
        ..SharedLimits::default()
    };
    let (mut machine, moved) = limited(&words, limits);
    machine.host.tsc_counter = Some((5, 7));
    (machine, moved)
}

/// The code of the thunk the terminal exit of block `index` jumps to.
fn thunk_of(machine: &Machine<FakeHost, NoDevices>, index: usize) -> Vec<u8> {
    let code = machine.code.blocks[index].code.0;
    let (at, jump) = machine
        .host
        .patches
        .iter()
        .find(|(at, bytes)| (code..code + NATIVE_CODE_STRIDE).contains(at) && bytes[0] == 0xe9)
        .expect("a jump to a thunk");
    let relative = i32::from_le_bytes(jump[1..5].try_into().unwrap());
    let thunk = (*at as i64 + 5 + i64::from(relative)) as usize;
    assert_eq!(thunk % NATIVE_CODE_STRIDE, 0, "a thunk's install");
    machine.host.installed[thunk / NATIVE_CODE_STRIDE - 1].clone()
}

/// A block whose exit a thunk completes (PAC, transition and mirrored-read
/// thunks) stays one PC's without `--shared-thunks`; with it, the second
/// page that maps the frame runs the block and thunk the first installed,
/// compiling and installing nothing.
#[test]
fn thunk_exits_share_only_with_shared_thunks() {
    let (mut machine, moved) = thunked(false);
    for (offset, _) in THUNKED {
        let first = fetched(&mut machine, BASE + offset);
        let second = fetched(&mut machine, moved + offset);
        assert_ne!(first, second, "{offset:#x}");
        assert!(!machine.code.blocks[first].shared && !machine.code.blocks[second].shared);
        thunk_of(&machine, first);
    }
    assert_eq!(machine.statistics.aliases[Alias::Installs as usize], 0);

    let (mut machine, moved) = thunked(true);
    for (offset, word) in THUNKED {
        let first = fetched(&mut machine, BASE + offset);
        assert!(machine.code.blocks[first].shared, "{offset:#x}");
        assert_eq!(machine.code.blocks[first].terminal, word);
        thunk_of(&machine, first);
        let (installed, compiled_so_far) = (machine.host.installed.len(), compiled(&machine));
        assert_eq!(fetched(&mut machine, moved + offset), first, "{offset:#x}");
        assert_eq!(machine.host.installed.len(), installed, "no install");
        assert_eq!(compiled(&machine), compiled_so_far);
    }
    let count = THUNKED.len() as u64;
    assert_eq!(machine.statistics.aliases[Alias::Installs as usize], count);
    assert_eq!(machine.statistics.aliases[Alias::Hits as usize], count);
}

/// A shared block's thunk, installed for the first page, completes at the
/// second page as the second page's own block and thunk do without
/// `--shared-thunks`: RETAA's and BLRAA's targets and link, MRS's next PC
/// (the next page's first for a page's last instruction) and register, and
/// SVC's and ERET's exits to the engine, after which ELR_EL1, SPSR_EL1 and
/// the PC are the same. A refused RETAA exits at the second page's PC.
#[cfg(all(target_arch = "x86_64", any(target_os = "macos", target_os = "linux")))]
#[test]
fn shared_thunks_complete_at_the_page_they_run_at() {
    use super::super::transitions::tests::run_native;
    use stela_compiler::PageBase;
    let status = MIRRORED
        .iter()
        .position(|&encoding| encoding == registers::encodings::GXF_STATUS_EL1)
        .unwrap();
    let ia = usize::from(x86_64_pointer::key_index(PointerAuthenticationKey::Ia));
    let observed = |machine: &Machine<FakeHost, NoDevices>| {
        let read = |register| machine.cpu.registers.get(register, 0);
        alloc::format!(
            "pc={:#x} x={:x?} sp={:#x} {:?} elr={:#x} spsr={:#x} esr={:#x}",
            machine.cpu.pc,
            machine.cpu.state.x,
            machine.cpu.state.sp,
            machine.cpu.pstate,
            read(SystemRegister::ELR_EL1),
            read(SystemRegister::SPSR_EL1),
            read(SystemRegister::ESR_EL1),
        )
    };
    for (offset, word) in THUNKED {
        for refused in [false, true] {
            let ((mut shared, moved), (mut own, _)) = (thunked(true), thunked(false));
            let first = fetched(&mut shared, BASE + offset);
            assert_eq!(fetched(&mut shared, moved + offset), first);
            let installed = fetched(&mut own, moved + offset);
            assert!(!own.code.blocks[installed].shared);
            let (pc, page) = (moved + offset, (moved + offset) & !(PAGE - 1));
            let mut exits = Vec::new();
            for (machine, index) in [(&mut shared, first), (&mut own, installed)] {
                let thunk = thunk_of(machine, index);
                machine.cpu.pc = pc;
                machine.cpu.state.x[8] = 0x0000_7000_0000_8000;
                machine.cpu.state.x[16] = 0x10;
                machine.cpu.state.x[30] = 0x0000_7000_0000_3000;
                machine.cpu.state.page_base = PageBase(page);
                machine.pointer.tags[ia] = if refused { 0 } else { x86_64_pointer::DISABLED };
                machine.mirrors[status] = register_read::Cell {
                    readable: 1,
                    value: 0x5a5a,
                };
                let registers = &mut machine.cpu.registers;
                registers.set(SystemRegister::SPSR_EL1, 0, 0);
                registers.set(SystemRegister::ELR_EL1, 0, 0x0000_7000_0000_4000);
                let (next, kind) = run_native(machine, &thunk);
                machine.cpu.pc = next;
                if kind == ExitKind::System as u64 {
                    assert_eq!(next, pc, "the original exit at this page");
                    machine.system_instruction(word).unwrap();
                }
                exits.push((next, kind));
            }
            assert_eq!(exits[0], exits[1], "{offset:#x}");
            assert_eq!(observed(&shared), observed(&own), "{offset:#x}");
            let (next, kind) = exits[0];
            let elr = shared.cpu.registers.get(SystemRegister::ELR_EL1, 0);
            match word {
                RETAA if refused => {
                    assert_eq!((next, kind), (pc, ExitKind::PointerAuthentication as u64))
                }
                RETAA => assert_eq!(
                    (next, kind),
                    (0x0000_7000_0000_3000, ExitKind::Return as u64)
                ),
                BLRAA_X8_X16 if !refused => {
                    assert_eq!((next, kind), (0x0000_7000_0000_8000, ExitKind::Call as u64));
                    assert_eq!(shared.cpu.state.x[30], pc + 4, "the third page's first PC");
                }
                SVC => assert_eq!(elr, pc + 4),
                MRS_GXF_STATUS => {
                    assert_eq!((next, kind), (pc + 4, ExitKind::Fallthrough as u64));
                    assert_eq!(shared.cpu.state.x[8], 0x5a5a);
                }
                _ => {}
            }
        }
    }
}

/// A shared thunk's direct exit links by the rules of every shared block's:
/// in its page into the shared block of its target, and out of it (the
/// next page, for a page's last instruction) through a cell, whose probe
/// compares the target, never a stamped tail.
#[test]
fn shared_thunk_successors_link_as_shared_blocks_do() {
    let (mut machine, moved) = thunked(true);
    // In the page: MRS at 0xc, then the RET at 0x10, both shared.
    machine.cpu.pc = moved + 0xc;
    machine.host.exits = vec![
        NativeExit::Exit {
            next_pc: moved + 0x10,
            kind: ExitKind::Fallthrough as u64,
        },
        NativeExit::Exit {
            next_pc: moved + 0x14,
            kind: ExitKind::Return as u64,
        },
    ];
    machine.step().unwrap();
    machine.step().unwrap();
    let mrs = fetched(&mut machine, moved + 0xc);
    let ret = fetched(&mut machine, moved + 0x10);
    assert!(machine.code.blocks[mrs].shared && machine.code.blocks[ret].shared);
    assert_eq!(machine.statistics.links, 1);
    assert!(machine.code.blocks.record(mrs).chained(0));
    assert_eq!(fetched(&mut machine, BASE + 0xc), mrs);
    // Out of the page: the next page's first PC, through a cell.
    let (links, stamped) = (machine.statistics.links, machine.statistics.stamped_tails);
    machine.cpu.pc = moved + 0xffc;
    machine.host.exits = vec![NativeExit::Exit {
        next_pc: moved + PAGE,
        kind: ExitKind::Fallthrough as u64,
    }];
    machine.step().unwrap();
    assert_eq!(machine.cpu.pc, moved + PAGE);
    let last = fetched(&mut machine, moved + 0xffc);
    let block = &machine.code.blocks[last];
    assert!(block.shared);
    assert_eq!(block.successors[0].target, GuestAddress(block.pc + 4));
    let point = chain::direct_patch(block.successors[0].site);
    assert_eq!(machine.statistics.links, links + 1);
    assert_eq!(machine.statistics.stamped_tails, stamped);
    assert!(
        machine
            .code
            .cache()
            .targets
            .site_cell_number(point)
            .is_some()
    );
}
