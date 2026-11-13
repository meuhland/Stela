use super::*;
use alloc::string::ToString;

/// Free RAM a PC of `installed` GiB shows Stela: firmware keeps about 3 %.
fn free(installed: u64) -> u64 {
    installed * GIB / 32 * 31
}

#[test]
fn every_host_size_keeps_the_parts_in_bounds_and_the_guest_above_its_floor() {
    for (installed, runners) in [
        (8, 2),
        (8, 4),
        (8, 8),
        (16, 8),
        (32, 8),
        (128, 8),
        (128, 64),
        (2048, 8),
        (2048, 64),
    ] {
        let free = free(installed);
        let sizes = sizes(free, runners, runners).unwrap();
        let floor = HEAP_FLOOR + sizes.code.window - FLOOR_WINDOW;
        assert!((floor..=HEAP_CEILING.max(floor)).contains(&sizes.heap));
        assert!(sizes.guest >= GUEST_FLOOR);
        assert_eq!(
            sizes.heap + sizes.code.bytes + sizes.pool + sizes.guest,
            free
        );
        // The pool maps each sharer's guest DRAM once, or its base and a
        // halves' sizes if less, up to its cap.
        let sharers = (runners as u64).min(POOL_SHARERS);
        let (lower, upper) = pool_halves(sizes.guest);
        let each = (sizes.guest / POOL_SHARE).min(lower + upper);
        let wanted = sharers * each;
        let cap = sharers * POOL_PER_RUNNER;
        let floor = POOL_FLOOR.max(runners as u64 * POOL_RUNNER_FLOOR);
        assert!(
            sizes.pool == cap || sizes.pool + ALIGNMENT * 2 >= wanted,
            "{installed} GiB"
        );
        assert!(sizes.pool <= wanted.max(floor) || sizes.pool == cap);
    }
}

#[test]
fn an_8_gib_host_gives_the_guest_what_the_parts_leave() {
    let sizes = sizes(free(8), 8, 8).unwrap();
    // The floor less the 16 MiB the 496 MiB window lacks of 512 MiB.
    assert_eq!(sizes.code.window, 496 * MIB);
    assert_eq!(sizes.heap, HEAP_FLOOR - 16 * MIB);
    assert_eq!(sizes.code.bytes, 512 * MIB);
    // 8 runners' sixty-fourths of the guest's 4 GiB, not of all 7.75 GiB.
    assert_eq!(sizes.pool, 512 * MIB);
    assert_eq!(sizes.guest, 4112 * MIB);
}

#[test]
fn a_16_gib_host_takes_96_mib_per_runner_for_its_window_and_a_quarter_for_its_heap() {
    let sizes = sizes(free(16), 8, 8).unwrap();
    // 8 runners' 96 MiB (768 MiB, under a sixteenth's 992 MiB), and each
    // executor's 2 MiB of its own.
    assert_eq!(sizes.code.window, 768 * MIB);
    assert_eq!(sizes.code.bytes, 784 * MIB);
    // A quarter of the 15872 MiB free, a byte per window byte and 16 MiB
    // per PE (3968 + 768 + 128 MiB), past the floor the window raises by
    // its 256 MiB over 512 MiB.
    assert_eq!(sizes.heap, 4864 * MIB);
    assert!(sizes.heap > HEAP_FLOOR + 256 * MIB);
    assert!(sizes.guest > 9 * GIB);
}

/// A window past 512 MiB raises the heap's floor by as much, which holds
/// where a quarter of the host, the window and the PEs come to less: 8448
/// MiB free and 8 runners take a 528 MiB window (a sixteenth) and the floor
/// and 16 MiB, over the 2112 + 528 + 128 MiB of the share.
#[test]
fn a_window_past_512_mib_raises_the_heap_floor() {
    let sizes = sizes(8448 * MIB, 8, 8).unwrap();
    assert_eq!(sizes.code.window, 528 * MIB);
    assert_eq!(sizes.heap, HEAP_FLOOR + 16 * MIB);
    assert!(sizes.guest >= GUEST_FLOOR);
}

