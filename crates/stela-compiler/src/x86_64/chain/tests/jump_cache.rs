//! The jump cache of [`lookup_tail`]s: a target the table probe entered is
//! entered from the tail across native runs, and every change the probe
//! would see (a TLBI of the page, a rekeyed page, a block's release or
//! replacement, a context switch, a native transition's new contexts) takes
//! the entry with it. Random runs against the plain tail.
use super::super::*;
use crate::GuestState;
use alloc::{boxed::Box, vec::Vec};
use core::ffi::c_void;
unsafe extern "C" {
    fn mmap(
        address: *mut c_void,
        bytes: usize,
        protection: i32,
        flags: i32,
        fd: i32,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, bytes: usize) -> i32;
}

const PAGE: usize = 4096;
/// The cached tail's site, the plain tail's, the probes of BTYPE 0 and 2.
const CACHED: usize = 64;
const PLAIN: usize = 256;
const PROBES: usize = 1024;
const BRANCH_CACHED: usize = 384;
const BRANCH_PROBES: usize = 2048;
const REGION: usize = 4 * PAGE;
const BLOCKS: usize = 8;
const RAN: u32 = 3;
const CONTEXTS: [u64; 2] = [5 << LOOKUP_SALT_BITS, 9 << LOOKUP_SALT_BITS];

struct Pages(*mut u8, usize);
impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: our mapping, nothing runs it any more.
        assert_eq!(unsafe { munmap(self.0.cast(), self.1) }, 0);
    }
}
fn pages(bytes: usize) -> Pages {
    // SAFETY: private anonymous memory, unmapped on drop.
    let base = unsafe { mmap(core::ptr::null_mut(), bytes, 7, 0x22, -1, 0) };
    assert_ne!(base as usize, usize::MAX);
    Pages(base.cast(), bytes)
}
fn put(pages: &Pages, offset: usize, bytes: &[u8]) {
    assert!(offset + bytes.len() <= pages.1);
    // SAFETY: inside the mapping; nothing runs it while it is written.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), pages.0.add(offset), bytes.len()) };
}

/// Blocks that return their marker in RAX and 99 in EDX.
fn blocks() -> Pages {
    let blocks = pages(PAGE);
    for block in 0..BLOCKS {
        let mut code = vec![0x48, 0xb8];
        code.extend((0x7700 + block as u64).to_le_bytes());
        code.extend([0xba, 99, 0, 0, 0, 0xc3]);
        put(&blocks, 64 * block, &code);
    }
    blocks
}

/// `mov rax, rsi` and a jump to the tail at `site`, at the region's start
/// and at `site - 16`; the cached tails, the plain tail and the probes.
fn region() -> Pages {
    let region = pages(REGION);
    let base = region.0 as u64;
    for (stub, site) in [(0, CACHED), (16, PLAIN), (32, BRANCH_CACHED)] {
        let mut code = vec![0x48, 0x89, 0xf0];
        code.extend(jump(base + stub as u64 + 3, base + site as u64).unwrap());
        put(&region, stub, &code);
    }
    let word = ExitWord::from(ExitKind::Return);
    let probe = base + (PROBES + PROBE_ENTRY) as u64;
    put(
        &region,
        CACHED,
        &lookup_tail(base + CACHED as u64, word, RAN, probe, 0).unwrap(),
    );
    let mut plain = Code::new();
    tail(&mut plain, base + PLAIN as u64, word, RAN, probe).unwrap();
    put(&region, PLAIN, &plain);
    let branch = ExitWord::from(ExitKind::Indirect);
    let branch_probe = base + (BRANCH_PROBES + PROBE_ENTRY) as u64;
    put(
        &region,
        BRANCH_CACHED,
        &lookup_tail(base + BRANCH_CACHED as u64, branch, RAN, branch_probe, 2).unwrap(),
    );
    put(&region, PROBES, &lookup_dispatcher(0).unwrap());
    put(&region, BRANCH_PROBES, &lookup_dispatcher(2).unwrap());
    region
}

/// Run the stub at `stub` with the target in RSI: RAX and RDX it returns.
fn run(region: &Pages, stub: usize, control: &mut ChainControl, target: u64) -> (u64, u64) {
    let mut frame = Box::new(GuestState::default());
    frame.anchors[Anchor::Control] = control as *mut ChainControl as u64;
    // SAFETY: the region holds the stubs, tails and probes with the block
    // ABI; they touch the frame's control, the tables, the stamps and the
    // blocks, which outlive the call.
    let result =
        unsafe { crate::x86_64::native::enter(region.0.add(stub), &mut *frame, target, 0) };
    (result as u64, (result >> 64) as u64)
}

struct World {
    table: LookupTable,
    itlb: LookupTable,
    control: Box<ChainControl>,
    blocks: Pages,
    region: Pages,
}

