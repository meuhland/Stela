//! Host cost of one transfer through each path an exit out of its block
//! takes to a block in another page: a stamped tail, a cell tail whose
//! cell holds the target, a cell tail whose cell holds nothing (its probe
//! goes on to the table probe), and a lookup tail through its jump cache
//! or the table probe for one, a few or many targets, in one native run
//! or in runs of a few dozen transfers as the guest's are. Run by hand: it
//! prints nanoseconds and TSC ticks per transfer, less a loop whose
//! transfer is a direct jump, and asserts only the counts.
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

const REGION: usize = 1 << 16;
/// The loop at 0, the tail under test, the probes of BTYPE 0, then the
/// target blocks, [`BLOCK_BYTES`] apart.
const TAIL: usize = 1024;
const PROBES: usize = 4096;
const BLOCKS: usize = 8192;
const BLOCK_BYTES: usize = 32;
const MOST_TARGETS: usize = (REGION - BLOCKS) / BLOCK_BYTES;
/// The target sequence's length, a power of two.
const SEQUENCE: usize = 1 << 12;
const RAN: u32 = 3;
const CONTEXTS: [u64; 2] = [5 << LOOKUP_SALT_BITS, 9 << LOOKUP_SALT_BITS];
const KEY: u64 = block_key(0x8_0040_0000, 1);
/// Guest PCs of the targets: one function entry every 0x44 bytes of a
/// library's text in the upper half, 4-byte aligned as A64 code is.
const fn target_pc(target: usize) -> u64 {
    0xffff_fe00_1234_0000 + 0x44 * target as u64
}

struct Pages(*mut u8);
impl Drop for Pages {
    fn drop(&mut self) {
        // SAFETY: our mapping, nothing runs it any more.
        assert_eq!(unsafe { munmap(self.0.cast(), REGION) }, 0);
    }
}

fn put(pages: &Pages, offset: usize, bytes: &[u8]) {
    assert!(offset + bytes.len() <= REGION);
    // SAFETY: inside the mapping; nothing runs it while it is written.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), pages.0.add(offset), bytes.len()) };
}

/// Where the target of each transfer comes from.
#[derive(Clone, Copy)]
enum Target {
    /// One PC, as a direct exit's `movabs rax`.
    Fixed(u64),
    /// The next of [`SEQUENCE`] PCs at X1, X0 counting them.
    Sequence,
}

/// A mapping for [`fill`].
fn mapped() -> Pages {
    // SAFETY: private anonymous memory, unmapped on drop.
    let base = unsafe { mmap(core::ptr::null_mut(), REGION, 7, 0x22, -1, 0) };
    assert_ne!(base as usize, usize::MAX);
    Pages(base.cast())
}

/// The loop in `region`: RSI transfers to the tail with RAX the target,
/// each entered block jumping back; then RAX 1 and EDX 2 return. `direct`
/// replaces the tail with a jump to the first block, the baseline.
fn fill(region: &Pages, target: Target, tail: &[u8], direct: bool) {
    put(region, 0, &[0xcc; REGION]);
    let at = region.0 as u64;
    let mut code = Vec::new();
    // sub rsi, 1; je done (rel32)
    code.extend([0x48, 0x83, 0xee, 0x01, 0x0f, 0x84, 0, 0, 0, 0]);
    let done = code.len();
    match target {
        Target::Fixed(pc) => {
            code.extend([0x48, 0xb8]);
            code.extend(pc.to_le_bytes());
        }
        Target::Sequence => {
            // mov rcx,[rdi]; add rcx,1; and ecx,SEQUENCE-1; mov [rdi],rcx;
            // mov r8,[rdi+8]; mov rax,[r8+rcx*8]
            code.extend([0x48, 0x8b, 0x0f, 0x48, 0x83, 0xc1, 0x01, 0x81, 0xe1]);
            code.extend(((SEQUENCE - 1) as u32).to_le_bytes());
            code.extend([
                0x48, 0x89, 0x0f, 0x4c, 0x8b, 0x47, 0x08, 0x49, 0x8b, 0x04, 0xc8,
            ]);
        }
    }
    let to = if direct { BLOCKS } else { TAIL };
    code.push(0xe9);
    let end = at + code.len() as u64 + 4;
    code.extend(((at + to as u64).wrapping_sub(end) as i32).to_le_bytes());
    let exit = code.len();
    code[done - 4..done].copy_from_slice(&((exit - done) as i32).to_le_bytes());
    // mov eax,1; mov edx,2; then the exit's return.
    code.extend([0xb8, 1, 0, 0, 0, 0xba, 2, 0, 0, 0]);
    code.extend(EXIT_RETURN);
    put(region, 0, &code);
    if !direct {
        put(region, TAIL, tail);
    }
    put(region, PROBES, &lookup_dispatcher(0).unwrap());
    for block in 0..MOST_TARGETS {
        let offset = BLOCKS + BLOCK_BYTES * block;
        let end = at + offset as u64 + 5;
        let mut jump = vec![0xe9];
        jump.extend((at.wrapping_sub(end) as i32).to_le_bytes());
        put(region, offset, &jump);
    }
}

