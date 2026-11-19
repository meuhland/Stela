//! When a video engine's job should be done, and when its frontend should
//! call again: the pacing the encoders' ([`super::encoders`]) and the
//! decoders' ([`super::decoders`]) calls share.
//!
//! A call submits a job (nouveau's `EXEC`) and then looks at its semaphore
//! until it lands or the call's budget is spent. A job expected past the
//! budget is answered `Pending` at once, with `retry_us`: the frontend
//! sleeps that long and calls again. Two numbers decide both:
//!
//! - **The engine's expected time**, from the submit to the semaphore. A
//!   look that sees the job land right after a look that missed it
//!   ([`ACCURATE_NS`]) measures it. A job first seen at the first look of a
//!   later call (after the frontend's sleep) only bounds it: done after the
//!   last miss, before the hit. Those bounds correct the expectation too,
//!   or it never moves once it ends past the budget: NVENC's HEVC kept the
//!   self-test's 5.8 ms (the GPU's low P-state) with an engine at 1.85 ms,
//!   so every frame slept 5.8 ms and woke ~3.5 ms later (9.3 ms `gpu_us`);
//!   NVDEC's 4K Main 10 learnt the sleeps themselves into its expectation
//!   (11.5 ms `gpu_us`).
//! - **The frontend's wake-up**: how much later than asked its next call
//!   comes (the guest's sleep, its scheduling, the gate's entry), a few
//!   milliseconds on this machine. The wait asked for is the time left less
//!   that, so the call comes back just before the job lands and sees it
//!   land, which keeps the expectation measured.
//!
//! A job seen done at the first look of a call whose frontend came late is
//! the sign of an expectation too high: it shrinks by an eighth (never below
//! the last miss), so the next calls come earlier, until one sees the job
//! land and measures it.

/// A completion seen within this of the look before it measures the
/// engine's time.
pub const ACCURATE_NS: u64 = 100_000;
/// The `retry_us` of a `Pending` answer: at once, for a call that only ran
/// out of its budget; the bounds of a wait for the engine; and the wait for
/// a job later than expected.
pub const RETRY_AT_ONCE_US: u32 = 1;
pub const RETRY_LEAST_US: u64 = 50;
pub const RETRY_MOST_US: u64 = 8_000;
pub const RETRY_LATE_US: u32 = 300;
/// A wait the frontend does not sleep for (the bundle's `Gate.m`): at most
/// this many microseconds.
const NO_SLEEP_US: u32 = 20;
/// The frontend's wake-up before any is measured, and the most one counts.
const WAKE_PRIOR_NS: u64 = 1_000_000;
const WAKE_MOST_NS: u64 = 4_000_000;
/// The least expected time.
const ENGINE_LEAST_NS: u64 = 100_000;

/// One session's pacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pace {
    engine_ns: u64,
    wake_ns: u64,
    /// When the last `Pending` answer asked to be called again, while that
    /// call has not come.
    due: Option<u64>,
}

impl Pace {
    /// A session whose engine is expected to take `engine_ns` a job.
    pub const fn new(engine_ns: u64) -> Self {
        Self {
            engine_ns,
            wake_ns: WAKE_PRIOR_NS,
            due: None,
        }
    }

    /// The engine's expected time for a job.
    #[cfg(test)]
    pub fn engine_ns(&self) -> u64 {
        self.engine_ns
    }

    /// How much later than asked the frontend's calls come.
    #[cfg(test)]
    pub fn wake_ns(&self) -> u64 {
        self.wake_ns
    }

    /// When a job submitted at `submitted` should land.
    pub fn expected(&self, submitted: u64) -> u64 {
        submitted.saturating_add(self.engine_ns)
    }

    /// A call of the session came at `now`: after a `Pending` answer's
    /// wait, it measures the frontend's wake-up.
    pub fn arrived(&mut self, now: u64) {
        if let Some(due) = self.due.take() {
            let late = now.saturating_sub(due).min(WAKE_MOST_NS);
            self.wake_ns = (3 * self.wake_ns + late) / 4;
        }
    }