impl World {
    fn new() -> Self {
        let mut table = LookupTable::new();
        let itlb = LookupTable::itlb();
        let stamps = table.new_stamps();
        let control = Box::new(ChainControl {
            budget: 1 << 20,
            table: table.address(),
            itlb: itlb.address(),
            contexts: CONTEXTS,
            salts: CONTEXTS.map(|context| itlb.salt(context)),
            masks: [table.mask(), itlb.mask()],
            stamps,
            ..ChainControl::default()
        });
        World {
            table,
            itlb,
            control,
            blocks: blocks(),
            region: region(),
        }
    }

    /// The page of `pc` translating to `key` and block `block` entered at
    /// `pc` there, accepting `targets`.
    fn map(&mut self, pc: u64, key: u64, block: usize, targets: u32) {
        let context = self.control.contexts[half(pc)];
        self.itlb.insert(
            self.itlb.page_slot(pc, context),
            LookupEntry::page(pc & !0xfff, context, key),
        );
        self.table.insert(
            self.table.slot(pc, key),
            LookupEntry {
                pc,
                key,
                entry: self.blocks.0 as u64 + 64 * block as u64,
                block: block as u32,
                targets,
            },
        );
    }

    fn run(&mut self, stub: usize, target: u64) -> (u64, u64) {
        run(&self.region, stub, &mut self.control, target)
    }
}

const fn exit(target: u64, kind: ExitKind) -> (u64, u64) {
    (target, kind as u64 | (RAN as u64) << 20)
}

/// A probe's transfer fills the entry, counted as a fill, which the next
/// transfer takes from the tail without the ITLB or the table, in later
/// native runs too: until a TLBI of the page grows the epoch of its half,
/// after which the forgotten page exits as the probe would.
#[test]
fn a_filled_entry_holds_until_its_page_goes() {
    const PC: u64 = 0x1_0000_0100;
    let filled = |world: &World| {
        // SAFETY: the world's stamps, whose table lives.
        let counts = unsafe { stamp_counts(world.control.stamps) };
        counts[StampCount::Filled as usize]
    };
    let mut world = World::new();
    world.map(PC, block_key(0x4000_0000, 1), 3, 0b1111);
    assert_eq!(world.run(0, PC), (0x7703, 99));
    assert_eq!(
        (
            world.control.hits,
            world.control.retired,
            world.control.block
        ),
        (1, u64::from(RAN), 3)
    );
    assert_eq!(filled(&world), 1);
    // Later runs take it from the tail.
    for hits in 2..5 {
        assert_eq!(world.run(0, PC), (0x7703, 99));
        assert_eq!(world.control.hits, hits);
    }
    assert_eq!(filled(&world), 1, "the cache's hit fills nothing");
    // A TLBI of the page: the probe and the cached tail both exit.
    let page = PC & !0xfff;
    assert!(world.itlb.forget_range(page..=page | 0xfff, u64::MAX));
    assert_eq!(world.run(16, PC), exit(PC, ExitKind::Return));
    assert_eq!(world.run(0, PC), exit(PC, ExitKind::Return));
    assert_eq!(world.control.hits, 4);
    // The page back, the probe fills again.
    world.map(PC, block_key(0x4000_0000, 1), 3, 0b1111);
    assert_eq!(world.run(0, PC), (0x7703, 99));
    assert_eq!(filled(&world), 2);
}

/// Each event that changes what the probe finds takes the entry with it:
/// a block's release, its replacement by another block, a context switch,
/// a rekeyed page; and the old context back finds the entry again.
#[test]
fn every_lookup_change_takes_the_entry() {
    const PC: u64 = 0xffff_fe00_0020_0040;
    let key = block_key(0x5000_0000, 1);
    let mut world = World::new();
    // A released block: its table entry goes, and the cache's with it.
    world.map(PC, key, 1, 0b1111);
    assert_eq!(world.run(0, PC), (0x7701, 99));
    assert_eq!(world.run(0, PC), (0x7701, 99));
    world.table.forget_block(PC, key, 1);
    let held = world.table.find(world.table.slot(PC, key), PC, key);
    assert!(!(held.pc == PC && held.key == key));
    assert_eq!(world.run(0, PC), exit(PC, ExitKind::Return));
    // Another block of the PC in its place.
    world.map(PC, key, 2, 0b1111);
    assert_eq!(world.run(0, PC), (0x7702, 99));
    world.map(PC, key, 5, 0b1111);
    assert_eq!(world.run(0, PC), (0x7705, 99));
    // A context switch of the upper half: the page translates in the new
    // context to another frame.
    world.control.contexts[1] += 1 << LOOKUP_SALT_BITS;
    world.control.salts[1] = world.itlb.salt(world.control.contexts[1]);
    world.map(PC, block_key(0x6000_0000, 1), 4, 0b1111);
    assert_eq!(world.run(0, PC), (0x7704, 99));
    // A native transition writes the old contexts back: the old frame's
    // block, which the probe enters again.
    world.control.contexts[1] -= 1 << LOOKUP_SALT_BITS;
    world.control.salts[1] = world.itlb.salt(world.control.contexts[1]);
    assert_eq!(world.run(0, PC), (0x7705, 99));
    // The page rekeyed in that context: a drop, and the new frame's block.
    world.map(PC, block_key(0x6000_0000, 1), 6, 0b1111);
    assert_eq!(world.run(0, PC), (0x7706, 99));
}

