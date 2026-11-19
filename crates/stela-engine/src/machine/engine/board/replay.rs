//! A host replay of the ring under sign-in's working set: the real
//! [`Ring`] and ghosts ([`Churn`]) take the installs of a guest that keeps
//! running a hot pool of blocks larger than a 512 MiB window holds and
//! runs a stream of new blocks once each, as run 20261119-021855 showed
//! (0.6-1.1 million installs a heartbeat, 1.6 million new blocks over 18
//! heartbeats of sign-in, about 1.05 million blocks in the window). Scaled
//! down a hundredfold: a window of 512 MiB holds 10 500 blocks.
//!
//! A run of a block the ring holds counts its part's heat, one run in
//! eight as the sampled transfers do; a run of a block it does not hold
//! installs it, in the tenured generation when its ghost says the ring took
//! it within the tenure age ([`Churn::promotes`]).
use super::super::churn::{self, Churn, ChurnCounts, Gone};
use super::{Advance, Grace, Ring, TENURED, YOUNG};
use alloc::{vec, vec::Vec};

const BLOCK_BYTES: usize = 64;
const PARTS: usize = 32;
const TICKS: u64 = 20;
const WARM_BEATS: u64 = 6;
const BEATS: u64 = 18;
/// Sampled heat: one run in this many counts.
const SAMPLE: u64 = 8;

/// The guest: `hot` blocks run with odds per beat between `least` and 4
/// runs (log-uniform), and `new` blocks a beat that run once.
#[derive(Clone, Copy, Debug)]
pub(super) struct Guest {
    pub(super) hot: usize,
    pub(super) least: f64,
    pub(super) new: usize,
}

/// What a replay counted per beat after its warm-up.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Outcome {
    pub(super) installs: u64,
    /// Installs of blocks installed before.
    pub(super) again: u64,
    pub(super) promoted: u64,
    pub(super) retired: u64,
}

struct Model {
    ring: Ring,
    grace: Grace,
    churn: Churn,
    counts: ChurnCounts,
    part_of: Vec<Option<u32>>,
    blocks_of: Vec<Vec<usize>>,
    seen: Vec<bool>,
    outcome: Outcome,
    counting: bool,
}

fn identity(block: usize) -> u64 {
    churn::identity(
        0x1_0000_0000 + 4 * block as u64,
        0x8000_0000 + 4 * block as u64,
        0,
    )
}

impl Model {
    fn retire(&mut self, part: u32) {
        let generation = self.ring.generation();
        for block in core::mem::take(&mut self.blocks_of[part as usize]) {
            self.part_of[block] = None;
            self.churn
                .gone(&mut self.counts, identity(block), 0, generation, Gone::Ring);
        }
        self.ring.retired(part, self.grace.snapshot());
        self.outcome.retired += u64::from(self.counting);
    }

    fn install(&mut self, block: usize) {
        let id = identity(block);
        let promoted = self
            .churn
            .promotes(id, self.ring.generation(), PARTS as u32);
        let stream = if promoted { TENURED } else { YOUNG };
        let place = loop {
            match self.ring.take(stream, BLOCK_BYTES, 16) {
                Ok(place) => break place,
                Err(full) => match self.ring.advance(stream, full, &self.grace, None) {
                    Advance::Moved => {}
                    Advance::Retire(part, _) => self.retire(part),
                    other => panic!("a ring nobody holds: {other:?}"),
                },
            }
        };
        self.ring.release(place);
        self.churn.installed(
            &mut self.counts,
            id,
            0,
            self.ring.generation(),
            None,
            PARTS as u32,
        );
        self.ring.installed(place.part as usize);
        self.part_of[block] = Some(place.part);
        self.blocks_of[place.part as usize].push(block);
        if self.counting {
            self.outcome.installs += 1;
            self.outcome.again += u64::from(self.seen[block]);
            self.outcome.promoted += u64::from(promoted);
        }
        self.seen[block] = true;
    }
}

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn uniform(seed: &mut u64) -> f64 {
    (next(seed) >> 11) as f64 / (1u64 << 53) as f64
}

