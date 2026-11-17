//! How the guest's pointer follows the host's in real time: the rate of the
//! display's presents that moved a layer (a pointer moves its layer whole),
//! and the time from a host pointer report to the first present of a swap
//! after it, both on the host's clock. The display paces itself on the
//! guest's counter, so its own rate (`DCP PACING`) is per guest second.
//!
//! The service notes the host read of the first pointer report a present
//! has not yet answered ([`motion`]); the boot PE looks at the display's
//! count of presents after each slice ([`Cursor::look`]), so a latency is
//! late by one slice at most. Any present answers it, moved or not: macOS
//! may draw its pointer into the frame rather than move a layer of its own,
//! and then no present ever moves one (`moved=0` in every `DCP PACING`
//! line), which left every window without a sample. A motion no present
//! answered within [`STALE_MILLIS`] is counted apart (`stale=`), not as a
//! sample: nothing on screen followed it.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};
use stela_soc::dcp::iomfb::{moved_presents, presents};

/// When the host read the oldest pointer report no present followed yet,
/// in TSC ticks; zero for none.
static MOTION_AT: AtomicU64 = AtomicU64::new(0);

/// How long after a motion a present still answers it.
const STALE_MILLIS: u64 = 1000;

/// The host read a pointer report at TSC `now`.
pub(super) fn motion(now: u64) {
    let _ = MOTION_AT.compare_exchange(0, now.max(1), Ordering::Relaxed, Ordering::Relaxed);
}

/// The presents and latencies since the last report.
pub(super) struct Cursor {
    seen_moved: u64,
    seen: u64,
    since: u64,
    moved: u64,
    presented: u64,
    samples: u64,
    stale: u64,
    total: u64,
    longest: u64,
}

impl Cursor {
    pub(super) fn new(now: u64) -> Self {
        Self {
            seen_moved: moved_presents(),
            seen: presents(),
            since: now,
            moved: 0,
            presented: 0,
            samples: 0,
            stale: 0,
            total: 0,
            longest: 0,
        }
    }

    /// Note the presents since the last look, at TSC `now`, on a clock of
    /// `tsc_hz` ticks a second.
    pub(super) fn look(&mut self, now: u64, tsc_hz: u64) {
        let moved = moved_presents();
        self.moved += moved.wrapping_sub(self.seen_moved);
        self.seen_moved = moved;
        let seen = presents();
        if seen == self.seen {
            return;
        }
        self.presented += seen.wrapping_sub(self.seen);
        self.seen = seen;
        let at = MOTION_AT.swap(0, Ordering::Relaxed);
        if at != 0 {
            let latency = now.saturating_sub(at);
            if latency >= tsc_hz / 1000 * STALE_MILLIS {
                self.stale += 1;
                return;
            }
            self.samples += 1;
            self.total += latency;
            self.longest = self.longest.max(latency);
        }
    }

    /// The window's line, and a new window from TSC `now`.
    pub(super) fn report(&mut self, now: u64, tsc_hz: u64) -> String {
        let tsc_hz = u128::from(tsc_hz.max(1));
        let millis = |ticks: u64| u128::from(ticks) * 1000 / tsc_hz;
        let wall = u128::from(now.saturating_sub(self.since).max(1));
        let per_ten = |count: u64| u128::from(count) * 10 * tsc_hz / wall;
        let (moved, presented) = (per_ten(self.moved), per_ten(self.presented));
        let pending = MOTION_AT.load(Ordering::Relaxed);
        let line = alloc::format!(
            "CURSOR moved_per_wall_second={}.{} presents_per_wall_second={}.{} \
             input_to_present_ms={}/{} samples={} stale={} unanswered_ms={}\n",
            moved / 10,
            moved % 10,
            presented / 10,
            presented % 10,
            millis(self.total / self.samples.max(1)),
            millis(self.longest),
            self.samples,
            self.stale,
            if pending == 0 {
                0
            } else {
                millis(now.saturating_sub(pending))
            },
        );
        *self = Self {
            since: now,
            moved: 0,
            presented: 0,
            samples: 0,
            stale: 0,
            total: 0,
            longest: 0,
            ..*self
        };
        line
    }
}