/// An entry enters a branch that sets a BTYPE only when its block accepts
/// it, and names the BTYPE; the budget and an interruption end the run as
/// the probe's would.
#[test]
fn a_hit_keeps_the_probes_btype_budget_and_interruption_rules() {
    const PC: u64 = 0x2_0000_0200;
    const REFUSING: u64 = 0x2_0000_0300;
    let mut world = World::new();
    world.map(PC, block_key(0x4000_0000, 1), 5, 0b0101);
    world.map(REFUSING, block_key(0x4000_0000, 1), 6, 0b0011);
    // Filled by the BTYPE 0 probe, taken by a BTYPE 2 tail.
    assert_eq!(world.run(0, PC), (0x7705, 99));
    assert_eq!(world.run(0, REFUSING), (0x7706, 99));
    let hits = world.control.hits;
    assert_eq!(world.run(32, PC), (0x7705, 99));
    assert_eq!(world.control.block, 5 | 2 << BLOCK_BTYPE_SHIFT);
    assert_eq!(world.control.hits, hits + 1);
    let branch = (REFUSING, ExitKind::Indirect as u64 | (RAN as u64) << 20);
    assert_eq!(world.run(32, REFUSING), branch);
    // A spent budget returns with the word and the instructions run.
    world.control.budget = 0;
    assert_eq!(world.run(0, PC), exit(PC, ExitKind::Return));
    world.control.budget = 100;
    // An interruption posted: the probe's gate exits.
    world.control.interrupt_epoch = 1;
    assert_eq!(world.run(0, PC), exit(PC, ExitKind::Return));
    world.control.interrupt_epoch = 0;
    assert_eq!(world.run(0, PC), (0x7705, 99));
}

/// Random runs: targets that share entries, pages put in, rekeyed and
/// forgotten, blocks released and contexts switched between runs. The
/// cached tail takes the transfer or the exit the plain tail takes, and
/// leaves the control the same.
#[test]
fn cached_tails_run_as_plain_tails() {
    let mut world = World::new();
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // Four PCs per half, two of them a cache's span apart (one entry).
    let span = (JUMP_CACHE_ENTRIES * 4) as u64;
    let pcs: Vec<u64> = [0x3_0000_0400_u64, 0xffff_fe00_0030_0400]
        .into_iter()
        .flat_map(|base| [base, base + span, base + 0x1000, base + 0x2000 + 8])
        .collect();
    let keys = [block_key(0x4000_0000, 1), block_key(0x7000_0000, 1)];
    let mut compared = 0;
    for _ in 0..20_000 {
        let pc = pcs[(next() % pcs.len() as u64) as usize];
        match next() % 16 {
            0 => world.map(
                pc,
                keys[(next() % 2) as usize],
                (next() % BLOCKS as u64) as usize,
                0b1111,
            ),
            1 => {
                let page = pc & !0xfff;
                world.itlb.forget_range(page..=page | 0xfff, u64::MAX);
            }
            2 => {
                let held = world.table.find(world.table.slot(pc, keys[0]), pc, keys[0]);
                if held.pc == pc {
                    world.table.forget_block(pc, held.key, held.block);
                }
            }
            3 => {
                let half = (next() % 2) as usize;
                world.control.contexts[half] ^= 1 << LOOKUP_SALT_BITS;
                world.control.salts[half] = world.itlb.salt(world.control.contexts[half]);
            }
            _ => {
                // The cached tail first: an entry of an earlier change
                // must miss.
                let mut plain = ChainControl { ..*world.control };
                let got = world.run(0, pc);
                let expected = run(&world.region, 16, &mut plain, pc);
                assert_eq!(got, expected, "{pc:#x}");
                // The probe fills; the tail counts as the probe counts.
                assert_eq!(
                    (
                        world.control.block,
                        world.control.retired,
                        world.control.hits
                    ),
                    (plain.block, plain.retired, plain.hits),
                    "{pc:#x}"
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 10_000);
}

/// An empty entry holds in no context (none is 0), so a branch to PC 0
/// takes the probe; the table's writer and a fill each take a stale entry
/// out of the cache.
#[test]
fn empty_and_replaced_entries_take_the_probe() {
    let mut world = World::new();
    assert_eq!(world.run(0, 0), exit(0, ExitKind::Return));
    assert_eq!(world.control.hits, 0);
    const PC: u64 = 0x3_0000_0040;
    let key = block_key(0x4000_0000, 1);
    world.map(PC, key, 2, 0b1111);
    assert_eq!(world.run(0, PC), (0x7702, 99));
    let entry = jump_entry(world.control.stamps, PC);
    // SAFETY: the entry's PC word, in the world's live stamps.
    let held = || unsafe { *(entry as *const u64) };
    assert_eq!(held(), PC);
    // The writer's change of the table entry takes the cache's.
    world.map(PC, key, 3, 0b1111);
    assert_eq!(held(), 0);
    assert_eq!(world.run(0, PC), (0x7703, 99));
    assert_eq!(held(), PC);
    world.table.clear();
    assert_eq!(held(), 0);
}
