//! A replay of a long session's allocations, shaped like the heap lines of
//! an 8-hour log of a 16 GB PC (4352 MiB heap): the live bytes of each size
//! class and each free-list bucket, long-lived blocks that turn over slowly,
//! blocks that live for seconds to minutes, and compile jobs whose scratch
//! comes and goes between them, with a Core Animation sized one each hour.
//! Everything is scaled down by [`SCALE`] in count, and buckets past 4 MiB in
//! size too, so the heap and its live share match the log's.
use super::*;
use std::vec::Vec;

/// The log's heap, 4352 MiB, over this.
const SCALE: usize = 32;
/// Beats of 10 s in 8 hours.
const BEATS: usize = 8 * 360;
/// The log's live bytes the long-lived blocks take; seconds-to-minutes
/// blocks take most of the rest.
const LONG_PERCENT: usize = 84;
/// A beat's turnover of the long-lived blocks, in parts per 10 000.
const TURNOVER: u64 = 20;

/// Live bytes by size class, 16 B doubling to 4 KiB (`classes=` in the
/// log's `HEAP` line at its busiest beat), in MiB.
const CLASS_MIB: [usize; CLASSES] = [24, 109, 100, 201, 415, 250, 260, 96, 129];
/// Live free-list blocks by bucket, 4 KiB doubling (`4K:39736/105M`...):
/// count and MiB.
const BUCKETS: [(usize, usize); LARGE_BUCKETS] = [
    (39736, 105),
    (9723, 84),
    (1978, 44),
    (3749, 129),
    (461, 33),
    (2825, 398),
    (92, 25),
    (86, 48),
    (39, 51),
    (15, 34),
    (16, 73),
    (8, 71),
    (8, 155),
    (2, 92),
    (3, 220),
    (1, 128),
];

/// A deterministic generator (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound.max(1) as u64) as usize
    }
}

/// A request of size class `class`.
fn class_size(random: &mut Random, class: usize) -> usize {
    let top = SMALLEST << class;
    let bottom = if class == 0 { 1 } else { top / 2 + 1 };
    bottom + random.below(top - bottom + 1)
}

/// A request of bucket `bucket`, around the bucket's mean in the log (the
/// first holds page-aligned blocks of under 4 KiB too, taken here as just
/// over a page), scaled past 4 MiB.
fn bucket_size(random: &mut Random, bucket: usize) -> usize {
    let (count, mib) = BUCKETS[bucket];
    let mean = (mib << 20) / count;
    let size = mean / 2 + random.below(mean) / 16 * 16;
    if bucket >= 11 { size / SCALE } else { size }.max(4097)
}

struct Live {
    pointer: *mut u8,
    layout: Layout,
    /// The beat it is freed at; `usize::MAX` for a long-lived block.
    until: usize,
}

struct Replay<'a> {
    heap: &'a Heap,
    random: Random,
    /// The boot's tables of a megabyte or more, kept all session.
    tables: Vec<Live>,
    long: Vec<Live>,
    timed: Vec<Live>,
    failures: usize,
}