/// A 12 GiB host with 8 cores (11514 MiB free) takes a 704 MiB window,
/// 192 MiB more than 64 MiB per runner gave, and as much more heap.
#[test]
fn a_12_gib_8_core_host_takes_a_larger_window_and_heap_from_its_guest() {
    let sizes = sizes(11514 * MIB, 8, 8).unwrap();
    // A quarter of the free RAM (2878 MiB), the window and 128 MiB for the
    // PEs.
    assert_eq!(
        [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB),
        [3710, 720, 714, 6370]
    );
    let small = estimate_with(11514 * MIB, 8, 8, SMALL_WINDOW_PER_RUNNER).unwrap();
    assert_eq!(sizes.code.window - small.code.window, 192 * MIB);
    assert_eq!(sizes.heap - small.heap, 192 * MIB);
}

/// A host whose guest the larger window would put under its floor keeps
/// 64 MiB per runner, and boots where it booted before.
#[test]
fn a_host_short_of_the_larger_window_falls_back_to_the_smaller() {
    // Two runners near the floor: 192 MiB of window would leave the guest
    // under 3 GiB, 128 MiB does not.
    let short = (5 * GIB / MIB..8 * GIB / MIB)
        .step_by(16)
        .map(|free| free * MIB)
        .find(|&free| {
            let large = estimate_with(free, 2, 2, WINDOW_PER_RUNNER).unwrap();
            let small = estimate_with(free, 2, 2, SMALL_WINDOW_PER_RUNNER).unwrap();
            large.guest < GUEST_FLOOR && small.guest >= GUEST_FLOOR
        })
        .expect("a host the fallback lets boot");
    let sizes = sizes(short, 2, 2).unwrap();
    assert_eq!(sizes.code.window, 2 * SMALL_WINDOW_PER_RUNNER);
    assert!(sizes.guest >= GUEST_FLOOR);
    // With a little more RAM the larger window fits.
    let roomy = sizes_of(short + 512 * MIB);
    assert_eq!(roomy.code.window, 2 * WINDOW_PER_RUNNER);
}

fn sizes_of(free: u64) -> Sizes {
    sizes(free, 2, 2).unwrap()
}

#[test]
fn a_32_gib_host_grows_the_heap_with_its_share_window_and_pes() {
    let sizes = sizes(free(32), 8, 8).unwrap();
    let heap = free(32) / HEAP_SHARE + HEAP_PER_WINDOW * 768 * MIB + 8 * PE_HEAP;
    assert_eq!(sizes.heap, heap / ALIGNMENT * ALIGNMENT);
    assert_eq!(sizes.heap, 8832 * MIB);
    assert!(HEAP_FLOOR < sizes.heap && sizes.heap < HEAP_CEILING);
    assert_eq!(sizes.code.bytes, 784 * MIB);
    // Each runner's halves for the guest's 20.7 GiB, not a 64th.
    assert_eq!(sizes.pool, 888 * MIB);
    assert!(sizes.pool < 8 * POOL_PER_RUNNER);
    assert!(sizes.guest > 20 * GIB);
    // From about 60 GiB free the share passes the ceiling.
    assert_eq!(super::sizes(free(128), 8, 8).unwrap().heap, HEAP_CEILING);
}

/// Heap, code, pool and guest MiB for `runners` PEs at `free` GiB, or None
/// when the guest's floor does not fit.
#[test]
fn stelas_share_stops_growing_per_pe_past_what_each_pe_takes() {
    for (runners, free, expected) in [
        (2, 4, None),
        (2, 8, Some([2496, 196, 166, 5334])),
        (2, 16, Some([4320, 196, 194, 11674])),
        (2, 30, Some([7904, 196, 224, 22396])),
        (2, 128, Some([16384, 196, 492, 114000])),
        (8, 4, None),
        (8, 8, Some([2816, 528, 538, 4310])),
        (8, 16, Some([4992, 784, 754, 9854])),
        (8, 30, Some([8576, 784, 880, 20480])),
        (8, 128, Some([16384, 784, 1950, 111954])),
        (24, 4, None),
        (24, 8, Some([2944, 560, 768, 3920])),
        (24, 16, Some([5504, 1072, 768, 9040])),
        (24, 30, Some([9984, 1968, 848, 17920])),
        (24, 128, Some([16384, 2096, 1936, 110656])),
        (64, 4, None),
        (64, 8, None),
        (64, 16, Some([6144, 1152, 2048, 7040])),
        (64, 30, Some([10624, 2048, 2048, 16000])),
        (64, 128, Some([16384, 2176, 2048, 110464])),
    ] {
        let actual = sizes(free * GIB, runners, runners)
            .ok()
            .map(|sizes| [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB));
        assert_eq!(actual, expected, "{runners} PEs, {free} GiB");
        if let Some([_, _, pool, guest]) = actual {
            // The pool's total stays within an eighth of the guest, or each
            // runner's floor, whatever the PE count.
            assert!(
                pool <= (guest / 8).max(runners as u64 * 32),
                "{runners} {free}"
            );
        }
    }
}

