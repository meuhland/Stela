//! The installed blocks by physical page: a page's slots sorted by a tag of
//! the guest page they run at, then page offset, whose blocks hold the rest
//! of each key. A frame mapped at other PCs (each process's own slide of
//! the same binary; a build's compiler has some twenty at once) keeps each
//! copy's blocks in a run of its own: a lookup loads only its own copy's
//! blocks, and the walks of a retirement or detach, which a link from
//! another guest page can never concern ([`super::board::Record::chained`]
//! links stay in their page), visit only the runs of the pages they are
//! for ([`BlockIndex::page_tags_part`]). A block costs a few bytes here,
//! and forgetting a page forgets its blocks at once. Each fetch a lookup
//! misses comes here, so a direct-mapped cache of pages answers most
//! without walking the ordered map, which sweeps keep their place in.
//!
//! The index changes under the board's code lock; a [`Finder`] beside it
//! answers a fetch without that lock.
use super::{Block, FETCH_PAGE, VariantId, board::Records, same_offset_and_half};
use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec, vec::Vec};
use core::ops::Index;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const _: () = assert!(FETCH_PAGE <= 1 << u16::BITS);

/// Where [`BlockIndex::sweep`] goes on: past this entry of this page.
pub(super) type Cursor = (u64, usize);

/// The pages [`BlockIndex::front`] holds, a power of two.
const FRONT: usize = 1 << 12;
/// A frame no page has: pages are aligned.
const NO_FRAME: u64 = 1;
/// The entries a forgotten page's list keeps room for.
const KEPT_ENTRIES: usize = 16;

/// One block of a page: its offset, a tag of its guest page
/// ([`page_tag`]) and its slot. The tag takes the room alignment left, so
/// an entry is 8 bytes.
#[derive(Clone, Copy)]
struct Entry {
    offset: u16,
    tag: u16,
    slot: u32,
}

impl Entry {
    /// Runs by guest page, then by offset: a page's offset and its guest
    /// page give the PC, so a run's blocks of one offset differ only in
    /// variant (or in a tag both pages share).
    fn order(&self) -> (u16, u16) {
        (self.tag, self.offset)
    }
}

/// A 16-bit tag of the guest page of `pc`: copies of one frame at other
/// PCs mostly differ in it. Two pages of one tag only make a walk visit
/// more than it needs.
pub(super) fn page_tag(pc: u64) -> u16 {
    ((pc / FETCH_PAGE).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 48) as u16
}

/// The sorted, distinct tags of the guest pages of `pcs`.
pub(super) fn page_tags(pcs: impl IntoIterator<Item = u64>) -> Vec<u16> {
    let mut tags: Vec<u16> = pcs.into_iter().map(page_tag).collect();
    tags.sort_unstable();
    tags.dedup();
    tags
}

/// The runs of `entries` whose tags `tags` (sorted) names, as position
/// ranges in ascending order.
fn tag_runs(entries: &[Entry], tags: &[u16]) -> Vec<core::ops::Range<usize>> {
    let mut runs = Vec::with_capacity(tags.len());
    for &tag in tags {
        let start = entries.partition_point(|entry| entry.tag < tag);
        let end = start + entries[start..].partition_point(|entry| entry.tag == tag);
        if start < end {
            runs.push(start..end);
        }
    }
    runs
}

/// The PC a shared block ([`Block::shared`]) is listed by in the
/// [`Finder`]: one list per frame offset for every page that maps it. No
/// block's PC, which is a multiple of 4.
const SHARED_PC: u64 = 1;

/// The PC `block` is listed by in the [`Finder`].
fn finder_pc(block: &Block) -> u64 {
    if block.shared { SHARED_PC } else { block.pc }
}

/// The shared block among `entries` (a frame's list) at `offset` in
/// `variant` that serves `pc` ([`Block::serves`]), whatever guest page it
/// was installed for: each guest page's run at the offset.
fn shared_in(
    blocks: &impl Index<usize, Output = Block>,
    entries: &[Entry],
    pc: u64,
    offset: u16,
    variant: VariantId,
    exact: bool,
    runs: &mut u64,
) -> Option<usize> {
    let mut run = 0;
    while run < entries.len() {
        *runs += 1;
        let run_tag = entries[run].tag;
        let end = run + entries[run..].partition_point(|entry| entry.tag == run_tag);
        let part = &entries[run..end];
        let start = part.partition_point(|entry| entry.offset < offset);
        let found = part[start..]
            .iter()
            .take_while(|entry| entry.offset == offset)
            .map(|entry| entry.slot as usize)
            .find(|&slot| {
                let block = &blocks[slot];
                block.shared
                    && block.variant == variant
                    && same_offset_and_half(block.pc, pc)
                    && (!exact || block.pc == pc)
            });
        if found.is_some() {
            return found;
        }
        run = end;
    }
    None
}

/// [`shared_in`] through a frame's list of its shared blocks alone
/// (`shared`, by ascending offset, [`BlockIndex::shared`]): one search at
/// the offset, whatever copies of the frame at other guest pages its
/// whole list holds. A block of another frame is never the answer, even
/// from an entry the list kept by mistake.
fn shared_at(
    blocks: &impl Index<usize, Output = Block>,
    shared: &[Entry],
    frame: u64,
    pc: u64,
    offset: u16,
    variant: VariantId,
    exact: bool,
) -> Option<usize> {
    let start = shared.partition_point(|entry| entry.offset < offset);
    shared[start..]
        .iter()
        .take_while(|entry| entry.offset == offset)
        .map(|entry| entry.slot as usize)
        .find(|&slot| {
            let block = &blocks[slot];
            block.shared
                && split(block.physical) == (frame, offset)
                && block.variant == variant
                && same_offset_and_half(block.pc, pc)
                && (!exact || block.pc == pc)
        })
}

/// Put `entry`, a shared block's, in `shared` in (offset, tag) order.
fn list_shared(shared: &mut Vec<Entry>, entry: Entry) {
    let at =
        shared.partition_point(|listed| (listed.offset, listed.tag) <= (entry.offset, entry.tag));
    shared.insert(at, entry);
}

/// The finder's hash of a block's physical address and PC.
fn key_hash(physical: u64, pc: u64) -> u64 {
    let hash = (physical >> 2).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (pc >> 2).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    hash ^ hash >> 29
}

pub(super) struct BlockIndex {
    /// The same blocks, for reads without the lock.
    finder: Arc<Finder>,
    /// Each physical page's [`Self::entries`].
    pages: BTreeMap<u64, u32>,
    /// A page's blocks, by ascending offset.
    entries: Vec<Vec<Entry>>,
    /// Each page's shared blocks ([`Block::shared`]) alone, by ascending
    /// offset then tag, beside [`Self::entries`]: what a lookup of a shared
    /// block searches with [`Self::offsets`], where a walk of every run of
    /// the page's list visits each copy of the frame at another guest page
    /// (each launch of a slid binary leaves one), a load of a block for
    /// each under the board's lock.
    shared: Vec<Vec<Entry>>,
    /// Shared blocks are looked up in [`Self::shared`]
    /// (`--shared-offset-index`), not by a walk of every run.
    offsets: bool,
    /// Shared block lookups ([`Self::get`] with aliases, [`Self::get_shared`])
    /// that searched a page, and the runs they visited: one each through
    /// [`Self::shared`], one per copy of the frame they passed in a walk.
    pub(super) shared_lookups: [u64; 2],
    /// The [`Self::entries`] of forgotten pages.
    free: Vec<u32>,
    /// Recently found pages and their [`Self::entries`].
    front: Box<[(u64, u32)]>,
    /// Shared blocks serve the PC they were installed for alone
    /// (`--shared-own-pc`).
    exact: bool,
}