impl Replay<'_> {
    fn allocate(&mut self, size: usize, until: usize) -> Option<Live> {
        let align = if self.random.below(64) == 0 { 64 } else { 16 };
        let layout = Layout::from_size_align(size, align).unwrap();
        // SAFETY: a nonzero layout on the initialized heap; freed once.
        let pointer = unsafe { self.heap.alloc(layout) };
        if pointer.is_null() {
            self.failures += 1;
            return None;
        }
        // Touch it, as an owner does.
        // SAFETY: the live allocation just made.
        unsafe { pointer.write(0xa5) };
        Some(Live {
            pointer,
            layout,
            until,
        })
    }

    fn free(&self, live: Live) {
        // SAFETY: a live allocation of this heap with its layout.
        unsafe { self.heap.dealloc(live.pointer, live.layout) };
    }

    /// A small block's size: by the classes' live blocks.
    fn small_size(&mut self) -> usize {
        let counts = CLASS_MIB.map(|mib| mib << 10);
        let blocks = |class: usize| counts[class] / (SMALLEST << class);
        let total: usize = (0..CLASSES).map(blocks).sum();
        let mut pick = self.random.below(total);
        let class = (0..CLASSES)
            .position(|class| {
                if pick < blocks(class) {
                    true
                } else {
                    pick -= blocks(class);
                    false
                }
            })
            .unwrap();
        class_size(&mut self.random, class)
    }

    fn keep(&mut self, size: usize) {
        if let Some(live) = self.allocate(size, usize::MAX) {
            self.long.push(live);
        }
    }

    fn timed(&mut self, size: usize, until: usize) {
        if let Some(live) = self.allocate(size, until) {
            self.timed.push(live);
        }
    }

    /// The long-lived profile, built as a boot builds it: its tables of a
    /// megabyte or more first, largest first, then the rest interleaved, with
    /// seconds-lived blocks between.
    fn build(&mut self) {
        let mut tables: Vec<usize> = Vec::new();
        let mut wanted: Vec<usize> = Vec::new();
        for (class, &mib) in CLASS_MIB.iter().enumerate() {
            let mut bytes = (mib << 20) / SCALE * LONG_PERCENT / 100;
            while bytes > 0 {
                // The log counts a block by its class.
                bytes = bytes.saturating_sub(SMALLEST << class);
                wanted.push(class_size(&mut self.random, class));
            }
        }
        for (bucket, &(count, _)) in BUCKETS.iter().enumerate() {
            let count = if bucket >= 11 {
                count
            } else {
                // Rounded at random, so few blocks keep their bytes.
                let scaled = count * LONG_PERCENT;
                let whole = scaled / (100 * SCALE);
                whole + usize::from(self.random.below(100 * SCALE) < scaled % (100 * SCALE))
            };
            for _ in 0..count {
                let size = bucket_size(&mut self.random, bucket);
                if bucket >= 8 {
                    tables.push(size);
                } else {
                    wanted.push(size);
                }
            }
        }
        tables.sort_unstable_by(|a, b| b.cmp(a));
        // Shuffle the rest, so classes and buckets interleave.
        for index in (1..wanted.len()).rev() {
            let other = self.random.below(index + 1);
            wanted.swap(index, other);
        }
        for size in tables {
            if let Some(live) = self.allocate(size, usize::MAX) {
                self.tables.push(live);
            }
        }
        for size in wanted {
            self.keep(size);
            if self.random.below(8) == 0 {
                let size = self.small_size();
                let until = 1 + self.random.below(30);
                self.timed(size, until);
            }
        }
    }

    /// One beat: the long-lived blocks turn over, seconds-to-minutes blocks
    /// come and go, and `job` bytes of a compile's scratch are taken between
    /// them and freed at the beat's end, a few of them kept.
    fn beat(&mut self, beat: usize, job: usize) {
        let turnover = self.long.len() * TURNOVER as usize / 10_000;
        for _ in 0..turnover {
            let index = self.random.below(self.long.len());
            let old = self.long.swap_remove(index);
            let size = old.layout.size();
            self.free(old);
            self.keep(size);
        }
        let mut scratch: Vec<Live> = Vec::new();
        let mut taken = 0;
        let churn = 2000 / SCALE * 8;
        let mut step = 0;
        while step < churn || taken < job {
            if step < churn {
                let size = if self.random.below(64) == 0 {
                    let bucket = self.random.below(4);
                    bucket_size(&mut self.random, bucket)
                } else {
                    self.small_size()
                };
                let until = beat + 1 + self.random.below(60);
                self.timed(size, until);
            }
            // A compile's scratch: mostly small IR nodes and sets, some
            // arrays of up to a megabyte, a few larger.
            for _ in 0..16 {
                if taken >= job {
                    break;
                }
                let size = match self.random.below(100) {
                    0 => ((2 << 20) + self.random.below(6 << 20)) / SCALE,
                    1..=40 => 4097 + self.random.below(1 << 20),
                    _ => 16 + self.random.below(240),
                };
                match self.allocate(size, beat) {
                    Some(live) => scratch.push(live),
                    // A compile that finds no memory stops: the heap's
                    // exhaustion, which the test counts.
                    None => {
                        taken = job;
                        break;
                    }
                }
                taken += size;
            }
            step += 1;
        }
        // A job keeps a few of its blocks: the compiled result.
        for live in scratch {
            if self.random.below(100) == 0 {
                self.long.push(live);
            } else {
                self.free(live);
            }
        }
        let mut index = 0;
        while index < self.timed.len() {
            if self.timed[index].until <= beat {
                let live = self.timed.swap_remove(index);
                self.free(live);
            } else {
                index += 1;
            }
        }
        // A kept compile result goes again later.
        while self.long.len() > 1 && self.random.below(4) == 0 {
            let index = self.random.below(self.long.len());
            let old = self.long.swap_remove(index);
            self.free(old);
        }
    }
}