/// The splits of measured boots' free RAM on 8 PEs, and the guest MiB each
/// boot gave:
/// - the 8 GiB VM, `HOST MEMORY free=7354M heap=2816M code=464M pool=452M
///   guest=3622M`: 58 MiB more guest;
/// - an 8 GB PC (7544 MiB free): 44 MiB more;
/// - the 12 GiB VM, `free=11514M heap=3008M code=720M pool=864M
///   guest=6922M`: 552 MiB less;
/// - the 16 GiB VM, `free=15710M heap=3280M code=992M pool=1032M
///   guest=10406M`: 1050 MiB less;
/// - the 32 GB PC, `free=30242M heap=3672M code=1040M pool=2836M
///   guest=22694M`: 2566 MiB less.
///
/// From 12 GiB up the heap takes a quarter of the free RAM, more than those
/// boots' heaps; the 8 GiB hosts' heaps stay at the floor.
#[test]
fn measured_boots_split_as_expected() {
    for (free, expected) in [
        (7354, [2752, 464, 458, 3680]),
        (HOST_8_GB / MIB, [2768, 480, 476, 3820]),
        (11514, [3710, 720, 714, 6370]),
        (15710, [4822, 784, 748, 9356]),
        (30242, [8456, 784, 874, 20128]),
    ] {
        let sizes = sizes(free * MIB, 8, 8).unwrap();
        assert_eq!(
            [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB),
            expected,
            "{free} MiB free"
        );
    }
}

/// Installed GiB and cores to the split, and the guest MiB the sizing of
/// v0.1.0 gave (8 GiB: +372 at 2 cores, +226 at 4, +16 at 8; 16 GiB: -1118
/// at 4, -810 at 8; 32 GiB: -2680; 64 GiB: -4632; 128 GiB: +1892). Past 8
/// GiB the heap's quarter of the free RAM takes more than the guest gained
/// since; the guest has more than v0.1.0 gave but for what the heap takes
/// past its floor.
#[test]
fn host_sizes_split_as_expected() {
    for (installed, cores, expected, before) in [
        (8, 2, [2496, 196, 158, 5086], 4714),
        (8, 4, [2688, 392, 284, 4572], 4346),
        (8, 8, [2800, 512, 512, 4112], 4096),
        (16, 4, [4416, 392, 382, 10682], 11800),
        (16, 8, [4864, 784, 750, 9474], 10284),
        (32, 8, [8832, 784, 888, 21240], 23920),
        (64, 8, [16384, 784, 1168, 45152], 49784),
        (128, 8, [16384, 784, 1904, 107904], 106012),
    ] {
        let sizes = sizes(free(installed), cores, cores).unwrap();
        let split = [sizes.heap, sizes.code.bytes, sizes.pool, sizes.guest].map(|b| b / MIB);
        assert_eq!(split, expected, "{installed} GiB, {cores} cores");
        let floor = (HEAP_FLOOR + sizes.code.window - FLOOR_WINDOW) / MIB;
        assert!(
            split[3] + (split[0] - floor) > before,
            "{installed} GiB, {cores} cores"
        );
        // From 16 GiB up each runner's user half holds at least the 52 MiB
        // the 12 GiB VM filled, beside its kernel half.
        if installed >= 16 {
            let per = sizes.pool / cores as u64;
            assert!(per - upper_half(per, sizes.guest, 4096) >= 52 * MIB);
        }
    }
}

