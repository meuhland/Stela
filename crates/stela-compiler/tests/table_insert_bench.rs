//! A model of a board's block table under a compile workload (many short
//! processes of one position-independent program, each at its own slide,
//! over a kernel and shared code at fixed addresses): what an insert costs,
//! and how many inserts are new entries and how many put back an entry the
//! table had put out. Ignored; run with
//! `cargo test --release -p stela-compiler --test table_insert_bench -- --ignored --nocapture`,
//! sized by the `TB_*` variables below.
#![cfg(target_arch = "x86_64")]

use std::collections::HashSet;

use stela_compiler::x86_64::chain::{LookupEntry, LookupTable, block_key};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn ticks() -> u64 {
    // SAFETY: RDTSC has no preconditions on x86_64.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The host address a guest frame's block at `slot` of `frame` starts at,
/// and that frame's physical address.
fn block(frame: u64, slot: u64) -> (u64, u64) {
    let mixed = (frame << 8 | slot).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let physical = 0x8_0000_0000 + (frame.wrapping_mul(0x2545_f491) % (1 << 20)) * 4096;
    (physical, (mixed >> 54) * 4)
}

#[test]
#[ignore]
fn table_insert_model() {
    let entries = knob("TB_ENTRIES", 1 << 21) as usize;
    let shared_frames = knob("TB_SHARED", 30_000);
    let binary_frames = knob("TB_BINARY", 30_000);
    let per_frame = knob("TB_PER_FRAME", 32);
    let working = knob("TB_WORKING", 80_000) as usize;
    let life = knob("TB_LIFE", 4_000_000);
    let processes = knob("TB_PROCESSES", 8) as usize;
    let accesses = knob("TB_ACCESSES", 60_000_000);
    let cells = knob("TB_CELLS", 0);
    let states = knob("TB_STATES", 2);
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut table = LookupTable::with_entries(entries);

    // One process's working set at its slide: blocks of the program, which
    // move with the slide, and of shared code, which does not.
    let start = |rng: &mut Rng| -> Vec<(u64, u64)> {
        let slide = (rng.next() % (1 << 16)) << 14;
        (0..working)
            .map(|_| {
                let skew = |rng: &mut Rng, n: u64| (rng.next() % n) * (rng.next() % n) / n;
                let shared = rng.next() & 1 == 0;
                let frame = if shared {
                    skew(rng, shared_frames)
                } else {
                    shared_frames + skew(rng, binary_frames)
                };
                let (physical, offset) = block(frame, rng.next() % per_frame);
                let page = if shared {
                    0x1_8000_0000 + frame * 4096
                } else {
                    0x1_0000_0000 + slide + (frame - shared_frames) * 4096
                };
                let state = 1 + (rng.next() % states) as u32;
                (page + offset, block_key(physical, state))
            })
            .collect()
    };
    let mut running: Vec<(Vec<(u64, u64)>, u64)> =
        (0..processes).map(|_| (start(&mut rng), 0)).collect();
    for cell in 0..cells {
        let pc = running[0].0[(rng.next() % working as u64) as usize].0;
        table.link_cell(0x7000_0000_0000 + cell * 64, pc, None);
    }

    let mut ever = HashSet::new();
    let (mut inserts, mut new, mut spent) = (0u64, 0u64, 0u64);
    let mut slowest = [0u64; 4];
    let wall = std::time::Instant::now();
    let first_tick = ticks();
    let mut done = 0;
    let mut process = 0;
    while done < accesses {
        let (set, age) = &mut running[process];
        for _ in 0..1000 {
            let w = set.len() as u64;
            let index = ((rng.next() % w) * (rng.next() % w) / w) as usize;
            let (pc, key) = set[index];
            let slot = table.slot(pc, key);
            let found = table.find(slot, pc, key);
            if found.pc != pc || found.key != key {
                let entry = LookupEntry {
                    pc,
                    key,
                    entry: 0x1000 + index as u64 * 64,
                    block: index as u32,
                    targets: 0xf,
                };
                let before = ticks();
                table.insert(slot, entry);
                let took = ticks() - before;
                spent += took;
                inserts += 1;
                let bucket = match took {
                    0..1_000 => 0,
                    1_000..10_000 => 1,
                    10_000..100_000 => 2,
                    _ => 3,
                };
                slowest[bucket] += 1;
                if ever.insert((pc, key)) {
                    new += 1;
                }
            }
        }
        done += 1000;
        *age += 1000;
        if *age >= life {
            *set = start(&mut rng);
            *age = 0;
        }
        process = (process + 1) % processes;
    }
    let elapsed = wall.elapsed();
    let per_tick = elapsed.as_nanos() as f64 / (ticks() - first_tick) as f64;
    let [put_out, moved_out] = table.evicted();
    println!(
        "entries={entries} accesses={accesses} inserts={inserts} new={new} again={} \
         insert_ns={:.0} evicted={put_out}/{moved_out} ticks<1k/10k/100k/more={:?} \
         index_kib={} wall_ms={}",
        inserts - new,
        spent as f64 * per_tick / inserts.max(1) as f64,
        slowest,
        table.index_bytes() / 1024,
        elapsed.as_millis(),
    );
}
