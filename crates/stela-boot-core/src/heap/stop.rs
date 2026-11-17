//! The heap at a stop. A stop can begin anywhere: in an NMI or a fault that
//! interrupted its processor inside the heap, holding the lock or halfway
//! through the processor's own cache, or while another processor that
//! never runs again holds the lock. Its report allocates all the same: its
//! text, the screen, the log's store. A lock that spins without end would
//! then hang the stop with nothing on the screen.
//!
//! So once a processor says it stops ([`Heap::stop_on`]), its allocations
//! skip its cache, try the lock [`SPINS`] times at most, and take from a
//! reserve carved while the boot ran ([`Heap::keep_stop_reserve`]) when the
//! lock stays held; once the lock was held that long, one try each. Its
//! frees of reserve blocks do nothing, and those of heap blocks the lock
//! refuses are dropped and counted. Diagnostics that read the heap under
//! its lock answer from the counts the lock last left. Processors that do
//! not stop are unchanged, and a heap where no stop began costs one load
//! per call.
use super::{
    Guard, Heap, LARGE_BUCKETS, Small, Usage, aligned, allocate_locked, class, release,
};
use core::{
    alloc::Layout,
    ptr,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

/// The stopping processors a stop names one by one; with more, every
/// processor's calls are a stop's.
const PROCESSORS: usize = 16;
const NONE: u32 = u32::MAX;
/// Tries of the lock a stopping processor makes before it takes from the
/// reserve: some milliseconds, far more than any holder that still runs
/// keeps it.
pub(super) const SPINS: u32 = 1 << 16;

pub(super) struct Stop {
    began: AtomicBool,
    /// The `fn() -> u32` that names the calling processor; 0 before a stop.
    identity: AtomicUsize,
    processors: [AtomicU32; PROCESSORS],
    /// More processors stopped than `processors` holds.
    every: AtomicBool,
    /// The reserve, and its next free byte; `end` 0 without one.
    start: AtomicUsize,
    end: AtomicUsize,
    next: AtomicUsize,
    /// A stopping processor found the lock held for all its tries.
    lock_lost: AtomicBool,
    /// Allocations the reserve served, and frees the stop dropped.
    reserved: AtomicU64,
    dropped: AtomicU64,
}

impl Stop {
    pub(super) const fn new() -> Self {
        Self {
            began: AtomicBool::new(false),
            identity: AtomicUsize::new(0),
            processors: [const { AtomicU32::new(NONE) }; PROCESSORS],
            every: AtomicBool::new(false),
            start: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
            lock_lost: AtomicBool::new(false),
            reserved: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        }
    }

    fn identity(&self) -> Option<u32> {
        let identity = self.identity.load(Ordering::Acquire);
        // SAFETY: only `stop_on` stores a nonzero value, a `fn() -> u32`.
        (identity != 0).then(|| unsafe { core::mem::transmute::<usize, fn() -> u32>(identity) }())
    }
}

/// What a stop took from the heap ([`Heap::stop_counts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StopCounts {
    /// Allocations the reserve served, as the lock was held.
    pub reserved: u64,
    /// Bytes of the reserve taken, and its size.
    pub reserve_used: usize,
    pub reserve_bytes: usize,
    /// Frees dropped, as the lock was held.
    pub dropped: u64,
}

impl Heap {
    /// From now on the calling processor stops: its allocations never wait
    /// for the lock without end ([`self`](super::stop)). `identity` names
    /// the calling processor, the same on every call there and different
    /// on every other; the first stop's is kept. Allocates nothing and
    /// takes no lock, so any stop path may call it first, an NMI's too.
    pub fn stop_on(&self, identity: fn() -> u32) {
        let stop = &self.stop;
        let _ = stop.identity.compare_exchange(
            0,
            identity as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let Some(me) = stop.identity() else {
            return;
        };
        let named = stop.processors.iter().any(|slot| {
            slot.load(Ordering::Relaxed) == me
                || slot
                    .compare_exchange(NONE, me, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
        });
        if !named {
            stop.every.store(true, Ordering::Release);
        }
        stop.began.store(true, Ordering::Release);
    }

    /// Carve the reserve a stop allocates from while the lock is held,
    /// `bytes` long, once; whether the heap had it. Call while the boot
    /// runs, once the heap is set up.
    pub fn keep_stop_reserve(&self, bytes: usize) -> bool {
        let stop = &self.stop;
        if stop.end.load(Ordering::Acquire) != 0 {
            return true;
        }
        let Ok(layout) = Layout::from_size_align(bytes.max(1), super::PAGE_BYTES) else {
            return false;
        };
        // SAFETY: a nonzero layout; the block is the stop's for the boot.
        let block = unsafe { core::alloc::GlobalAlloc::alloc(self, layout) };
        if block.is_null() {
            return false;
        }
        stop.start.store(block as usize, Ordering::Relaxed);
        stop.next.store(block as usize, Ordering::Relaxed);
        stop.end
            .store(block as usize + layout.size(), Ordering::Release);
        true
    }

    /// What stops took from the heap so far.
    pub fn stop_counts(&self) -> StopCounts {
        let stop = &self.stop;
        let start = stop.start.load(Ordering::Relaxed);
        StopCounts {
            reserved: stop.reserved.load(Ordering::Relaxed),
            reserve_used: stop.next.load(Ordering::Relaxed) - start,
            reserve_bytes: stop.end.load(Ordering::Relaxed).saturating_sub(start),
            dropped: stop.dropped.load(Ordering::Relaxed),
        }
    }

    /// Whether the calling processor stops: one load while no stop began.
    pub(super) fn stopping(&self) -> bool {
        let stop = &self.stop;
        if !stop.began.load(Ordering::Acquire) {
            return false;
        }
        if stop.every.load(Ordering::Relaxed) {
            return true;
        }
        let Some(me) = stop.identity() else {
            return false;
        };
        stop.processors
            .iter()
            .any(|slot| slot.load(Ordering::Relaxed) == me)
    }

    /// The lock for a stopping processor, after [`SPINS`] tries at most.
    pub(super) fn stop_lock(&self) -> Option<Guard<'_>> {
        let tries = match self.stop.lock_lost.load(Ordering::Relaxed) {
            true => 1,
            false => SPINS,
        };
        for _ in 0..tries {
            if !self.held.load(Ordering::Relaxed)
                && let Some(guard) = self.try_lock()
            {
                return Some(guard);
            }
            core::hint::spin_loop();
        }
        self.stop.lock_lost.store(true, Ordering::Relaxed);
        None
    }

    /// The lock: waited for as ever, or as a stop does
    /// ([`Self::stop_lock`]) on a stopping processor.
    pub(super) fn lock_or_stop(&self) -> Option<Guard<'_>> {
        match self.stopping() {
            true => self.stop_lock(),
            false => Some(self.lock()),
        }
    }