#[test]
fn a_2_tib_host_caps_every_part() {
    let sizes = sizes(free(2048), 8, 8).unwrap();
    assert_eq!(sizes.heap, HEAP_CEILING);
    assert_eq!(sizes.code.bytes, 784 * MIB);
    assert_eq!(sizes.pool, 8 * POOL_PER_RUNNER);
    assert_eq!(sizes.guest, free(2048) - 24 * GIB - 784 * MIB);
}

#[test]
fn the_heap_grows_with_the_host_between_its_bounds() {
    // A quarter of the free RAM reaches the ceiling from about 60 GiB.
    let small = sizes(free(16), 8, 8).unwrap().heap;
    let large = sizes(free(32), 8, 8).unwrap().heap;
    assert!(HEAP_FLOOR < small && small < large && large < HEAP_CEILING);
}

#[test]
fn too_little_ram_says_how_much_is_needed() {
    let refusal = sizes(4 * GIB, 8, 8).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "this PC has 4.0 GB of RAM free; Stela needs at least 7 GB free: \
         3 GB for macOS and the rest for its own memory"
    );
    let two_cores = sizes(5 * GIB, 2, 2).unwrap_err();
    assert!(
        two_cores.to_string().contains("at least 6 GB"),
        "{two_cores}"
    );
    assert!(sizes(6 * GIB, 2, 2).is_ok());
    assert!(sizes(free(8), 2, 2).is_ok());
}

#[test]
fn parked_executors_add_only_their_reserves() {
    let parked = sizes(free(16), 12, 8).unwrap();
    let rest = free(16) - parked.heap - parked.code.bytes;
    let share = 8 * rest / (POOL_SHARE + 8);
    let grown = 8 * (2048 * (LOWER_BASE + UPPER_BASE) + 3 * rest) / (2048 + 3 * 8);
    let running = share.min(grown) / ALIGNMENT * ALIGNMENT;
    assert_eq!(parked.pool, running + 4 * 4 * MIB);
}

/// The free RAM an 8 GB host with eight cores shows the loader with no pack
/// read (`HOST MEMORY free=7544M`).
const HOST_8_GB: u64 = 7544 * MIB;

#[test]
fn an_8_gb_pc_keeps_a_sixteenth_for_the_pack() {
    let budget = pack_budget(HOST_8_GB, 8, 8);
    assert_eq!(budget, 470 * MIB);
    // An 845 MB pack, which stopped such a host's boots, no longer fits whole,
    // and what is read of it leaves the parts and the guest's floor.
    assert!(budget < 845_496_619);
    let sizes = sizes(HOST_8_GB - budget - PACK_MARGIN, 8, 8).unwrap();
    assert!(sizes.guest > GUEST_FLOOR + 200 * MIB, "{sizes:?}");
    assert_eq!(pack_budget(free(8), 8, 8), 496 * MIB);
}

#[test]
fn a_16_gb_pc_keeps_about_a_gigabyte_for_the_pack() {
    assert_eq!(pack_budget(free(16), 8, 8), 992 * MIB);
}

#[test]
fn a_128_gb_pc_keeps_more_for_the_pack() {
    let budget = pack_budget(free(128), 16, 16);
    assert_eq!(budget, 7936 * MIB);
    let sizes = sizes(free(128) - budget - PACK_MARGIN, 16, 16).unwrap();
    assert!(sizes.guest > 72 * GIB, "{sizes:?}");
}