/// A board's tables with `targets` blocks in the region, each in the
/// table and its page in the ITLB, and a control whose budget never ends.
struct World {
    table: LookupTable,
    _itlb: LookupTable,
    control: Box<ChainControl>,
}

impl World {
    fn new(region: u64, targets: usize) -> Self {
        let mut table = LookupTable::new();
        let mut itlb = LookupTable::itlb();
        let cells = table.stamped_cells();
        let stamps = table.new_stamps();
        for target in 0..targets {
            let pc = target_pc(target);
            itlb.insert(
                itlb.page_slot(pc, CONTEXTS[1]),
                LookupEntry::page(pc & !0xfff, CONTEXTS[1], KEY),
            );
            table.insert(table.slot(pc, KEY), entry(region, target));
        }
        let control = Box::new(ChainControl {
            budget: u64::MAX / 2,
            table: table.address(),
            itlb: itlb.address(),
            contexts: CONTEXTS,
            salts: CONTEXTS.map(|context| itlb.salt(context)),
            masks: [table.mask(), itlb.mask()],
            stamps,
            cells,
            ..ChainControl::default()
        });
        World {
            table,
            _itlb: itlb,
            control,
        }
    }

    /// Enter the loop for `transfers` transfers, in native runs of at most
    /// `run` transfers, each entered as the engine enters native code: TSC
    /// ticks taken.
    fn run(&mut self, region: &Pages, sequence: &[u64], transfers: u64, run: u64) -> u64 {
        let mut state = Box::new(GuestState::default());
        state.anchors[Anchor::Control] = &raw mut *self.control as u64;
        state.x[1] = sequence.as_ptr() as u64;
        let start = time();
        let mut left = transfers;
        while left > 0 {
            let now = left.min(run);
            left -= now;
            // SAFETY: the region holds the loop, a tail, probes and blocks
            // with the block ABI; they touch the state, the control, the
            // tables, the stamps and the sequence, which outlive the call.
            let result = unsafe {
                crate::x86_64::native::enter(region.0 as *const u8, &mut *state, now + 1, 0)
            };
            assert_eq!((result as u64, (result >> 64) as u64), (1, 2));
        }
        time() - start
    }
}

fn entry(region: u64, target: usize) -> LookupEntry {
    LookupEntry {
        pc: target_pc(target),
        key: KEY,
        entry: region + (BLOCKS + BLOCK_BYTES * target) as u64,
        block: 10 + target as u32,
        targets: 0b1111,
    }
}

fn time() -> u64 {
    // SAFETY: RDTSC reads the time stamp counter only.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// [`SEQUENCE`] PCs of `targets` targets in a pseudo-random order, which no
/// predictor learns.
fn sequence(targets: usize) -> Vec<u64> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..SEQUENCE)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            target_pc((state >> 20) as usize % targets)
        })
        .collect()
}

/// The path a case's tail takes.
#[derive(Clone, Copy, Debug)]
enum Path {
    /// A hot exit's stamped tail, through a stamped cell.
    Stamped,
    /// A cell tail through a cell past the stamped ones, as every exit's
    /// is until its transfers make it hot: the census's cell probes.
    Cell,
    /// A cell tail whose cell holds no entry.
    EmptyCell,
    /// A lookup tail of this many targets in one native run.
    Lookup(usize),
    /// One of this many targets in native runs of this many transfers.
    LookupRuns(usize, u64),
}