/// What the heap looks like at an hour's end.
#[derive(Clone, Copy, Debug)]
struct Hour {
    live: usize,
    carved: usize,
    largest: usize,
    extents: usize,
    peak: usize,
}

fn replay() -> (Vec<Hour>, usize, usize) {
    let bytes = (4352usize << 20) / SCALE;
    let layout = Layout::from_size_align(bytes, 1 << 16).unwrap();
    // SAFETY: a nonzero layout; freed at the end.
    let ram = unsafe { std::alloc::alloc(layout) };
    assert!(!ram.is_null());
    let heap = Heap::new();
    // SAFETY: exclusive backing for the replay, freed only after it.
    unsafe { heap.initialize(ram, bytes) };
    // As the engine keeps it (`HEAP_RESERVE_SHARE`).
    heap.reserve_tail(bytes / 16);
    let mut replay = Replay {
        heap: &heap,
        random: Random(0x5eed_1234_abcd_0001),
        tables: Vec::new(),
        long: Vec::new(),
        timed: Vec::new(),
        failures: 0,
    };
    replay.build();
    let mut hours = Vec::new();
    for beat in 0..BEATS {
        let job = if beat % 360 == 180 {
            // Core Animation's generic shader: 778 MiB at peak.
            (778 << 20) / SCALE
        } else if beat % 30 == 7 {
            (60 << 20) / SCALE
        } else {
            0
        };
        replay.beat(beat, job);
        if beat % 360 == 359 {
            let usage = heap.usage();
            hours.push(Hour {
                live: usage.live,
                carved: usage.carved,
                largest: usage.largest,
                extents: usage.extents,
                peak: usage.peak,
            });
        }
    }
    let failures = replay.failures;
    let blocks = replay.tables.drain(..);
    for live in blocks
        .chain(replay.long.drain(..))
        .chain(replay.timed.drain(..))
    {
        // SAFETY: each live block once, with its layout.
        unsafe { heap.dealloc(live.pointer, live.layout) };
    }
    let usage = heap.usage();
    let left = usage.carved;
    // SAFETY: the heap holds nothing live; nothing uses its backing now.
    unsafe { std::alloc::dealloc(ram, layout) };
    (hours, failures, left)
}

#[test]
fn eight_hours_of_a_sessions_allocations_keep_holes_and_the_largest_extent_bounded() {
    let total = (4352usize << 20) / SCALE;
    let (hours, failures, left) = replay();
    let mib = |bytes: usize| bytes as f64 * SCALE as f64 / f64::from(1 << 20);
    std::println!("failed allocations: {failures}");
    for (hour, at) in hours.iter().enumerate() {
        std::println!(
            "hour {}: live={:.0}M carved={:.0}M holes={:.0}M largest={:.0}M extents={} peak={:.0}M \
             (as at full scale)",
            hour + 1,
            mib(at.live),
            mib(at.carved),
            mib(at.carved - at.live),
            mib(at.largest),
            at.extents * SCALE,
            mib(at.peak),
        );
    }
    assert_eq!(failures, 0, "allocations failed");
    // Everything freed returns to the bins, but each class's empty slab.
    assert!(left <= CLASSES * SLAB_LARGEST, "{left} bytes left carved");
    let reserve = total / 16;
    for at in &hours {
        // Holes and the slabs' free blocks stay under a sixteenth of the heap.
        assert!(
            at.carved - at.live < total / 16,
            "{:.0}M of holes",
            mib(at.carved - at.live)
        );
        // Free extents stay few: one per 16 KiB of heap at most.
        assert!(at.extents < total >> 14, "{} extents", at.extents);
        // The main part keeps an extent larger than the whole reserve.
        assert!(
            at.largest > reserve,
            "largest {:.0}M, reserve {:.0}M",
            mib(at.largest),
            mib(reserve)
        );
    }
    // The last hour looks like the first: nothing grows with time.
    let (first, last) = (hours[0], hours[hours.len() - 1]);
    assert!(last.extents <= first.extents * 5 / 4);
    assert!(last.carved - last.live <= first.carved - first.live);
}
