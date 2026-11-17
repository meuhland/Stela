//! A call a guest processor posts for the boot processor's Linux tasks to
//! perform, and the bounds of the poster's wait.
//!
//! A guest processor spinning in one exit for longer than about half a
//! second makes XNU panic. A posted call no task has taken by
//! [`WITHDRAW_MS`] is withdrawn: its poster answers the caller
//! `ETIMEDOUT` and reports it, and the task that meets the call later frees
//! it unperformed. A call a task already performs cannot be withdrawn, as
//! the ioctl writes the caller's memory: its poster reports it and waits on,
//! and a call still running at [`STOP_MS`] ends in a named stop rather than
//! a processor spinning forever ([`crate::bounded_spin`]).
use core::sync::atomic::{AtomicU32, Ordering};

/// How long a posted call may wait for a task to take it.
pub const WITHDRAW_MS: u64 = 300;
/// How long a posted call a task performs may run before the boot stops.
pub const STOP_MS: u64 = 10_000;
/// The longest a posted DRM fence wait may wait: the bound of the GPU
/// backend's own idle waits. A wait asking for longer, as Mesa's
/// `vkQueueWaitIdle` does with no timeout, ends at this bound with
/// `ETIME`, its caller's answer for a wait that timed out. Linux sleeps in
/// such a wait until the fence signals, which on a channel that hangs
/// without a channel error is when the scheduler's 10 s job timeout kills
/// the channel; that is [`STOP_MS`] after the post at the earliest, so a
/// guest processor spinning on it would always reach the stop first.
pub const WAIT_MS: u64 = 1_000;

/// `DRM_IOCTL_SYNCOBJ_WAIT`: `struct drm_syncobj_wait`, 40 bytes.
pub const SYNCOBJ_WAIT: u32 = 0xc028_64c3;
/// `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`: `struct drm_syncobj_timeline_wait`,
/// 48 bytes.
pub const SYNCOBJ_TIMELINE_WAIT: u32 = 0xc030_64ca;

/// The byte offset of the absolute `timeout_nsec` (CLOCK_MONOTONIC, signed)
/// in the argument of the fence wait `request`, None for other requests.
pub const fn wait_timeout_offset(request: u32) -> Option<usize> {
    match request {
        // handles, then timeout_nsec.
        SYNCOBJ_WAIT => Some(8),
        // handles, points, then timeout_nsec.
        SYNCOBJ_TIMELINE_WAIT => Some(16),
        _ => None,
    }
}

/// The deadline a posted fence wait asking for `timeout` (absolute, in the
/// monotonic clock that reads `now` nanoseconds) runs to instead: None when
/// it ends within [`WAIT_MS`] already, polls, or has passed.
pub const fn bounded_wait_deadline(timeout: i64, now: u64) -> Option<i64> {
    let bound = now.saturating_add(WAIT_MS * 1_000_000);
    let bound = if bound > i64::MAX as u64 {
        i64::MAX
    } else {
        bound as i64
    };
    if timeout > bound { Some(bound) } else { None }
}

const POSTED: u32 = 0;
const TAKEN: u32 = 1;
const WITHDRAWN: u32 = 2;

/// Who has a posted call: its poster, a task that performs it, or nobody
/// once the poster withdrew it. Exactly one of [`Self::take`] and
/// [`Self::withdraw`] succeeds.
#[derive(Debug, Default)]
pub struct Claim(AtomicU32);

impl Claim {
    pub const fn new() -> Self {
        Self(AtomicU32::new(POSTED))
    }

    /// A task takes the call to perform it. False when its poster withdrew
    /// it: the poster no longer waits, and the task frees the call's storage
    /// without performing it.
    pub fn take(&self) -> bool {
        match self
            .0
            .compare_exchange(POSTED, TAKEN, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state == TAKEN,
        }
    }

    /// The poster withdraws the call, its wait at [`WITHDRAW_MS`]. True when
    /// no task took it: the poster answers its caller without it and leaves
    /// the call's storage to the task that meets it. False when a task
    /// performs it: the poster waits for its result.
    pub fn withdraw(&self) -> bool {
        match self
            .0
            .compare_exchange(POSTED, WITHDRAWN, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => true,
            Err(state) => state == WITHDRAWN,
        }
    }

    /// Whether the poster withdrew the call.
    pub fn withdrawn(&self) -> bool {
        self.0.load(Ordering::Acquire) == WITHDRAWN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_taken_first_cannot_be_withdrawn() {
        let claim = Claim::new();
        assert!(claim.take());
        assert!(!claim.withdraw());
        assert!(!claim.withdrawn());
        assert!(claim.take(), "taking again changes nothing");
    }

    #[test]
    fn a_call_withdrawn_first_is_never_performed() {
        let claim = Claim::new();
        assert!(claim.withdraw());
        assert!(claim.withdrawn());
        assert!(!claim.take());
        assert!(claim.withdraw(), "withdrawing again changes nothing");
    }

    #[test]
    fn the_bounds_stay_inside_xnus_half_second_and_stop_after_it() {
        const _: () = assert!(WITHDRAW_MS < 500 && STOP_MS > 500);
    }

    #[test]
    fn a_fence_wait_ends_before_the_stop() {
        const _: () = assert!(WITHDRAW_MS + WAIT_MS < STOP_MS);
    }

    #[test]
    fn a_fence_wait_without_a_timeout_ends_at_the_bound() {
        let now = 5_000_000_000;
        let bound = (now + WAIT_MS * 1_000_000) as i64;
        // Mesa's vkQueueWaitIdle: UINT64_MAX, i64::MAX once converted.
        assert_eq!(bounded_wait_deadline(i64::MAX, now), Some(bound));
        assert_eq!(bounded_wait_deadline(bound + 1, now), Some(bound));
        assert_eq!(bounded_wait_deadline(bound, now), None);
        assert_eq!(bounded_wait_deadline(now as i64 + 1, now), None);
        // A poll or a passed deadline stays as it is.
        assert_eq!(bounded_wait_deadline(0, now), None);
        assert_eq!(bounded_wait_deadline(-1, now), None);
        assert_eq!(bounded_wait_deadline(i64::MAX, u64::MAX), None);
    }

    #[test]
    fn only_fence_waits_have_a_timeout() {
        assert_eq!(wait_timeout_offset(SYNCOBJ_WAIT), Some(8));
        assert_eq!(wait_timeout_offset(SYNCOBJ_TIMELINE_WAIT), Some(16));
        // DRM_IOCTL_SYNCOBJ_DESTROY and NOUVEAU_GEM_NEW.
        assert_eq!(wait_timeout_offset(0xc008_64c0), None);
        assert_eq!(wait_timeout_offset(0xc030_6480), None);
    }
}
