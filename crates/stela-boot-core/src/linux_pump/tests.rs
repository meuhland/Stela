use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

/// Microsecond ticks, as `stela-boot`'s limits: 1 ms unmasked, 4 ms longest.
const LIMITS: Limits = Limits {
    unmasked: 1000,
    longest: 4000,
};
/// How often a boot PE that holds work back looks again (`RECHECK_NS`).
const RECHECK: u64 = 250;

#[test]
fn boot_decision_keeps_the_deferral_limits() {
    assert_eq!(boot_decision(Calm::Quiet, 0, LIMITS), Some(BootPump::Quiet));
    assert_eq!(boot_decision(Calm::Unmasked, 999, LIMITS), None);
    assert_eq!(
        boot_decision(Calm::Unmasked, 1000, LIMITS),
        Some(BootPump::Unmasked)
    );
    assert_eq!(boot_decision(Calm::Masked, 3999, LIMITS), None);
    assert_eq!(
        boot_decision(Calm::Masked, 4000, LIMITS),
        Some(BootPump::Forced)
    );
    assert_eq!(
        boot_decision(Calm::Quiet, 4000, LIMITS),
        Some(BootPump::Forced)
    );
}

/// A sleeping PE in the model: woken by a kick, it reaches its idle service
/// `latency` ticks later and pumps there unless its guest got work first.
struct Sleeper {
    latency: u64,
    /// Its guest has work when it wakes, so it serves its guest instead.
    busy_on_wake: bool,
}

/// One event posted at tick 0 while the boot PE runs guest code that is
/// `calm` throughout: who pumps it, and when. Mirrors `stela-boot`: the
/// event kicks the first sleeper, else calls the boot PE; the boot PE looks
/// at each slice end ([`RECHECK`] while work waits) and pumps by
/// [`boot_decision`]; every pump takes the [`Turn`] first.
fn serve(calm: Calm, sleepers: &[Sleeper]) -> (Taker, u64) {
    let turn = Turn::new();
    let boot = 0;
    let ready_since = 0u64;
    // `Events::notify`: the first sleeper is kicked; with none, the boot
    // PE's slice ends at once.
    let kicked = sleepers.first();
    let mut boot_look = if kicked.is_some() { RECHECK } else { 0 };
    let wake = kicked.map(|sleeper| sleeper.latency);
    let mut served = None;
    for now in 0..=LIMITS.longest {
        if served.is_some() {
            break;
        }
        if let (Some(at), Some(sleeper)) = (wake, kicked)
            && now == at
            && !sleeper.busy_on_wake
            && let Some(_held) = turn.try_take(1)
        {
            served = Some((Taker::Waiting, now));
            continue;
        }
        if now == boot_look {
            match boot_decision(calm, now - ready_since, LIMITS) {
                Some(_) => {
                    let _held = turn.try_take(boot).expect("free turn");
                    served = Some((Taker::Boot, now));
                }
                None => boot_look = now + RECHECK,
            }
        }
    }
    served.expect("work served within the longest limit")
}

#[test]
fn a_waiting_pe_serves_an_event_within_its_wake_latency() {
    for calm in [Calm::Unmasked, Calm::Masked] {
        let sleepers = [Sleeper {
            latency: 30,
            busy_on_wake: false,
        }];
        assert_eq!(serve(calm, &sleepers), (Taker::Waiting, 30));
    }
}

#[test]
fn with_no_waiting_pe_the_boot_pe_keeps_the_old_limits() {
    assert_eq!(serve(Calm::Quiet, &[]), (Taker::Boot, 0));
    assert_eq!(serve(Calm::Unmasked, &[]), (Taker::Boot, LIMITS.unmasked));
    assert_eq!(serve(Calm::Masked, &[]), (Taker::Boot, LIMITS.longest));
}

#[test]
fn a_woken_pe_with_guest_work_leaves_the_event_to_the_boot_pe() {
    let sleepers = [Sleeper {
        latency: 30,
        busy_on_wake: true,
    }];
    assert_eq!(
        serve(Calm::Unmasked, &sleepers),
        (Taker::Boot, LIMITS.unmasked)
    );
    assert_eq!(
        serve(Calm::Masked, &sleepers),
        (Taker::Boot, LIMITS.longest)
    );
}

#[test]
fn the_turn_has_one_holder_at_a_time() {
    let turn = Turn::new();
    let held = turn.try_take(3).expect("free");
    assert_eq!(turn.holder(), Some(3));
    assert!(turn.try_take(0).is_none());
    assert!(turn.try_take(3).is_none(), "not taken twice");
    drop(held);
    assert_eq!(turn.holder(), None);
    assert!(turn.try_take(0).is_some());
    assert!(turn.try_take(u32::MAX).is_none(), "no index past the range");
}

#[test]
fn the_turn_excludes_concurrent_pumpers() {
    let turn = Arc::new(Turn::new());
    let inside = Arc::new(AtomicU32::new(0));
    let overlap = Arc::new(AtomicBool::new(false));
    let pumps = Arc::new(AtomicU32::new(0));
    let threads: Vec<_> = (0..8)
        .map(|processor| {
            let (turn, inside, overlap, pumps) =
                (turn.clone(), inside.clone(), overlap.clone(), pumps.clone());
            std::thread::spawn(move || {
                for _ in 0..20_000 {
                    if let Some(_held) = turn.try_take(processor) {
                        if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                            overlap.store(true, Ordering::SeqCst);
                        }
                        pumps.fetch_add(1, Ordering::Relaxed);
                        std::hint::spin_loop();
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert!(!overlap.load(Ordering::SeqCst));
    assert!(pumps.load(Ordering::Relaxed) > 0);
    assert_eq!(turn.holder(), None);
}

#[test]
fn one_wake_for_each_ready_episode() {
    let wakes = Wakes::new();
    assert!(!wakes.first_for(0), "no episode");
    assert!(wakes.first_for(100));
    assert!(!wakes.first_for(100), "the same episode again");
    assert!(wakes.first_for(350));
    assert!(wakes.first_for(100), "a new episode with an older start");
}

#[test]
fn a_clock_sampled_on_two_processors_stays_monotonic() {
    assert_eq!(advance(100, 100, 64), Sample::Same);
    assert_eq!(advance(100, 160, 64), Sample::Moved { delta: 60 });
    assert_eq!(
        advance(u64::MAX - 1, 3, 64),
        Sample::Moved { delta: 5 },
        "wraps"
    );
    assert_eq!(advance(100, 40, 64), Sample::Behind);
    assert_eq!(advance(100, 36, 64), Sample::Behind);
    assert_eq!(advance(100, 35, 64), Sample::Corrupt);
    assert_eq!(advance(100, 99, 0), Sample::Corrupt, "no skew allowed");
    // A sequence of reads from two counters 20 ticks apart: the count only
    // grows, by one tick for each read behind.
    let (mut previous, mut elapsed) = (1000u64, 0u64);
    for (raw, expected) in [(1010, 10), (995, 11), (1012, 13), (1030, 31), (1011, 32)] {
        match advance(previous, raw, 64) {
            Sample::Moved { delta } => {
                previous = raw;
                elapsed += delta;
            }
            Sample::Behind => elapsed += 1,
            other => panic!("{other:?}"),
        }
        assert_eq!(elapsed, expected);
    }
}