    /// Whether a call whose budget ends at `budget_end` keeps looking, at
    /// `now`, at a job submitted at `submitted`: not past its budget, and
    /// not for a job expected only after it (a job already late is looked
    /// at until the budget ends).
    pub fn keep_looking(&self, now: u64, submitted: u64, budget_end: u64) -> bool {
        let expected = self.expected(submitted);
        now < budget_end && !(expected > budget_end && now < expected)
    }

    /// The `retry_us` of a `Pending` answer at `now` for a job submitted at
    /// `submitted`: the time left to its landing, less the frontend's
    /// wake-up, or the late wait. The call it asks for is then due.
    pub fn retry_us(&mut self, now: u64, submitted: u64) -> u32 {
        let expected = self.expected(submitted);
        let retry_us = if expected > now {
            ((expected - now).saturating_sub(self.wake_ns) / 1000)
                .clamp(RETRY_LEAST_US, RETRY_MOST_US) as u32
        } else {
            RETRY_LATE_US
        };
        self.asked(now, retry_us);
        retry_us
    }

    /// A `Pending` answer at `now` asked for `retry_us`: the call after a
    /// sleep is due then. A wait the frontend does not sleep says nothing
    /// of its wake-up.
    pub fn asked(&mut self, now: u64, retry_us: u32) {
        self.due = (retry_us > NO_SLEEP_US).then(|| now.saturating_add(u64::from(retry_us) * 1000));
    }

