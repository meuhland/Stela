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