    pub(super) fn in_reserve(&self, pointer: *mut u8) -> bool {
        let at = pointer as usize;
        let stop = &self.stop;
        stop.end.load(Ordering::Acquire) > at && at >= stop.start.load(Ordering::Relaxed)
    }

    fn take_reserve(&self, layout: Layout) -> *mut u8 {
        let stop = &self.stop;
        let end = stop.end.load(Ordering::Acquire);
        let mut next = stop.next.load(Ordering::Relaxed);
        loop {
            let Some(after) = aligned(next, layout.align())
                .and_then(|at| Some((at, at.checked_add(layout.size().max(1))?)))
                .filter(|&(_, after)| after <= end)
            else {
                return ptr::null_mut();
            };
            match stop.next.compare_exchange_weak(
                next,
                after.1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    stop.reserved.fetch_add(1, Ordering::Relaxed);
                    return after.0 as *mut u8;
                }
                Err(now) => next = now,
            }
        }
    }

    /// A stopping processor's allocation: from the heap under its lock,
    /// else from the reserve; null when neither has room.
    ///
    /// # Safety
    /// As `GlobalAlloc::alloc`.
    pub(super) unsafe fn stop_alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(guard) = self.stop_lock() {
            // SAFETY: the guard owns the state; class matches the layout.
            let block = unsafe { allocate_locked(&mut *self.state.get(), layout, class(layout)) };
            drop(guard);
            if !block.is_null() {
                return block;
            }
        }
        self.take_reserve(layout)
    }

    /// A stopping processor's free: nothing for a reserve block; a heap
    /// block goes back under the lock, or is dropped where the lock stays
    /// held. A size-class block goes to its class's free list, never to
    /// this processor's cache, which the stop may have interrupted.
    ///
    /// # Safety
    /// As `GlobalAlloc::dealloc`.
    pub(super) unsafe fn stop_dealloc(&self, pointer: *mut u8, layout: Layout) {
        if self.in_reserve(pointer) {
            return;
        }
        let Some(_guard) = self.stop_lock() else {
            self.stop.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // SAFETY: the guard owns the state until this borrow ends.
        let state = unsafe { &mut *self.state.get() };
        // A block a core's cache handed out is not in `state.live`.
        if let Some(class) = class(layout) {
            state.live = state.live.saturating_sub(super::SMALLEST << class);
            let block = pointer.cast::<Small>();
            // SAFETY: the caller's block of this class, at least 16 bytes
            // and 16-byte aligned, which it no longer accesses.
            unsafe {
                block.write(Small {
                    next: state.small[class],
                })
            };
            state.small[class] = block;
        } else {
            state.live = state.live.saturating_sub(layout.size());
            state.count_large(layout.size(), false);
            // SAFETY: the caller's live free-list block; the lock is held.
            unsafe { release(state, pointer) };
        }
    }

    /// A stopping processor's resize: a size-class block that keeps its
    /// class stays; any other moves to a new allocation.
    ///
    /// # Safety
    /// As `GlobalAlloc::realloc`.
    pub(super) unsafe fn stop_realloc(
        &self,
        pointer: *mut u8,
        layout: Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: GlobalAlloc's realloc contract.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let old_class = class(layout);
        if !self.in_reserve(pointer) && old_class.is_some() && old_class == class(new_layout) {
            return pointer;
        }
        // SAFETY: as `GlobalAlloc::realloc`: a new block is disjoint, and
        // only then is the old one freed.
        unsafe {
            let next = self.stop_alloc(new_layout);
            if !next.is_null() {
                ptr::copy_nonoverlapping(pointer, next, layout.size().min(new_size));
                self.stop_dealloc(pointer, layout);
            }
            next
        }
    }

    /// The usage the lock last left, for a stopping processor that cannot
    /// have the lock: the bin walk's fields are zero.
    pub(super) fn usage_unlocked(&self) -> Usage {
        let total = self.total.load(Ordering::Relaxed);
        Usage {
            total,
            live: self.live(),
            carved: total.saturating_sub(self.room.load(Ordering::Relaxed)),
            peak: 0,
            largest: 0,
            extents: 0,
            carves: 0,
            large: [(0, 0); LARGE_BUCKETS],
        }
    }
}