fn split(physical: u64) -> (u64, u16) {
    (
        physical & !(FETCH_PAGE - 1),
        (physical & (FETCH_PAGE - 1)) as u16,
    )
}

impl BlockIndex {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_finder(Arc::new(Finder::new(1 << 16)))
    }

    /// An empty index that keeps `finder` in step with it.
    pub(super) fn with_finder(finder: Arc<Finder>) -> Self {
        Self {
            finder,
            pages: BTreeMap::new(),
            entries: Vec::new(),
            shared: Vec::new(),
            offsets: false,
            shared_lookups: [0; 2],
            free: Vec::new(),
            front: vec![(NO_FRAME, 0); FRONT].into_boxed_slice(),
            exact: false,
        }
    }

    /// Let shared blocks serve the PC they were installed for alone
    /// (`--shared-own-pc`), the finder's too.
    pub(super) fn set_exact(&mut self, exact: bool) {
        self.exact = exact;
        self.finder.exact.store(exact, Ordering::Relaxed);
    }

    /// Look shared blocks up in each page's list of them
    /// (`--shared-offset-index`, [`Self::shared`]), which the index keeps
    /// either way.
    pub(super) fn set_offsets(&mut self, offsets: bool) {
        self.offsets = offsets;
    }

    /// The shared block at `offset` of the page at `frame` in `variant`
    /// that serves `pc`: through [`Self::shared`] with [`Self::offsets`],
    /// else a walk of every run of the page's list.
    fn shared_of(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        frame: u64,
        pc: u64,
        offset: u16,
        variant: VariantId,
    ) -> Option<usize> {
        let exact = self.exact;
        if self.offsets {
            let page = self.page_of(frame)?;
            self.shared_lookups[0] += 1;
            self.shared_lookups[1] += 1;
            return shared_at(
                blocks,
                &self.shared[page],
                frame,
                pc,
                offset,
                variant,
                exact,
            );
        }
        let page = self.page_of(frame)?;
        self.front[Self::front_slot(frame)] = (frame, page as u32);
        self.shared_lookups[0] += 1;
        let runs = &mut self.shared_lookups[1];
        shared_in(
            blocks,
            &self.entries[page],
            pc,
            offset,
            variant,
            exact,
            runs,
        )
    }

    /// The cache slot of the page at `frame`.
    fn front_slot(frame: u64) -> usize {
        (frame / FETCH_PAGE) as usize & (FRONT - 1)
    }

    /// The [`Self::entries`] of the page at `frame`.
    fn page_of(&self, frame: u64) -> Option<usize> {
        let (cached, entries) = self.front[Self::front_slot(frame)];
        if cached == frame {
            return Some(entries as usize);
        }
        self.pages.get(&frame).map(|&entries| entries as usize)
    }

    /// The entries of the page at `frame`, noted in the front cache.
    fn list(&mut self, frame: u64) -> Option<&[Entry]> {
        let page = self.page_of(frame)?;
        self.front[Self::front_slot(frame)] = (frame, page as u32);
        Some(&self.entries[page])
    }

    /// The block at `pc`, `physical` and `variant`: one installed for `pc`,
    /// else with `aliases`, a shared block of the frame offset installed
    /// for any page ([`Block::serves`]). Only the entries of the guest
    /// page's run at its offset load their blocks, but for the shared
    /// block's search, which visits each page's run at the offset.
    pub(super) fn get(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        pc: u64,
        physical: u64,
        variant: VariantId,
        aliases: bool,
    ) -> Option<usize> {
        let (frame, offset) = split(physical);
        let key = (page_tag(pc), offset);
        let entries = self.list(frame)?;
        let start = entries.partition_point(|entry| entry.order() < key);
        let own = entries[start..]
            .iter()
            .take_while(|entry| entry.order() == key)
            .map(|entry| entry.slot as usize)
            .find(|&slot| {
                let block = &blocks[slot];
                !block.shared && block.pc == pc && block.variant == variant
            });
        if own.is_some() || !aliases {
            return own;
        }
        self.shared_of(blocks, frame, pc, offset, variant)
    }

    /// The shared block at `physical` in `variant` that serves `pc`
    /// ([`Block::serves`]), not one installed for `pc` alone: what a
    /// shared block's link in its page may enter at every page it runs at.
    pub(super) fn get_shared(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        pc: u64,
        physical: u64,
        variant: VariantId,
    ) -> Option<usize> {
        let (frame, offset) = split(physical);
        self.shared_of(blocks, frame, pc, offset, variant)
    }

    /// Whether the block in `slot` is the one the index holds for its key.
    pub(super) fn contains(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        slot: usize,
    ) -> bool {
        let block = &blocks[slot];
        let (frame, offset) = split(block.physical);
        let key = (page_tag(block.pc), offset);
        let Some(entries) = self.list(frame) else {
            return false;
        };
        let start = entries.partition_point(|entry| entry.order() < key);
        entries[start..]
            .iter()
            .take_while(|entry| entry.order() == key)
            .any(|entry| entry.slot as usize == slot)
    }

    /// What else `physical`'s frame offset holds besides a block of `pc`
    /// in `variant`: a block of another PC (`Some(true)`, the frame mapped
    /// at another address), else one of `pc` in another variant
    /// (`Some(false)`), else nothing.
    pub(super) fn sharing(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        pc: u64,
        physical: u64,
        variant: VariantId,
    ) -> Option<bool> {
        let mut shared = None;
        let (frame, offset) = split(physical);
        let tag = page_tag(pc);
        let entries = self.list(frame)?;
        // Each guest page's run at the offset.
        let mut run = 0;
        while run < entries.len() {
            let run_tag = entries[run].tag;
            let end = run + entries[run..].partition_point(|entry| entry.tag == run_tag);
            let part = &entries[run..end];
            let start = part.partition_point(|entry| entry.offset < offset);
            for entry in part[start..]
                .iter()
                .take_while(|entry| entry.offset == offset)
            {
                // Another tag is another guest page, so another PC, without
                // loading its block.
                if entry.tag != tag {
                    return Some(true);
                }
                let other = &blocks[entry.slot as usize];
                if other.pc != pc {
                    return Some(true);
                }
                if other.variant != variant {
                    shared = Some(false);
                }
            }
            run = end;
        }
        shared
    }

    /// The blocks of the page holding `physical`.
    pub(super) fn page(&self, physical: u64) -> impl Iterator<Item = usize> + '_ {
        let (frame, _) = split(physical);
        BlockIndex::page_of(self, frame)
            .into_iter()
            .flat_map(|page| &self.entries[page])
            .map(|entry| entry.slot as usize)
    }

    /// The blocks of the page holding `physical` at positions `from..` of
    /// its list, at most `most`, and the list's length.
    pub(super) fn page_part(
        &self,
        physical: u64,
        from: usize,
        most: usize,
    ) -> (impl Iterator<Item = usize> + '_, usize) {
        let (frame, _) = split(physical);
        let entries: &[Entry] = self
            .page_of(frame)
            .map_or(&[], |page| self.entries[page].as_slice());
        let part = entries.get(from..).unwrap_or(&[]);
        let part = &part[..part.len().min(most)];
        (part.iter().map(|entry| entry.slot as usize), entries.len())
    }

    /// The blocks of the page holding `physical` that run at guest pages of
    /// the sorted `tags` ([`page_tags`]), at positions `from..` of their
    /// runs taken in turn, at most `most`, and the runs' length: what a
    /// walk for blocks of those pages visits, the frame's other copies
    /// left out.
    pub(super) fn page_tags_part(
        &self,
        physical: u64,
        tags: &[u16],
        from: usize,
        most: usize,
    ) -> (Vec<usize>, usize) {
        let (frame, _) = split(physical);
        let entries: &[Entry] = self
            .page_of(frame)
            .map_or(&[], |page| self.entries[page].as_slice());
        let runs = tag_runs(entries, tags);
        let listed = runs.iter().map(|run| run.len()).sum();
        let slots = runs
            .into_iter()
            .flat_map(|run| entries[run].iter())
            .skip(from)
            .take(most)
            .map(|entry| entry.slot as usize)
            .collect();
        (slots, listed)
    }

    /// Find the block in `slot` by its key, in place of any block of the
    /// key: the slot of the block it replaces.
    pub(super) fn insert(
        &mut self,
        blocks: &impl Index<usize, Output = Block>,
        slot: usize,
    ) -> Option<usize> {
        let block = &blocks[slot];
        let (frame, offset) = split(block.physical);
        let page = match self.page_of(frame) {
            Some(page) => page,
            None => {
                let page = self.free.pop().unwrap_or_else(|| {
                    self.entries.push(Vec::new());
                    self.shared.push(Vec::new());
                    self.entries.len() as u32 - 1
                });
                self.pages.insert(frame, page);
                self.finder.pages.store(self.pages.len(), Ordering::Relaxed);
                page as usize
            }
        };
        let entries = &mut self.entries[page];
        let tag = page_tag(block.pc);
        let start = entries.partition_point(|entry| entry.order() < (tag, offset));
        let same = entries[start..]
            .iter()
            .take_while(|entry| entry.order() == (tag, offset))
            .position(|entry| {
                let other = &blocks[entry.slot as usize];
                other.pc == block.pc
                    && other.variant == block.variant
                    && other.shared == block.shared
            });
        match same {
            Some(position) => {
                let replaced = core::mem::replace(&mut entries[start + position].slot, slot as u32);
                if replaced as usize == slot {
                    return None;
                }
                // Whatever either block's `shared`: the replaced slot's
                // entry goes, and the new block's comes if it is shared.
                let shared = &mut self.shared[page];
                shared.retain(|entry| entry.slot != replaced);
                if block.shared {
                    list_shared(shared, entries[start + position]);
                }
                self.finder.unlink(replaced);
                self.finder
                    .link(slot as u32, block.physical, finder_pc(block));
                Some(replaced as usize)
            }
            None => {
                let entry = Entry {
                    offset,
                    tag,
                    slot: slot as u32,
                };
                entries.insert(start, entry);
                if block.shared {
                    list_shared(&mut self.shared[page], entry);
                }
                self.finder
                    .link(slot as u32, block.physical, finder_pc(block));
                None
            }
        }
    }

    /// Stop finding the block in `slot`, at `physical`: whether it was found.
    #[cfg(test)]
    pub(super) fn forget(&mut self, physical: u64, slot: usize) -> bool {
        let (frame, offset) = split(physical);
        let Some(page) = self.page_of(frame) else {
            return false;
        };
        let entries = &mut self.entries[page];
        let Some(position) = entries
            .iter()
            .position(|entry| entry.offset == offset && entry.slot as usize == slot)
        else {
            return false;
        };
        entries.remove(position);
        self.shared[page].retain(|entry| entry.slot as usize != slot);
        self.finder.unlink(slot as u32);
        if entries.is_empty() {
            self.forget_page(frame);
        }
        true
    }

    /// Stop finding the blocks of the page at `frame` whose slots `marked`
    /// names, in one pass over the page.
    #[cfg(test)]
    pub(super) fn forget_marked(&mut self, frame: u64, marked: impl Fn(u32) -> bool) {
        self.forget_marked_in(frame, None, marked)
    }

    /// [`Self::forget_marked`] for blocks only in the runs of the sorted
    /// `tags`, or in all of them with none: the rest of the page moves up,
    /// unvisited.
    pub(super) fn forget_marked_in(
        &mut self,
        frame: u64,
        tags: Option<&[u16]>,
        marked: impl Fn(u32) -> bool,
    ) {
        let Some(page) = self.page_of(frame) else {
            return;
        };
        let entries = &mut self.entries[page];
        let runs = match tags {
            Some(tags) => tag_runs(entries, tags),
            None => core::iter::once(0..entries.len()).collect(),
        };
        let Some(first) = runs.first().map(|run| run.start) else {
            return;
        };
        let finder = &self.finder;
        let shared = &mut self.shared[page];
        let mut forgotten = Vec::new();
        let mut write = first;
        let mut runs = runs.into_iter().peekable();
        for read in first..entries.len() {
            while runs.peek().is_some_and(|run| run.end <= read) {
                runs.next();
            }
            let entry = entries[read];
            let visited = runs.peek().is_some_and(|run| run.contains(&read));
            if visited && marked(entry.slot) {
                finder.unlink(entry.slot);
                if !shared.is_empty() {
                    forgotten.push(entry.slot);
                }
                continue;
            }
            entries[write] = entry;
            write += 1;
        }
        entries.truncate(write);
        if !forgotten.is_empty() {
            forgotten.sort_unstable();
            shared.retain(|entry| forgotten.binary_search(&entry.slot).is_err());
        }
        if entries.is_empty() {
            self.forget_page(frame);
        }
    }

    /// Stop finding the blocks of the page at `frame`: whether it had any.
    /// Its list keeps a small capacity for the next page.
    pub(super) fn forget_page(&mut self, frame: u64) -> bool {
        let Some(page) = self.pages.remove(&frame) else {
            return false;
        };
        self.finder.pages.store(self.pages.len(), Ordering::Relaxed);
        let entries = &mut self.entries[page as usize];
        for entry in entries.iter() {
            self.finder.unlink(entry.slot);
        }
        entries.clear();
        entries.shrink_to(KEPT_ENTRIES);
        self.shared[page as usize].clear();
        self.free.push(page);
        let cached = &mut self.front[Self::front_slot(frame)];
        if cached.0 == frame {
            cached.0 = NO_FRAME;
        }
        true
    }

    pub(super) fn clear(&mut self) {
        self.finder.clear();
        self.pages.clear();
        self.entries.clear();
        self.shared.clear();
        self.free.clear();
        self.front.fill((NO_FRAME, 0));
    }

    /// The indexed blocks, past `cursor`, in page order, with the cursor
    /// that goes on after each.
    pub(super) fn sweep(
        &self,
        cursor: Option<Cursor>,
    ) -> impl Iterator<Item = (Cursor, usize)> + '_ {
        let (first, past) = cursor.map_or((0, None), |(frame, position)| (frame, Some(position)));
        self.pages.range(first..).flat_map(move |(&frame, &page)| {
            let skip = past
                .filter(|_| frame == first)
                .map_or(0, |position| position + 1);
            self.entries[page as usize]
                .iter()
                .enumerate()
                .skip(skip)
                .map(move |(position, entry)| ((frame, position), entry.slot as usize))
        })
    }

    /// The heap bytes it holds.
    pub(super) fn heap_bytes(&self) -> usize {
        let entries: usize = self.entries.iter().map(Vec::capacity).sum();
        let shared: usize = self.shared.iter().map(Vec::capacity).sum();
        (entries + shared) * size_of::<Entry>()
            + (self.entries.capacity() + self.shared.capacity()) * size_of::<Vec<Entry>>()
            + self.free.capacity() * size_of::<u32>()
            + self.front.len() * size_of::<(u64, u32)>()
            // A B-tree entry and its share of a node about half full.
            + self.pages.len() * 2 * size_of::<(u64, u32)>()
    }

    /// The physical pages that have indexed blocks.
    pub(super) fn frames(&self) -> impl Iterator<Item = u64> + '_ {
        self.pages.keys().copied()
    }

    #[cfg(any(test, all(feature = "std", feature = "native-profile")))]
    pub(super) fn len(&self) -> usize {
        self.entries.iter().map(Vec::len).sum()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// The most list entries [`Finder::find`] walks before it leaves the
/// answer to the locked index.
const FIND_STEPS: usize = 32;

/// The index's blocks by physical address and PC, read without the board's
/// lock: each bucket heads a list of slots (stored plus one, 0 ends it)
/// chained through `next`. Copies of one frame at other PCs go to other
/// buckets, so no list grows with them. Only the index's writer, under the lock, pushes a slot
/// at its bucket's head or unlinks one; an unlinked slot keeps its `next`,
/// so a reader standing on it goes on along its old list. A slot is taken
/// again only after its grace ([`super::board::Slots`]), so a reader inside
/// its section never sees a slot it stands on join another list.
pub(super) struct Finder {
    heads: Box<[AtomicU32]>,
    next: Records<AtomicU32>,
    /// Each slot's bucket while it is listed, plus one; 0 when it is not.
    /// The writer's alone: a link needs no walk to know a slot is listed,
    /// and an unlink knows its list.
    listed: Records<AtomicU32>,
    /// A slot had no room for its link: reads go to the locked index.
    partial: AtomicBool,
    /// The index's pages, for counts read without the lock.
    pages: AtomicUsize,
    /// A shared block was listed ([`SHARED_PC`]): a fetch that finds no
    /// block of its own PC looks there too.
    shares: AtomicBool,
    /// [`BlockIndex::exact`].
    exact: AtomicBool,
}

impl Finder {
    /// Buckets for up to `slots` block slots: one per 16 slots, at least
    /// 2^10 and at most 2^20 (4 MiB).
    pub(super) fn new(slots: u32) -> Self {
        let buckets = (slots as usize / 16)
            .next_power_of_two()
            .clamp(1 << 10, 1 << 20);
        let zeroed = vec![0u32; buckets].into_boxed_slice();
        // SAFETY: AtomicU32 has the size, alignment and bit validity of u32.
        let heads = unsafe { Box::from_raw(Box::into_raw(zeroed) as *mut [AtomicU32]) };
        Self {
            heads,
            next: Records::new(slots),
            listed: Records::new(slots),
            partial: AtomicBool::new(false),
            pages: AtomicUsize::new(0),
            shares: AtomicBool::new(false),
            exact: AtomicBool::new(false),
        }
    }

    fn bucket_of(&self, physical: u64, pc: u64) -> usize {
        (key_hash(physical, pc) >> 32) as usize & (self.heads.len() - 1)
    }

    /// The pages the index holds.
    pub(super) fn pages(&self) -> usize {
        self.pages.load(Ordering::Relaxed)
    }

    /// The block at `pc`, `physical` and `variant` among `blocks`, as the
    /// locked index's [`BlockIndex::get`] finds it (a shared one with
    /// `aliases`), or `None` when the lists cannot say (the locked index
    /// can). A reader inside its section, or holding the lock.
    pub(super) fn find(
        &self,
        blocks: &super::board::Blocks,
        pc: u64,
        physical: u64,
        variant: VariantId,
        aliases: bool,
    ) -> Option<Option<usize>> {
        if self.partial.load(Ordering::Relaxed) {
            return None;
        }
        let own = self.walk(blocks, physical, pc, |block| {
            !block.shared
                && block.physical == physical
                && block.pc == pc
                && block.variant == variant
        })?;
        if own.is_some() || !aliases || !self.shares.load(Ordering::Relaxed) {
            return Some(own);
        }
        let exact = self.exact.load(Ordering::Relaxed);
        self.walk(blocks, physical, SHARED_PC, |block| {
            block.shared
                && block.physical == physical
                && block.variant == variant
                && same_offset_and_half(block.pc, pc)
                && (!exact || block.pc == pc)
        })
    }

    /// The first live block of the list of `physical` and `pc` that
    /// `matches`, or `None` when the list is longer than [`FIND_STEPS`].
    fn walk(
        &self,
        blocks: &super::board::Blocks,
        physical: u64,
        pc: u64,
        matches: impl Fn(&Block) -> bool,
    ) -> Option<Option<usize>> {
        let mut link = self.heads[self.bucket_of(physical, pc)].load(Ordering::Acquire);
        for _ in 0..FIND_STEPS {
            let Some(slot) = link.checked_sub(1) else {
                return Some(None);
            };
            let found = blocks.get(slot as usize).is_some_and(&matches);
            if found && blocks.record(slot as usize).live() {
                return Some(Some(slot as usize));
            }
            link = self.next.get(slot)?.load(Ordering::Acquire);
        }
        None
    }

    /// Put `slot`, a block at `physical` and `pc`, at the head of its
    /// bucket's list, unless it is listed.
    fn link(&self, slot: u32, physical: u64, pc: u64) {
        let (Some(next), Some(listed)) =
            (self.next.try_reserve(slot), self.listed.try_reserve(slot))
        else {
            self.partial.store(true, Ordering::Relaxed);
            return;
        };
        if listed.load(Ordering::Relaxed) != 0 {
            return;
        }
        if pc == SHARED_PC {
            self.shares.store(true, Ordering::Relaxed);
        }
        let bucket = self.bucket_of(physical, pc);
        let head = &self.heads[bucket];
        next.store(head.load(Ordering::Relaxed), Ordering::Relaxed);
        head.store(slot + 1, Ordering::Release);
        listed.store(bucket as u32 + 1, Ordering::Relaxed);
    }

    /// Take `slot` out of its bucket's list, if it is listed; it keeps its
    /// `next`.
    fn unlink(&self, slot: u32) {
        let Some(listed) = self.listed.get(slot) else {
            return;
        };
        let Some(bucket) = listed.swap(0, Ordering::Relaxed).checked_sub(1) else {
            return;
        };
        let mut at = &self.heads[bucket as usize];
        loop {
            let link = at.load(Ordering::Relaxed);
            let Some(held) = link.checked_sub(1) else {
                return;
            };
            let Some(next) = self.next.get(held) else {
                return;
            };
            if held == slot {
                at.store(next.load(Ordering::Relaxed), Ordering::Release);
                return;
            }
            at = next;
        }
    }

    /// Empty every list.
    fn clear(&self) {
        for head in &self.heads {
            let mut link = head.swap(0, Ordering::Release);
            while let Some(slot) = link.checked_sub(1) {
                if let Some(listed) = self.listed.get(slot) {
                    listed.store(0, Ordering::Relaxed);
                }
                link = self
                    .next
                    .get(slot)
                    .map_or(0, |next| next.load(Ordering::Relaxed));
            }
        }
        self.pages.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::super::NativeCode;
    use super::*;

    fn block(pc: u64, physical: u64, variant: u32) -> Block {
        Block {
            pc,
            physical,
            code: NativeCode(0),
            instructions: 1,
            variant: VariantId(variant),
            successors: Box::default(),
            dynamic_site: None,
            landing: super::super::fetch::Landing::None,
            terminal: 0,
            source_hash: 0,
            installer: 0,
            part: super::super::board::NO_PART,
            device_stub: false,
            shared: false,
        }
    }

    fn push(blocks: &mut Vec<Block>, block: Block) -> usize {
        blocks.push(block);
        blocks.len() - 1
    }

    fn add(blocks: &mut Vec<Block>, index: &mut BlockIndex, block: Block) -> usize {
        let slot = push(blocks, block);
        assert_eq!(index.insert(blocks, slot), None);
        slot
    }

    /// A frame mapped at many guest pages keeps each page's blocks in a run:
    /// a walk for some pages visits only theirs, and forgetting marked
    /// blocks there keeps the others' and every unmarked block findable.
    #[test]
    fn a_frames_copies_at_other_pages_keep_runs_of_their_own() {
        const FRAME: u64 = 0x8000_0000;
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        let id = VariantId;
        let copies: Vec<u64> = (0..20u64)
            .map(|copy| 0x1_0000_0000 + copy * 0x10_0000)
            .collect();
        let mut slots = Vec::new();
        for &base in &copies {
            for offset in [0x10, 0x40, 0x80] {
                slots.push(add(
                    &mut blocks,
                    &mut index,
                    block(base + offset, FRAME + offset, 0),
                ));
            }
        }
        for (copy, &base) in copies.iter().enumerate() {
            for (n, offset) in [0x10, 0x40, 0x80].into_iter().enumerate() {
                assert_eq!(
                    index.get(&blocks, base + offset, FRAME + offset, id(0), false),
                    Some(slots[copy * 3 + n])
                );
            }
        }
        assert_eq!(
            index.sharing(&blocks, copies[3] + 0x40, FRAME + 0x40, id(0)),
            Some(true)
        );

        // Two copies' pages: their six blocks, from any position on.
        let tags = page_tags([copies[2] + 0x10, copies[7] + 0x80]);
        let (walked, listed) = index.page_tags_part(FRAME, &tags, 0, usize::MAX);
        assert_eq!(listed, 6);
        let mut expected: Vec<usize> = [2, 7]
            .iter()
            .flat_map(|&copy| slots[copy * 3..copy * 3 + 3].to_vec())
            .collect();
        let mut sorted = walked.clone();
        sorted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(sorted, expected);
        let (rest, _) = index.page_tags_part(FRAME, &tags, 4, usize::MAX);
        assert_eq!(rest, walked[4..]);
        let (part, _) = index.page_tags_part(FRAME, &tags, 1, 2);
        assert_eq!(part, walked[1..3]);

        // Forget one block of copy 2 and all of copy 7, visiting their runs.
        let gone = [
            slots[2 * 3 + 1],
            slots[7 * 3],
            slots[7 * 3 + 1],
            slots[7 * 3 + 2],
        ];
        index.forget_marked_in(FRAME, Some(&tags), |slot| gone.contains(&(slot as usize)));
        for (copy, &base) in copies.iter().enumerate() {
            for (n, offset) in [0x10, 0x40, 0x80].into_iter().enumerate() {
                let slot = slots[copy * 3 + n];
                let found = index.get(&blocks, base + offset, FRAME + offset, id(0), false);
                assert_eq!(
                    found,
                    (!gone.contains(&slot)).then_some(slot),
                    "copy {copy} offset {offset:#x}"
                );
            }
        }
        assert_eq!(index.page(FRAME).count(), 60 - gone.len());
        // A mark outside the runs is not visited.
        let other = slots[0];
        index.forget_marked_in(FRAME, Some(&tags), |slot| slot as usize == other);
        assert_eq!(
            index.get(&blocks, copies[0] + 0x10, FRAME + 0x10, id(0), false),
            Some(other)
        );
    }

    #[test]
    fn a_page_finds_its_blocks_by_every_part_of_their_key() {
        const PAGE: u64 = 0x8000_0000;
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        let late = add(&mut blocks, &mut index, block(0x1040, PAGE + 0x40, 0));
        let early = add(&mut blocks, &mut index, block(0x1010, PAGE + 0x10, 0));
        // The same physical word: through another mapping, and in another variant.
        let alias = add(&mut blocks, &mut index, block(0x9010, PAGE + 0x10, 0));
        let variant = add(&mut blocks, &mut index, block(0x1010, PAGE + 0x10, 1));
        let next = add(&mut blocks, &mut index, block(0x5000, PAGE + FETCH_PAGE, 0));
        let id = VariantId;
        assert_eq!(
            index.get(&blocks, 0x1010, PAGE + 0x10, id(0), false),
            Some(early)
        );
        assert_eq!(
            index.get(&blocks, 0x9010, PAGE + 0x10, id(0), false),
            Some(alias)
        );
        assert_eq!(
            index.get(&blocks, 0x1010, PAGE + 0x10, id(1), false),
            Some(variant)
        );
        assert_eq!(
            index.get(&blocks, 0x1040, PAGE + 0x40, id(0), false),
            Some(late)
        );
        assert_eq!(index.get(&blocks, 0x1010, PAGE + 0x14, id(0), false), None);
        assert_eq!(index.get(&blocks, 0x1010, PAGE + 0x10, id(2), false), None);

        // A recompilation of a key takes its place.
        let again = push(&mut blocks, block(0x1040, PAGE + 0x40, 0));
        assert_eq!(index.insert(&blocks, again), Some(late));
        assert_eq!(
            index.get(&blocks, 0x1040, PAGE + 0x40, id(0), false),
            Some(again)
        );
        assert_eq!(index.sweep(None).count(), 5);
        // Forgetting one block keeps the rest of its page.
        assert!(!index.forget(PAGE + 0x40, late));
        assert!(index.forget(PAGE + 0x40, again));
        assert_eq!(index.get(&blocks, 0x1040, PAGE + 0x40, id(0), false), None);
        assert_eq!(
            index.get(&blocks, 0x1010, PAGE + 0x10, id(0), false),
            Some(early)
        );
        assert_eq!(index.sweep(None).count(), 4);

        // A sweep goes on past its cursor, across pages.
        let order: Vec<usize> = index.sweep(None).map(|(_, slot)| slot).collect();
        assert_eq!(*order.last().unwrap(), next);
        let (cursor, _) = index.sweep(None).nth(1).unwrap();
        let rest: Vec<usize> = index.sweep(Some(cursor)).map(|(_, slot)| slot).collect();
        assert_eq!(rest, order[2..]);

        assert!(index.forget_page(PAGE));
        assert!(!index.forget_page(PAGE));
        assert_eq!(index.get(&blocks, 0x1010, PAGE + 0x10, id(0), false), None);
        assert_eq!(
            index.sweep(None).map(|(_, slot)| slot).collect::<Vec<_>>(),
            [next]
        );
        // The page of the last block forgotten goes with it.
        assert!(index.forget(PAGE + FETCH_PAGE, next));
        assert!(index.is_empty());
    }

    #[test]
    fn marked_blocks_leave_their_page_in_one_pass() {
        const PAGE: u64 = 0x8000_0000;
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        let slots: Vec<usize> = [0x10, 0x20, 0x30]
            .iter()
            .map(|&offset| {
                add(
                    &mut blocks,
                    &mut index,
                    block(0x1000 + offset, PAGE + offset, 0),
                )
            })
            .collect();
        index.forget_marked(PAGE, |slot| slot as usize != slots[1]);
        assert_eq!(index.page(PAGE).collect::<Vec<_>>(), [slots[1]]);
        assert_eq!(
            index.get(&blocks, 0x1020, PAGE + 0x20, VariantId(0), false),
            Some(slots[1])
        );
        index.forget_marked(PAGE, |_| true);
        assert!(index.is_empty());
        // A page indexed again takes the list the forgotten one left.
        let again = add(&mut blocks, &mut index, block(0x5000, PAGE + FETCH_PAGE, 0));
        assert_eq!(index.page(PAGE + FETCH_PAGE).collect::<Vec<_>>(), [again]);
    }

    /// `count` blocks of one physical word at as many PCs (the same code
    /// mapped at a new address by as many processes), as a board holds
    /// them: their slots and the index.
    fn aliases(count: usize) -> (super::super::board::Blocks, BlockIndex, Vec<usize>) {
        const PAGE: u64 = 0x8000_0000;
        let blocks = super::super::board::Blocks::new(count as u32 + 64);
        let grace = super::super::board::Grace::new(1);
        let mut index = BlockIndex::with_finder(Arc::new(Finder::new(count as u32 + 64)));
        let slots = (0..count as u64)
            .map(|alias| {
                let slot = blocks.take(&grace, Some(0)).unwrap();
                blocks.fill(slot, block(0x1_0000_0010 + (alias << 14), PAGE + 0x10, 0));
                assert_eq!(index.insert(&blocks, slot), None);
                slot
            })
            .collect();
        (blocks, index, slots)
    }

    /// A shared block installed for one guest page serves every page of
    /// its frame offset, variant and half, with aliases asked for, both
    /// locked and without the lock; a block of the PC's own comes first,
    /// and a shared block's link finds only a shared one.
    #[test]
    fn a_shared_block_serves_every_page_of_its_frame() {
        const PAGE: u64 = 0x8000_0000;
        let blocks = super::super::board::Blocks::new(64);
        let grace = super::super::board::Grace::new(1);
        let mut index = BlockIndex::with_finder(Arc::new(Finder::new(64)));
        let finder = Arc::clone(&index.finder);
        let install = |index: &mut BlockIndex, pc: u64, variant: u32, shared: bool| {
            let slot = blocks.take(&grace, Some(0)).unwrap();
            blocks.fill(
                slot,
                Block {
                    shared,
                    ..block(pc, PAGE + (pc & 0xfff), variant)
                },
            );
            assert_eq!(index.insert(&blocks, slot), None);
            slot
        };
        let (p, q) = (0x1_0000_0040, 0x7_4321_5040);
        let body = install(&mut index, p, 0, true);
        let id = VariantId;
        for pc in [p, q] {
            assert_eq!(index.get(&blocks, pc, PAGE + 0x40, id(0), true), Some(body));
            assert_eq!(
                finder.find(&blocks, pc, PAGE + 0x40, id(0), true),
                Some(Some(body))
            );
            // Without aliases, not even at the PC it was installed for.
            assert_eq!(index.get(&blocks, pc, PAGE + 0x40, id(0), false), None);
            assert_eq!(
                finder.find(&blocks, pc, PAGE + 0x40, id(0), false),
                Some(None)
            );
        }
        // Another variant, offset or half is another block.
        assert_eq!(index.get(&blocks, q, PAGE + 0x40, id(1), true), None);
        assert_eq!(index.get(&blocks, q + 4, PAGE + 0x44, id(0), true), None);
        let upper = q | 1 << 55;
        assert_eq!(index.get(&blocks, upper, PAGE + 0x40, id(0), true), None);
        assert_eq!(
            finder.find(&blocks, upper, PAGE + 0x40, id(0), true),
            Some(None)
        );
        // A block of Q's own comes first; the shared one stays for others.
        let own = install(&mut index, q, 0, false);
        assert_eq!(index.get(&blocks, q, PAGE + 0x40, id(0), true), Some(own));
        assert_eq!(
            finder.find(&blocks, q, PAGE + 0x40, id(0), true),
            Some(Some(own))
        );
        assert_eq!(index.get_shared(&blocks, q, PAGE + 0x40, id(0)), Some(body));
        let r = 0x2_0000_0040;
        assert_eq!(index.get(&blocks, r, PAGE + 0x40, id(0), true), Some(body));
        assert!(index.contains(&blocks, body) && index.contains(&blocks, own));
        // Forgotten, it serves no page.
        index.forget_marked(PAGE, |slot| slot as usize == body);
        assert!(!index.contains(&blocks, body));
        assert_eq!(index.get(&blocks, r, PAGE + 0x40, id(0), true), None);
        assert_eq!(
            finder.find(&blocks, r, PAGE + 0x40, id(0), true),
            Some(None)
        );
        assert_eq!(index.get(&blocks, q, PAGE + 0x40, id(0), true), Some(own));
    }

    /// Many copies of one frame at other PCs are each found without the
    /// lock, by their own PC, and the locked index finds each too
    /// (build 49: they shared one list of the finder, every insert walked
    /// it, and reads past 32 of them went to the locked index).
    #[test]
    fn many_aliases_of_one_frame_are_found_without_the_lock() {
        const PAGE: u64 = 0x8000_0000;
        let (blocks, mut index, slots) = aliases(1024);
        let finder = Arc::clone(&index.finder);
        for (alias, &slot) in slots.iter().enumerate() {
            let pc = 0x1_0000_0010 + ((alias as u64) << 14);
            assert_eq!(
                finder.find(&blocks, pc, PAGE + 0x10, VariantId(0), false),
                Some(Some(slot)),
                "alias {alias}"
            );
            assert_eq!(
                index.get(&blocks, pc, PAGE + 0x10, VariantId(0), false),
                Some(slot)
            );
            assert_eq!(
                finder.find(&blocks, pc, PAGE + 0x10, VariantId(1), false),
                Some(None)
            );
        }
        // Each leaves by itself, and the others stay.
        index.forget_marked(PAGE, |slot| slot as usize == slots[7]);
        let pc = |alias: u64| 0x1_0000_0010 + (alias << 14);
        assert_eq!(
            finder.find(&blocks, pc(7), PAGE + 0x10, VariantId(0), false),
            Some(None)
        );
        assert_eq!(
            index.get(&blocks, pc(7), PAGE + 0x10, VariantId(0), false),
            None
        );
        assert_eq!(
            finder.find(&blocks, pc(8), PAGE + 0x10, VariantId(0), false),
            Some(Some(slots[8]))
        );
        assert!(index.forget_page(PAGE));
        assert_eq!(
            finder.find(&blocks, pc(8), PAGE + 0x10, VariantId(0), false),
            Some(None)
        );
    }

    /// Host timing of inserts and lookups as copies of one frame grow:
    /// `cargo test -p stela-engine --release --lib alias_cost -- --ignored
    /// --nocapture`.
    #[test]
    #[ignore = "manual: opt-in"]
    fn alias_cost() {
        extern crate std;
        const PAGE: u64 = 0x8000_0000;
        for count in [64, 256, 1024, 4096] {
            let started = std::time::Instant::now();
            let (blocks, mut index, slots) = aliases(count);
            let insert = started.elapsed().as_nanos() / count as u128;
            let started = std::time::Instant::now();
            let mut locked = 0;
            for alias in 0..count as u64 {
                let pc = 0x1_0000_0010 + (alias << 14);
                if index
                    .finder
                    .find(&blocks, pc, PAGE + 0x10, VariantId(0), false)
                    .is_none()
                {
                    locked += 1;
                    assert!(
                        index
                            .get(&blocks, pc, PAGE + 0x10, VariantId(0), false)
                            .is_some()
                    );
                }
            }
            let find = started.elapsed().as_nanos() / count as u128;
            std::println!(
                "aliases={count} insert_ns={insert} find_ns={find} locked_fallbacks={locked} slots={}",
                slots.len()
            );
        }
    }

    /// A frame as a slid binary's launches leave it: `copies` copies of
    /// its blocks at offsets 0x10, 0x20 and 0x30, each at another guest
    /// page (not shared, as a PAC terminal's), with shared blocks at 0x20
    /// in variants 0 and 1 installed for one page among them.
    fn launched_frame(copies: u64) -> (super::super::board::Blocks, BlockIndex, [usize; 2]) {
        const PAGE: u64 = 0x8000_0000;
        let blocks = super::super::board::Blocks::new(4 * copies as u32 + 64);
        let grace = super::super::board::Grace::new(1);
        let mut index = BlockIndex::with_finder(Arc::new(Finder::new(4 * copies as u32 + 64)));
        let mut install = |pc: u64, variant: u32, shared: bool| {
            let slot = blocks.take(&grace, Some(0)).unwrap();
            blocks.fill(
                slot,
                Block {
                    shared,
                    ..block(pc, PAGE + (pc & 0xfff), variant)
                },
            );
            index.insert(&blocks, slot);
            slot
        };
        let mut shared = [0; 2];
        for copy in 0..copies {
            let base = 0x1_0000_0000 + (copy << 14);
            for offset in [0x10, 0x20, 0x30] {
                install(base + offset, 0, false);
            }
            if copy == copies / 2 {
                shared = [install(base + 0x20, 0, true), install(base + 0x20, 1, true)];
            }
        }
        (blocks, index, shared)
    }

    /// `--shared-offset-index`: a shared block's lookup through its frame's
    /// list of shared blocks finds what the walk of every copy's run finds,
    /// for every offset, variant, half and own-PC setting, and the list
    /// follows inserts that replace a block, forgets of marked blocks in
    /// some runs or all, and a forgotten page indexed again.
    #[test]
    fn the_offset_index_finds_what_the_walk_of_every_copy_finds() {
        const PAGE: u64 = 0x8000_0000;
        let (blocks, mut index, shared) = launched_frame(40);
        let grace = super::super::board::Grace::new(1);
        let agree = |index: &mut BlockIndex, expect: &[(u64, u32, Option<usize>)]| {
            for exact in [false, true] {
                index.set_exact(exact);
                for &(pc, variant, wanted) in expect {
                    let physical = PAGE + (pc & 0xfff);
                    let mut answers = [false, true].map(|offsets| {
                        index.set_offsets(offsets);
                        (
                            index.get(&blocks, pc, physical, VariantId(variant), true),
                            index.get_shared(&blocks, pc, physical, VariantId(variant)),
                        )
                    });
                    assert_eq!(answers[0], answers[1], "{pc:#x} variant {variant}");
                    if !exact && let Some(wanted) = wanted {
                        assert_eq!(answers[1].1, Some(wanted), "{pc:#x}");
                    }
                    answers[0] = (None, None);
                }
            }
            index.set_exact(false);
        };
        let alias = 0x7_0000_0020;
        let upper = alias | 1 << 55;
        let queries = |shared: [usize; 2]| {
            [
                (alias, 0, Some(shared[0])),
                (alias, 1, Some(shared[1])),
                (alias, 2, None),
                (alias - 0x10, 0, None),
                (alias + 0x10, 0, None),
                (upper, 0, None),
                (0x1_0000_0020, 0, None),
                (0x1_0000_0000 + (20 << 14) + 0x20, 0, None),
            ]
        };
        agree(&mut index, &queries(shared));
        // The shared block of variant 0 installed again in its slot's place.
        let again = blocks.take(&grace, Some(0)).unwrap();
        let (pc, physical) = (blocks[shared[0]].pc, blocks[shared[0]].physical);
        blocks.fill(
            again,
            Block {
                shared: true,
                ..block(pc, physical, 0)
            },
        );
        assert_eq!(index.insert(&blocks, again), Some(shared[0]));
        let shared = [again, shared[1]];
        agree(&mut index, &queries(shared));
        // Forgetting in the runs of other pages leaves the shared blocks;
        // in their own page's run, variant 1's goes.
        let own = blocks[shared[1]].pc;
        let others = page_tags((0..10).map(|copy| 0x1_0000_0000 + (copy << 14)));
        index.forget_marked_in(PAGE, Some(&others), |_| true);
        agree(&mut index, &queries(shared));
        let tags = page_tags([own]);
        index.forget_marked_in(PAGE, Some(&tags), |slot| slot as usize == shared[1]);
        index.set_offsets(true);
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x20, VariantId(1)),
            None
        );
        agree(
            &mut index,
            &queries(shared)
                .map(|(pc, variant, wanted)| (pc, variant, wanted.filter(|_| variant == 0))),
        );
        // A whole forget, then the page again.
        index.forget_marked(PAGE, |_| true);
        index.set_offsets(true);
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x20, VariantId(0)),
            None
        );
        assert_eq!(index.insert(&blocks, shared[0]), None);
        agree(&mut index, &[(alias, 0, Some(shared[0])), (alias, 1, None)]);
        assert!(index.forget_page(PAGE));
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x20, VariantId(0)),
            None
        );
        assert_eq!(index.insert(&blocks, shared[1]), None);
        agree(&mut index, &[(alias, 1, Some(shared[1])), (alias, 0, None)]);
    }

    /// The shared lookups count the runs they visit: a walk passes copies
    /// of the frame at other guest pages before the shared block's, the
    /// frame's shared list one search.
    #[test]
    fn shared_lookups_count_the_runs_they_visit() {
        const PAGE: u64 = 0x8000_0000;
        let (blocks, mut index, shared) = launched_frame(40);
        let alias = 0x7_0000_0020;
        let found = index.get(&blocks, alias, PAGE + 0x20, VariantId(1), true);
        assert_eq!(found, Some(shared[1]));
        let [lookups, runs] = index.shared_lookups;
        assert_eq!(lookups, 1);
        assert!(runs > 1, "{runs} runs");
        index.set_offsets(true);
        let found = index.get(&blocks, alias, PAGE + 0x20, VariantId(1), true);
        assert_eq!(found, Some(shared[1]));
        assert_eq!(index.shared_lookups, [lookups + 1, runs + 1]);
        // A block of the PC's own needs no shared lookup.
        let own = 0x1_0000_0020;
        index.get(&blocks, own, PAGE + 0x20, VariantId(0), true);
        assert_eq!(index.shared_lookups, [lookups + 1, runs + 1]);
    }

    /// A slot the frame's shared list names that now holds a shared block
    /// of another frame at the same offset (an entry the list kept by
    /// mistake): the lookup through the list never answers with it.
    #[test]
    fn the_shared_list_never_answers_with_another_frames_block() {
        const A: u64 = 0x8000_0000;
        const B: u64 = 0x9000_0000;
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        index.set_offsets(true);
        let shared = |pc: u64, frame: u64| Block {
            shared: true,
            ..block(pc, frame + (pc & 0xfff), 0)
        };
        let slot = add(&mut blocks, &mut index, shared(0x1_0000_0040, A));
        let alias = 0x7_0000_0040;
        assert_eq!(
            index.get_shared(&blocks, alias, A + 0x40, VariantId(0)),
            Some(slot)
        );
        // The slot taken by frame B's block of the same offset, variant
        // and half, A's list unchanged.
        blocks[slot] = shared(0x2_0000_0040, B);
        assert_eq!(
            index.get_shared(&blocks, alias, A + 0x40, VariantId(0)),
            None
        );
        assert_eq!(
            index.get(&blocks, alias, A + 0x40, VariantId(0), true),
            None
        );
    }

    /// A replacing insert keeps the shared list right whatever the shared
    /// mark of the block it replaces and of the new one: a replaced shared
    /// block's slot leaves it, a new shared block joins it.
    #[test]
    fn a_replacing_insert_keeps_the_shared_list_right() {
        const PAGE: u64 = 0x8000_0000;
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        index.set_offsets(true);
        let alias = 0x7_0000_0040;
        let at = |shared: bool| Block {
            shared,
            ..block(0x1_0000_0040, PAGE + 0x40, 0)
        };
        let first = add(&mut blocks, &mut index, at(true));
        // The same key again: replaced, and the list names the new slot.
        let second = push(&mut blocks, at(true));
        assert_eq!(index.insert(&blocks, second), Some(first));
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x40, VariantId(0)),
            Some(second)
        );
        assert!(
            index.shared[index.page_of(PAGE).unwrap()]
                .iter()
                .all(|entry| entry.slot as usize != first)
        );
        // A replacement of another shared mark, forced in place: the
        // replaced shared slot leaves the list, and a non-shared block
        // never joins it.
        blocks[second] = at(false);
        let third = push(&mut blocks, at(false));
        assert_eq!(index.insert(&blocks, third), Some(second));
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x40, VariantId(0)),
            None
        );
        assert!(index.shared[index.page_of(PAGE).unwrap()].is_empty());
        // And back: a shared block replacing a non-shared one joins it.
        blocks[third] = at(true);
        let fourth = push(&mut blocks, at(true));
        assert_eq!(index.insert(&blocks, fourth), Some(third));
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x40, VariantId(0)),
            Some(fourth)
        );
        // Reuse of the old slot by another frame's shared block at the
        // same offset: never found for this frame.
        blocks[first] = Block {
            shared: true,
            ..block(0x2_0000_0040, 0x9000_0040, 0)
        };
        assert_eq!(
            index.get_shared(&blocks, alias, PAGE + 0x40, VariantId(0)),
            Some(fourth)
        );
    }

    /// Random inserts, replacing inserts, forgets of marked blocks in some
    /// runs or all and forgotten pages over a few frames, offsets, PCs and
    /// variants: after each, every lookup through the shared lists answers
    /// as the walk of every run does.
    #[test]
    fn random_index_changes_keep_the_shared_lists_in_step_with_the_walk() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |below: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % below
        };
        let frames = [0x8000_0000u64, 0x8000_1000, 0x9000_0000];
        let mut blocks = Vec::new();
        let mut index = BlockIndex::new();
        let mut indexed: Vec<usize> = Vec::new();
        for step in 0..4000 {
            match next(10) {
                0..=5 => {
                    let frame = frames[next(3) as usize];
                    let offset = 0x10 * next(4);
                    let page = (0x1_0000_0000 + (next(24) << 14)) | (next(2) << 55);
                    let slot = push(
                        &mut blocks,
                        Block {
                            shared: next(3) == 0,
                            ..block(page + offset, frame + offset, next(2) as u32)
                        },
                    );
                    index.insert(&blocks, slot);
                    indexed.push(slot);
                }
                6 | 7 if !indexed.is_empty() => {
                    let victim = indexed[next(indexed.len() as u64) as usize];
                    let (pc, physical) = (blocks[victim].pc, blocks[victim].physical);
                    let frame = physical & !(FETCH_PAGE - 1);
                    if next(2) == 0 {
                        index.forget_marked(frame, |slot| slot as usize == victim);
                    } else {
                        let tags = page_tags([pc]);
                        index.forget_marked_in(frame, Some(&tags), |slot| {
                            slot as usize == victim || slot % 3 == 0
                        });
                    }
                }
                8 => {
                    index.forget_page(frames[next(3) as usize]);
                }
                _ => {}
            }
            for _ in 0..8 {
                let frame = frames[next(3) as usize];
                let offset = 0x10 * next(4);
                let pc = (0x1_0000_0000 + (next(32) << 14) + offset) | (next(2) << 55);
                let variant = VariantId(next(2) as u32);
                let answers = [false, true].map(|offsets| {
                    index.set_offsets(offsets);
                    (
                        index.get(&blocks, pc, frame + offset, variant, true),
                        index.get_shared(&blocks, pc, frame + offset, variant),
                    )
                });
                assert_eq!(answers[0], answers[1], "step {step} pc {pc:#x}");
            }
        }
    }

    /// Host timing of a shared block's locked lookup at an alias PC as a
    /// frame's copies at other guest pages grow, by a walk of every run and
    /// through the frame's shared list: `cargo test -p stela-engine
    /// --release --lib shared_lookup_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "manual: opt-in"]
    fn shared_lookup_cost() {
        extern crate std;
        const PAGE: u64 = 0x8000_0000;
        const ROUNDS: u32 = 20_000;
        for copies in [1, 16, 64, 256, 1024] {
            let (blocks, mut index, shared) = launched_frame(copies);
            let mut line = std::format!("copies={copies}");
            for offsets in [false, true] {
                index.set_offsets(offsets);
                let started = std::time::Instant::now();
                for round in 0..ROUNDS {
                    let pc = 0x7_0000_0020 + (u64::from(round % 64) << 14);
                    let found = index.get(&blocks, pc, PAGE + 0x20, VariantId(0), true);
                    assert_eq!(found, Some(shared[0]));
                }
                let ns = started.elapsed().as_nanos() / u128::from(ROUNDS);
                line.push_str(&std::format!(" offsets={offsets}:{ns}ns"));
            }
            std::println!("{line}");
        }
    }
}
