//! The board's block table ([`LookupTable`]) behind a lock of its own.
//!
//! The table was the board's code lock's: a fetch's insert of a block's
//! entry took that lock to check the block was still indexed and to write
//! the entry, and those inserts (millions a beat during a build, about a
//! microsecond each) held it most of the time, so other PEs' installs and
//! links waited and their own inserts found it held and were skipped.
//!
//! Now every access takes this lock alone ([`Targets::lock`], or one of the
//! forwarding methods for a single call). A code lock holder that changes
//! the table takes it inside the code lock (code lock first, never the
//! other way). A fetch's insert takes only this lock, and stands for the
//! index check with [`super::Code::departures`]: every path that takes
//! blocks out of the index advances it under the code lock after the index
//! changed and before it forgets their table entries, so an insert that
//! finds it unchanged under this lock either precedes those forgets (which
//! take this lock after it and remove the entry) or follows none.
use super::{Mutex, MutexGuard, timestamp};
use alloc::vec::Vec;
use stela_compiler::x86_64::chain::{CellLink, LookupEntry, LookupTable};

/// How the table's lock was taken, and the entries fetches put in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableLockCounts {
    pub acquired: u64,
    /// Acquisitions that found it held, and the TSC ticks they waited.
    pub contended: u64,
    pub waited: u64,
    /// TSC ticks it was held, and the longest hold since the last report.
    pub held: u64,
    pub longest_held: u64,
    /// Fetches' entries the table took, and those refused because the
    /// block left the index meanwhile ([`super::TargetCounts`]).
    pub inserted: u64,
    pub raced: u64,
}

struct State {
    table: LookupTable,
    counts: TableLockCounts,
}

/// The block table and its lock.
pub(in super::super) struct Targets {
    state: Mutex<State>,
}

/// The table, locked: its hold is counted when it drops.
pub(in super::super) struct TargetsGuard<'a> {
    guard: MutexGuard<'a, State>,
    since: u64,
}

impl core::ops::Deref for TargetsGuard<'_> {
    type Target = LookupTable;
    fn deref(&self) -> &LookupTable {
        &self.guard.table
    }
}

impl core::ops::DerefMut for TargetsGuard<'_> {
    fn deref_mut(&mut self) -> &mut LookupTable {
        &mut self.guard.table
    }
}

impl TargetsGuard<'_> {
    /// The lock's counts so far, this hold's acquisition among them.
    #[cfg(test)]
    pub(in super::super) fn guard_counts(&self) -> TableLockCounts {
        self.guard.counts
    }

    /// Count a fetch's entry the table took, or refused.
    pub(in super::super) fn count_insert(&mut self, inserted: bool) {
        let counts = &mut self.guard.counts;
        if inserted {
            counts.inserted += 1;
        } else {
            counts.raced += 1;
        }
    }
}

impl Drop for TargetsGuard<'_> {
    fn drop(&mut self) {
        let held = timestamp().wrapping_sub(self.since);
        let counts = &mut self.guard.counts;
        counts.held += held;
        counts.longest_held = counts.longest_held.max(held);
    }
}

impl Targets {
    pub(in super::super) fn new(table: LookupTable) -> Self {
        Self {
            state: Mutex::new(State {
                table,
                counts: TableLockCounts::default(),
            }),
        }
    }

    /// The table, for the caller alone until the guard drops: for a
    /// sequence of calls no other writer may come between.
    pub(in super::super) fn lock(&self) -> TargetsGuard<'_> {
        if let Ok(mut guard) = self.state.try_lock() {
            guard.counts.acquired += 1;
            return TargetsGuard {
                guard,
                since: timestamp(),
            };
        }
        let start = timestamp();
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let since = timestamp();
        let counts = &mut guard.counts;
        counts.acquired += 1;
        counts.contended += 1;
        counts.waited += since.wrapping_sub(start);
        TargetsGuard { guard, since }
    }

    /// The lock's counts so far; the longest hold starts over.
    pub(in super::super) fn take_counts(&self) -> TableLockCounts {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let counts = state.counts;
        state.counts.longest_held = 0;
        counts
    }

    // One call each, under the lock for that call alone.

    #[cfg(test)]
    pub(in super::super) fn slot(&self, pc: u64, key: u64) -> usize {
        self.lock().slot(pc, key)
    }
    #[cfg(test)]
    pub(in super::super) fn find(&self, slot: usize, pc: u64, key: u64) -> LookupEntry {
        self.lock().find(slot, pc, key)
    }
    #[cfg(test)]
    pub(in super::super) fn entry(&self, slot: usize) -> LookupEntry {
        self.lock().entry(slot)
    }
    pub(in super::super) fn index_bytes(&self) -> usize {
        self.lock().index_bytes()
    }
    #[cfg(test)]
    pub(in super::super) fn set(&self, slot: usize, entry: LookupEntry) {
        self.lock().set(slot, entry)
    }
    pub(in super::super) fn clear(&self) {
        self.lock().clear()
    }
    pub(in super::super) fn forget_frames(
        &self,
        frames: core::ops::RangeInclusive<u64>,
        forget: impl FnMut(&LookupEntry) -> bool,
    ) -> bool {
        self.lock().forget_frames(frames, forget)
    }
    pub(in super::super) fn forget_block(&self, pc: u64, key: u64, block: u32) -> usize {
        self.lock().forget_block(pc, key, block)
    }
    #[cfg(test)]
    pub(in super::super) fn link_cell(
        &self,
        site: u64,
        pc: u64,
        current: Option<LookupEntry>,
    ) -> u64 {
        self.lock().link_cell(site, pc, current)
    }
    pub(in super::super) fn release_cells(&self, sites: core::ops::RangeInclusive<u64>) {
        self.lock().release_cells(sites)
    }
    pub(in super::super) fn release_some_cells(
        &self,
        sites: core::ops::RangeInclusive<u64>,
        most: usize,
    ) -> Option<u64> {
        self.lock().release_some_cells(sites, most)
    }
    #[cfg(test)]
    pub(in super::super) fn site_cell(&self, site: u64) -> Option<LookupEntry> {
        self.lock().site_cell(site)
    }
    pub(in super::super) fn site_cell_number(&self, site: u64) -> Option<u32> {
        self.lock().site_cell_number(site)
    }
    #[cfg(test)]
    pub(in super::super) fn cells(&self) -> usize {
        self.lock().cells()
    }
    pub(in super::super) fn stamped_cells(&self) -> u64 {
        self.lock().stamped_cells()
    }
    pub(in super::super) fn new_stamps(&self) -> u64 {
        self.lock().new_stamps()
    }
    pub(in super::super) fn holds_stamps(&self, stamps: u64) -> bool {
        self.lock().holds_stamps(stamps)
    }
    pub(in super::super) fn settle_heat(&self, cell: u32) {
        self.lock().settle_heat(cell)
    }
    pub(in super::super) fn note_stamped(&self, cell: u32) {
        self.lock().note_stamped(cell)
    }
    pub(in super::super) fn stamped_tails(&self) -> usize {
        self.lock().stamped_tails()
    }
    pub(in super::super) fn hot_links(&self, cells: core::ops::Range<u32>) -> Vec<CellLink> {
        self.lock().hot_links(cells)
    }
    pub(in super::super) fn cell_heat(&self, cell: u32) -> u32 {
        self.lock().cell_heat(cell)
    }
}