/// Replay `guest` on a window of `blocks` blocks whose young code keeps a
/// `nursery`th of the parts: per beat, after the warm-up.
pub(super) fn replay(guest: Guest, blocks: usize, nursery: u32) -> Outcome {
    let per_part = blocks / PARTS;
    let total = guest.hot + guest.new * (WARM_BEATS + BEATS) as usize + 1;
    let mut model = Model {
        ring: Ring::with_nursery(PARTS, per_part * BLOCK_BYTES, nursery),
        grace: Grace::new(1),
        churn: Churn::new(1 << 20),
        counts: ChurnCounts::default(),
        part_of: vec![None; total],
        blocks_of: vec![Vec::new(); PARTS],
        seen: vec![false; total],
        outcome: Outcome::default(),
        counting: false,
    };
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let odds: Vec<f64> = (0..guest.hot)
        .map(|_| (guest.least.ln() * (1.0 - uniform(&mut seed))).exp() * 4.0 / TICKS as f64)
        .collect();
    let mut fresh = guest.hot;
    let mut runs = Vec::new();
    for beat in 0..WARM_BEATS + BEATS {
        model.counting = beat >= WARM_BEATS;
        for _ in 0..TICKS {
            runs.clear();
            runs.extend((0..guest.hot).filter(|&block| uniform(&mut seed) < odds[block]));
            for _ in 0..guest.new / TICKS as usize {
                runs.push(fresh);
                fresh += 1;
            }
            // Shuffle: the guest's runs interleave.
            for at in (1..runs.len()).rev() {
                runs.swap(at, next(&mut seed) as usize % (at + 1));
            }
            for &block in &runs {
                match model.part_of[block] {
                    Some(part) => {
                        if next(&mut seed).is_multiple_of(SAMPLE) {
                            model.ring.entered(part as usize);
                        }
                    }
                    None => model.install(block),
                }
            }
        }
    }
    let beats = BEATS;
    let outcome = model.outcome;
    Outcome {
        installs: outcome.installs / beats,
        again: outcome.again / beats,
        promoted: outcome.promoted / beats,
        retired: outcome.retired / beats,
    }
}

/// Sign-in's hot pool (1.3 million blocks unscaled, 0.25-1 of 4 runs a
/// beat) and 90 thousand new blocks a beat: on a 512 MiB window with a
/// quarter of it young, as before, the ring installs about 1.1 million
/// blocks again a beat, as run 20261119-021855 did; on the 816 MiB window
/// a 12 GiB host now gets, with an eighth young, under a tenth of that.
#[test]
fn a_window_that_holds_sign_ins_hot_code_with_a_small_nursery_keeps_it() {
    let guest = Guest {
        hot: 13_000,
        least: 0.25,
        new: 900,
    };
    let before = replay(guest, 10_500, 4);
    let after = replay(guest, 16_700, super::ring::NURSERY_SHARE);
    assert!(before.again >= 8_000, "{before:?}");
    assert!(after.again * 10 <= before.again, "{before:?} {after:?}");
    // A larger window alone, its nursery a quarter, keeps less than half.
    let larger = replay(guest, 16_700, 4);
    assert!(larger.again > 2 * after.again, "{larger:?} {after:?}");
}

/// The table behind it: hot pools, windows and nurseries.
#[test]
#[ignore = "manual: the table"]
fn sign_in_replay_on_the_real_ring() {
    for hot in [9_500, 13_000, 14_000] {
        let guest = Guest {
            hot,
            least: 0.25,
            new: 900,
        };
        for (window, blocks) in [(512, 10_500), (816, 16_700), (1024, 21_000)] {
            for nursery in [4, 8, 16] {
                let outcome = replay(guest, blocks, nursery);
                std::println!(
                    "REPLAY hot={hot}00 window={window}M nursery=1/{nursery} installs={} again={} \
                 promoted={} retired={} (per beat, x100 for the VM)",
                    outcome.installs,
                    outcome.again,
                    outcome.promoted,
                    outcome.retired
                );
            }
        }
    }
}