    /// The job submitted at `submitted` was seen done at `seen`, the look
    /// before it at `looked` (`submitted` if none): the engine's time when
    /// the two looks were close, which the expectation follows. Otherwise
    /// the two looks bound it: an expectation below the miss rises past it
    /// (halfway to the hit, at most half the miss again), one above the hit
    /// falls to it, and one between them shrinks by an eighth, for the
    /// frontend came back after the job landed.
    pub fn done(&mut self, submitted: u64, looked: u64, seen: u64) -> Option<u64> {
        let took = seen.saturating_sub(submitted);
        if seen.saturating_sub(looked) <= ACCURATE_NS {
            self.engine_ns = ((3 * self.engine_ns + took) / 4).max(ENGINE_LEAST_NS);
            return Some(took);
        }
        let missed = looked.saturating_sub(submitted);
        self.engine_ns = if missed >= self.engine_ns {
            missed + (took - missed).min(missed) / 2
        } else if took < self.engine_ns {
            took
        } else {
            (self.engine_ns - self.engine_ns / 8).max(missed)
        }
        .max(ENGINE_LEAST_NS);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One job on a model of a frontend that sleeps what it is asked plus
    /// `wake_ns`, with a call budget of 4.5 ms whose submit comes `setup_ns`
    /// into it, for an engine of `engine_ns`: the job's time from submit to
    /// the look that saw it, and the calls it took.
    fn job(
        pace: &mut Pace,
        engine_ns: u64,
        setup_ns: u64,
        wake_ns: u64,
        now: &mut u64,
    ) -> (u64, u32) {
        const BUDGET: u64 = 4_500_000;
        const LOOK: u64 = 10_000;
        let mut started = *now;
        pace.arrived(started);
        let submitted = started + setup_ns;
        let lands = submitted + engine_ns;
        let mut at = submitted;
        let mut looked = submitted;
        let mut calls = 1;
        loop {
            if at >= lands {
                pace.done(submitted, looked, at);
                *now = at;
                return (at - submitted, calls);
            }
            looked = at;
            if pace.keep_looking(at, submitted, started + BUDGET) {
                at += LOOK;
                continue;
            }
            let retry = pace.retry_us(at, submitted);
            started = at + u64::from(retry) * 1000 + wake_ns;
            pace.arrived(started);
            at = started;
            calls += 1;
        }
    }

    #[test]
    fn a_stale_high_expectation_comes_down_to_the_engine() {
        // NVENC's HEVC: expected 5.8 ms, engine 1.85 ms, the store and
        // EXEC 3.7 ms of the submitting call, a frontend that wakes 3.5 ms
        // late. The job lands past the call's budget, so the frontend's
        // wake-up is the floor: it was seen 9.3 ms after its submit.
        let mut pace = Pace::new(5_800_000);
        let mut now = 0;
        let first = job(&mut pace, 1_850_000, 3_700_000, 3_500_000, &mut now).0;
        assert!(first > 8_000_000, "{first}");
        let mut seen = 0;
        for _ in 0..20 {
            job(&mut pace, 1_850_000, 3_700_000, 3_500_000, &mut now);
        }
        for _ in 0..40 {
            let (at, calls) = job(&mut pace, 1_850_000, 3_700_000, 3_500_000, &mut now);
            assert!(calls <= 2);
            seen += at;
        }
        assert!(
            seen / 40 < 4_500_000,
            "a job is seen {} us after its submit",
            seen / 40 / 1000
        );
        assert!(pace.wake_ns() > 3_000_000, "{}", pace.wake_ns());
    }

    #[test]
    fn a_stale_high_expectation_finds_a_job_that_fits_the_call() {
        // NVENC's H.264 expected at 5.8 ms with an engine at 2.3 ms that
        // lands within the submitting call: once the expectation came down,
        // each job takes one call and is seen as it lands.
        let mut pace = Pace::new(5_800_000);
        let mut now = 0;
        for _ in 0..30 {
            job(&mut pace, 2_300_000, 1_300_000, 3_500_000, &mut now);
        }
        for _ in 0..10 {
            let (at, calls) = job(&mut pace, 2_300_000, 1_300_000, 3_500_000, &mut now);
            assert_eq!(calls, 1);
            assert!(at < 2_400_000, "{at}");
        }
        assert!(
            pace.engine_ns().abs_diff(2_300_000) < 100_000,
            "{}",
            pace.engine_ns()
        );
    }

    #[test]
    fn a_job_past_the_budget_is_seen_close_to_its_landing() {
        // NVDEC's 4K Main 10 at about 5 ms, expected 4.8 ms at first, with
        // the frontend 3 ms late: two calls a job, and the second one sees
        // it land within a few looks.
        let mut pace = Pace::new(4_800_000);
        let mut now = 0;
        let mut worst = 0;
        for k in 0..60 {
            let (seen, calls) = job(&mut pace, 5_000_000, 700_000, 3_000_000, &mut now);
            if k >= 20 {
                assert!(calls <= 2, "job {k}: {calls} calls");
                worst = worst.max(seen);
            }
        }
        assert!(
            worst < 5_800_000,
            "a job seen {} us after its submit",
            worst / 1000
        );
        let expected = pace.engine_ns();
        assert!((4_000_000..=5_200_000).contains(&expected), "{expected}");
    }

    #[test]
    fn a_slow_engine_raises_the_expectation_past_the_last_miss() {
        let mut pace = Pace::new(2_000_000);
        assert_eq!(pace.done(0, 6_000_000, 9_000_000), None);
        assert_eq!(pace.engine_ns(), 7_500_000);
        assert_eq!(pace.done(0, 8_950_000, 9_000_000), Some(9_000_000));
        assert_eq!(pace.engine_ns(), 7_875_000);
        // A hit before the expectation brings it down to the hit.
        assert_eq!(pace.done(0, 0, 3_000_000), None);
        assert_eq!(pace.engine_ns(), 3_000_000);
        // Between the miss and the hit: an eighth less.
        assert_eq!(pace.done(0, 1_000_000, 8_000_000), None);
        assert_eq!(pace.engine_ns(), 2_625_000);
    }

    #[test]
    fn the_wait_leaves_out_the_frontends_wake_up() {
        let mut pace = Pace::new(6_000_000);
        // Before any call came back: the prior wake-up.
        assert_eq!(pace.retry_us(0, 0), 5_000);
        pace.arrived(5_000_000 + 3_000_000);
        assert_eq!(pace.wake_ns(), 1_500_000);
        assert_eq!(pace.retry_us(1_000_000, 0), 3_500);
        // Late, and the bounds.
        assert_eq!(pace.retry_us(7_000_000, 0), RETRY_LATE_US);
        assert_eq!(pace.retry_us(5_900_000, 0), RETRY_LEAST_US as u32);
        // A wait that is no sleep measures nothing.
        pace.asked(0, RETRY_AT_ONCE_US);
        pace.arrived(9_000_000);
        assert_eq!(pace.wake_ns(), 1_500_000);
        // A call long after: counted up to the most.
        pace.asked(0, 1_000);
        pace.arrived(1_000_000 + 100_000_000);
        assert_eq!(pace.wake_ns(), (3 * 1_500_000 + WAKE_MOST_NS) / 4);
    }
}