/// Nanoseconds and TSC ticks of one transfer through `path`, less the
/// direct jump's, and the table probe transfers it made per transfer.
fn cost(path: Path) -> (f64, f64, f64) {
    const TRANSFERS: u64 = 1 << 22;
    let pc = target_pc(0);
    let (target, targets, run) = match path {
        Path::Lookup(targets) => (Target::Sequence, targets, TRANSFERS),
        Path::LookupRuns(targets, run) => (Target::Sequence, targets, run),
        _ => (Target::Fixed(pc), 1, TRANSFERS),
    };
    let sequence = sequence(targets);
    let word = ExitWord::from(match path {
        Path::Lookup(_) | Path::LookupRuns(..) => ExitKind::Indirect,
        _ => ExitKind::Direct,
    });
    // The region's address first: tails jump relative to it.
    let (measured, direct) = (mapped(), mapped());
    let base = measured.0 as u64;
    let mut world = World::new(base, targets);
    let at = base + TAIL as u64;
    let site = 0x1000;
    let tail: Vec<u8> = match path {
        Path::Stamped | Path::Cell => {
            let held = world.table.find(world.table.slot(pc, KEY), pc, KEY);
            match path {
                Path::Stamped => world.table.link_hot_cell(site, pc, Some(held)).unwrap(),
                _ => world.table.link_cell(site, pc, Some(held)),
            };
            let number = world.table.site_cell_number(site).unwrap();
            let cell = LookupTable::cell_slot(&world.table.cells, number) as *const Slot as u64;
            match path {
                Path::Stamped => {
                    world.table.note_stamped(number);
                    let stamping = base + (PROBES + STAMPING_PROBE_ENTRY) as u64;
                    stamped_tail(
                        at, cell, number, pc, word, RAN, held.block, held.entry, stamping,
                    )
                    .unwrap()
                    .to_vec()
                }
                _ => {
                    let probe = base + (PROBES + CELL_PROBE_ENTRY) as u64;
                    cell_tail(at, cell, word, RAN, probe).unwrap().to_vec()
                }
            }
        }
        Path::EmptyCell => {
            world.table.link_cell(site, pc, None);
            let number = world.table.site_cell_number(site).unwrap();
            let cell = LookupTable::cell_slot(&world.table.cells, number) as *const Slot as u64;
            let probe = base + (PROBES + CELL_PROBE_ENTRY) as u64;
            cell_tail(at, cell, word, RAN, probe).unwrap().to_vec()
        }
        Path::Lookup(_) | Path::LookupRuns(..) => {
            let probe = base + (PROBES + PROBE_ENTRY) as u64;
            lookup_tail(at, word, RAN, probe, 0).unwrap().to_vec()
        }
    };
    fill(&measured, target, &tail, false);
    // The baseline: the same loop, whose transfer is a direct jump.
    fill(&direct, target, &tail, true);
    let filled = |world: &World| {
        // SAFETY: the table's stamps, which live while it does.
        let counts = unsafe { stamp_counts(world.control.stamps) };
        counts[StampCount::Filled as usize]
    };
    // Warm: the stamp, the jump cache, the predictors.
    world.run(&measured, &sequence, 1 << 16, run);
    world.run(&direct, &sequence, 1 << 16, run);
    let (mut best, mut baseline) = (u64::MAX, u64::MAX);
    let mut table_probes = 0;
    for _ in 0..7 {
        let before = filled(&world);
        best = best.min(world.run(&measured, &sequence, TRANSFERS, run));
        table_probes = filled(&world) - before;
        baseline = baseline.min(world.run(&direct, &sequence, TRANSFERS, run));
    }
    let ticks = best.saturating_sub(baseline) as f64 / TRANSFERS as f64;
    let hz = tsc_hz();
    (
        ticks * 1e9 / hz,
        ticks,
        table_probes as f64 / TRANSFERS as f64,
    )
}

/// The TSC's rate, from a 50 ms sleep.
fn tsc_hz() -> f64 {
    let (start, at) = (std::time::Instant::now(), time());
    std::thread::sleep(std::time::Duration::from_millis(50));
    (time() - at) as f64 / start.elapsed().as_secs_f64()
}

#[test]
#[ignore = "manual: opt-in"]
fn transfer_path_costs() {
    for path in [
        Path::Stamped,
        Path::Cell,
        Path::EmptyCell,
        Path::Lookup(1),
        Path::Lookup(2),
        Path::Lookup(4),
        Path::Lookup(64),
        Path::Lookup(1024),
        Path::LookupRuns(1, 32),
        Path::LookupRuns(16, 32),
        Path::LookupRuns(64, 32),
    ] {
        let (ns, ticks, table) = cost(path);
        std::println!(
            "{path:?}: {ns:.2} ns, {ticks:.1} TSC ticks, {table:.2} table probes per transfer"
        );
    }
}