#[test]
fn the_pack_gets_only_what_the_parts_and_the_guest_leave() {
    // Just above what eight cores need, the pack gets the rest, not its
    // share; below it, nothing, and the boot goes on without a pack.
    let needed = (6000..7200)
        .map(|mib| mib * MIB)
        .find(|&free| sizes(free, 8, 8).is_ok())
        .unwrap();
    assert_eq!(pack_budget(needed, 8, 8), 0);
    // 100 MiB more leaves the pack about that much, less the margin: the
    // most that still fits.
    let budget = pack_budget(needed + 100 * MIB, 8, 8);
    assert!((PACK_FLOOR..=100 * MIB).contains(&budget), "{budget}");
    assert!(sizes(needed + 100 * MIB - budget - PACK_MARGIN, 8, 8).is_ok());
    assert!(sizes(needed + 100 * MIB - budget - ALIGNMENT - PACK_MARGIN, 8, 8).is_err());
    assert_eq!(pack_budget(4 * GIB, 8, 8), 0);
    for cores in [1, 2, 4, 8, 16, 64] {
        for free in (2 * GIB..40 * GIB).step_by(37 * MIB as usize) {
            let budget = pack_budget(free, cores, cores);
            assert!(budget.is_multiple_of(ALIGNMENT));
            assert!(budget <= free / PACK_SHARE);
            if budget != 0 {
                assert!(budget >= PACK_FLOOR);
                let left = free - budget - PACK_MARGIN;
                assert!(sizes(left, cores, cores).is_ok(), "{free} {cores}");
            }
        }
    }
}

#[test]
fn the_guest_reports_the_dram_that_backs_it_not_the_hosts() {
    // The 8 GiB bench: 3920 MiB backed of 8190 MiB installed.
    let dram = guest_dram(0x1_ffe0_0000, 0xf500_0000);
    assert_eq!(dram.reported, 0xf500_0000);
    assert_eq!(dram.kept, 0x1_0ae0_0000);
    // `memSize` (the managed range above the carveouts and below the
    // framebuffer) stays within it, and tag storage sized from it lies at
    // the managed range's top.
    let dram_base = 0x100_0000_0000;
    let phys_base = dram_base + 32 * MIB;
    let managed = dram.reported - 32 * MIB - 16 * MIB;
    let value = stela_contracts::tag_storage::hand_off(phys_base, managed, dram.reported).unwrap();
    let base = stela_contracts::tag_storage::base(value);
    assert!(base >= phys_base);
    assert!(base + stela_contracts::tag_storage::bytes(dram.reported) <= phys_base + managed);
    // A host that backs everything it has keeps nothing.
    assert_eq!(guest_dram(8 * GIB, 8 * GIB).kept, 0);
}

/// Each runner's pool splits into the kernel half's 32 MiB and 1024th of
/// the guest and the user half's 48 MiB and 2048th; a pool smaller than
/// both (a sixty-fourth of a small guest, at 8 GiB) splits in their ratio.
#[test]
fn each_runners_pool_gives_the_kernel_half_what_it_needs_and_the_user_half_the_rest() {
    const PAGE: u64 = 4096;
    const ROOTS: u64 = 64 * PAGE;
    for (installed, sized) in [(8, false), (12, true), (16, true), (64, true)] {
        let sizes = sizes(free(installed), 8, 8).unwrap();
        let shadow = sizes.pool / 8 / PAGE * PAGE - ROOTS;
        let upper = upper_half(shadow, sizes.guest, PAGE);
        let lower = shadow - upper;
        let (lower_wanted, upper_wanted) = pool_halves(sizes.guest);
        assert!(upper.is_multiple_of(PAGE));
        assert!(upper <= upper_wanted, "{installed} GiB");
        if sized {
            // Both sizes, less the roots and the pool's 2 MiB rounding.
            assert!(upper + ROOTS + 2 * MIB >= upper_wanted, "{installed} GiB");
            assert!(lower + ROOTS + 2 * MIB >= lower_wanted, "{installed} GiB");
        } else {
            // Short of both: each the part's share in their ratio.
            let ratio = |half: u64| half as f64 / (lower_wanted + upper_wanted) as f64;
            let share = |bytes: u64| bytes as f64 / shadow as f64;
            assert!(
                (share(upper) - ratio(upper_wanted)).abs() < 0.01,
                "{installed} GiB"
            );
            assert!(lower > upper, "{installed} GiB");
        }
    }
    // The 16 GiB host's kernel half: 32 MiB and a MiB per GiB of its
    // 9.25 GiB guest.
    let sizes = sizes(free(16), 8, 8).unwrap();
    let upper = upper_half(sizes.pool / 8 / 4096 * 4096 - ROOTS, sizes.guest, PAGE);
    assert!((41..=42).contains(&(upper / MIB)));
    // A tiny pool's halves never go below a page each.
    assert_eq!(upper_half(2 * PAGE, 64 * GIB, PAGE), PAGE);
}
