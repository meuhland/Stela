use super::*;

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
