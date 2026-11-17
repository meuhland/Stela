use super::*;
use alloc::{vec, vec::Vec};

/// A display output device whose `_BQC` answers `level` (as an index of
/// `steps` where `indexed`), or nothing; every `_BCM` is recorded.
struct Fake {
    query: bool,
    level: u64,
    indexed: Option<Vec<u64>>,
    /// `_BQC` answers this until the first `_BCM`, as firmware that
    /// leaves its variable uninitialized.
    first: Option<u64>,
    sets: Vec<u64>,
}

impl Fake {
    fn new(level: u64) -> Self {
        Self {
            query: true,
            level,
            indexed: None,
            first: None,
            sets: Vec::new(),
        }
    }
}

impl Panel for Fake {
    fn current(&mut self) -> Option<u64> {
        if !self.query {
            return None;
        }
        if self.sets.is_empty()
            && let Some(first) = self.first
        {
            return Some(first);
        }
        Some(match &self.indexed {
            Some(steps) => steps.iter().position(|&step| step == self.level)? as u64,
            None => self.level,
        })
    }

    fn set(&mut self, level: u64) -> Result<(), Error> {
        self.sets.push(level);
        self.level = level;
        Ok(())
    }
}

fn parsed(listed: &[u64]) -> Levels {
    Levels::parse(&listed.iter().copied().map(Some).collect::<Vec<_>>()).unwrap()
}

#[test]
fn bcl_is_read_as_linux_reads_it() {
    // The usual list: AC and battery levels, then the steps ascending.
    let plain = parsed(&[100, 40, 10, 40, 70, 100]);
    assert_eq!(
        (plain.steps(), plain.max, plain.reversed),
        (&[10, 40, 70, 100][..], 100, false)
    );
    // Repeats after the first two are dropped where they follow each other.
    assert_eq!(
        parsed(&[100, 40, 10, 10, 40, 100, 100]).steps(),
        [10, 40, 100]
    );
    // Steps listed highest first are sorted.
    let reversed = parsed(&[100, 40, 100, 70, 40, 10]);
    assert_eq!(
        (reversed.steps(), reversed.reversed),
        (&[10, 40, 70, 100][..], true)
    );
    // First two entries missing from the steps lead them.
    assert_eq!(parsed(&[50, 30, 10, 60, 80]).steps(), [50, 30, 10, 60, 80]);
    assert_eq!(parsed(&[90, 50, 10, 50, 80]).steps(), [50, 10, 50, 80]);
    // Levels are 32-bit; entries that are not integers are skipped.
    let wide = Levels::parse(&[Some(1 << 32 | 100), Some(40), None, Some(40), Some(100)]);
    assert_eq!(wide.unwrap().steps(), [40, 100]);
    assert_eq!(Levels::parse(&[Some(100)]), None);
    assert_eq!(Levels::parse(&[None, None, Some(5)]), None);
}

#[test]
fn a_listed_level_read_back_is_kept() {
    let levels = parsed(&[100, 40, 10, 40, 70, 100]);
    let mut panel = Fake::new(70);
    let set = initialize(&levels, &mut panel).unwrap();
    assert_eq!(
        set,
        Brightness {
            level: 70,
            max: 100,
            read: Some(70)
        }
    );
    // The test level, then the level read.
    assert_eq!(panel.sets, [100, 70]);
    // At the maximum, the test is the second step.
    let mut full = Fake::new(100);
    assert_eq!(initialize(&levels, &mut full).unwrap().level, 100);
    assert_eq!(full.sets, [40, 100]);
}

/// A GPU reset or firmware that leaves the level at zero, or one `_BCL`
/// does not list, gets the highest level: the backlight comes on.
#[test]
fn a_zero_or_unlisted_level_becomes_the_highest() {
    let levels = parsed(&[100, 40, 0, 10, 40, 70, 100]);
    for read in [0, 55] {
        let mut panel = Fake::new(read);
        let set = initialize(&levels, &mut panel).unwrap();
        assert_eq!((set.level, set.read), (100, Some(read)));
        assert_eq!(panel.sets.last(), Some(&100));
    }
    // No `_BQC`: the highest at once.
    let mut blind = Fake::new(0);
    blind.query = false;
    assert_eq!(initialize(&levels, &mut blind).unwrap().read, None);
    assert_eq!(blind.sets, [100]);
}

#[test]
fn a_bqc_that_answers_indices_is_read_as_one() {
    let listed = [100, 40, 10, 40, 70, 100];
    let levels = parsed(&listed);
    let mut panel = Fake::new(40);
    panel.indexed = Some(levels.steps().to_vec());
    // It reads index 1, which is 40 of the steps.
    let set = initialize(&levels, &mut panel).unwrap();
    assert_eq!((set.level, set.read), (40, Some(40)));
    // Highest first, an index counts from the top as listed.
    let reversed = parsed(&[100, 40, 100, 70, 40, 10]);
    assert_eq!(reversed.indexed(0), Some(100));
    assert_eq!(reversed.indexed(3), Some(10));
    assert_eq!(reversed.indexed(4), None);
}

#[test]
fn a_bqc_that_answers_neither_is_not_trusted() {
    let levels = parsed(&[100, 40, 10, 40, 70, 100]);
    // Uninitialized at first, then always 7: neither a level nor an index.
    let mut panel = Fake::new(7);
    panel.first = Some(70);
    panel.indexed = None;
    let mut stuck = StuckAt(7, vec![]);
    let set = initialize(&levels, &mut stuck).unwrap();
    assert_eq!((set.level, set.read), (100, None));
    assert_eq!(stuck.1, [100, 100]);
    // Where the test level reads back, the first read is kept when it is
    // listed, as Linux keeps it.
    assert_eq!(initialize(&levels, &mut panel).unwrap().level, 70);
}

/// `_BQC` always answers the same value.
struct StuckAt(u64, Vec<u64>);

impl Panel for StuckAt {
    fn current(&mut self) -> Option<u64> {
        Some(self.0)
    }

    fn set(&mut self, level: u64) -> Result<(), Error> {
        self.1.push(level);
        Ok(())
    }
}

#[test]
fn a_refused_bcm_is_answered() {
    struct Refusing;
    impl Panel for Refusing {
        fn current(&mut self) -> Option<u64> {
            None
        }
        fn set(&mut self, _: u64) -> Result<(), Error> {
            Err(Error(5))
        }
    }
    let levels = parsed(&[100, 40, 10, 100]);
    assert_eq!(initialize(&levels, &mut Refusing), Err(Error(5)));
}
