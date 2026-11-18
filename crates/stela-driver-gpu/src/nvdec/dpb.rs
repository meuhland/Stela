//! Surface slots and output order: which surface each picture is decoded
//! to, when a surface is free again, and when a picture can be shown.
//!
//! NVDEC itself keeps no picture buffer: the driver names a surface slot
//! per picture (`CurrPicIdx`, `curr_pic_idx`) and the slots its references
//! sit in. In nvdec-cap-1118 those slots are FFmpeg's (`nvdec.c`), which
//! NVIDIA's `libnvcuvid` passes through. To give NVIDIA's setups byte for
//! byte, the parsers place pictures as FFmpeg does:
//!
//! - **The free slots are a stack** (FFmpeg's `AVBufferPool` of picture
//!   indices): a slot given back is the next one taken, and a new slot is
//!   only opened when none is free.
//! - **A slot is held** while its picture is a reference, and while it waits
//!   to be shown (in output order). H.264 gives slots back at the start of
//!   the next picture, in the order of FFmpeg's picture array; HEVC gives
//!   back those its reference picture set drops at the start of the
//!   picture, then those shown at that start, before taking the picture's.
//! - **Output order** is FFmpeg's: H.264 shows a picture once more than
//!   `has_b_frames` wait (the VUI's `max_num_reorder_frames`), HEVC bumps
//!   per `sps_max_num_reorder_pics` and `sps_max_dec_pic_buffering`.
//!
//! The per-codec rules are in [`super::parse::h264`] and
//! [`super::parse::hevc`]; this module holds what they share.
//!
//! **Slots are not surfaces.** The slot is the picture index the setup
//! names (`CurrPicIdx`, `dpb[].index`, `RefDiffPicOrderCnts[slot]`);
//! NVIDIA's driver binds each slot to one of its surfaces, and the method
//! stream names surfaces. A slot keeps its surface, except when the
//! picture in that surface is being shown as the slot is decoded into
//! again (HEVC gives a shown picture's slot back before it is copied out):
//! then the slot moves to the lowest surface no slot holds and nobody is
//! showing ([`Surfaces`]). hevc-main-bf moves slot 3 to surface 4 at its
//! sixth picture, slot 0 to surface 5 at its tenth.
use alloc::vec::Vec;

/// A picture ready to be shown: its slot, the surface it is in, its order
/// count and which decode it was (0 for the stream's first picture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub slot: u8,
    pub surface: u8,
    pub poc: i32,
    pub decoded: u32,
}

/// The surface slots: a stack of free ones, and how many were ever opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pool {
    free: Vec<u8>,
    opened: u8,
    most: u8,
}

impl Pool {
    /// A pool of at most `most` slots, none opened yet.
    pub fn new(most: u8) -> Self {
        Self {
            free: Vec::new(),
            opened: 0,
            most,
        }
    }

    /// The most recently freed slot, or a new one; none when all `most` are
    /// held.
    pub fn take(&mut self) -> Option<u8> {
        if let Some(slot) = self.free.pop() {
            return Some(slot);
        }
        if self.opened < self.most {
            self.opened += 1;
            return Some(self.opened - 1);
        }
        None
    }

    /// Gives `slot` back: the next [`Pool::take`] returns it.
    pub fn give(&mut self, slot: u8) {
        if slot < self.opened && !self.free.contains(&slot) {
            self.free.push(slot);
        }
    }

    /// The highest slot opened so far.
    pub fn high_water(&self) -> Option<u8> {
        self.opened.checked_sub(1)
    }

    /// The slots that may be held at once.
    pub fn most(&self) -> u8 {
        self.most
    }

    /// Raises the bound to `most` (a new sequence that needs more).
    pub fn widen(&mut self, most: u8) {
        self.most = self.most.max(most);
    }

    /// Whether `slot` is free.
    pub fn is_free(&self, slot: u8) -> bool {
        slot >= self.opened || self.free.contains(&slot)
    }
}

/// The binding of slots to surfaces, as NVIDIA's driver keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Surfaces {
    bound: [Option<u8>; 16],
    most: u8,
}

impl Surfaces {
    /// Bindings onto at most `most` surfaces, none bound yet.
    pub fn new(most: u8) -> Self {
        Self {
            bound: [None; 16],
            most,
        }
    }

    /// The surface `slot` is bound to.
    pub fn of(&self, slot: u8) -> Option<u8> {
        self.bound.get(usize::from(slot)).copied().flatten()
    }

    /// Binds `slot` for a picture decoded into it while the surfaces in
    /// `showing` are being shown: its own surface if not among them, else
    /// (or when it has none) the lowest surface no other slot holds and
    /// not among them.
    pub fn bind(&mut self, slot: u8, showing: &[u8]) -> Option<u8> {
        let index = usize::from(slot);
        if index >= self.bound.len() {
            return None;
        }
        if let Some(surface) = self.bound[index]
            && !showing.contains(&surface)
        {
            return Some(surface);
        }
        let surface = (0..self.most).find(|s| {
            !showing.contains(s)
                && !self
                    .bound
                    .iter()
                    .enumerate()
                    .any(|(k, b)| k != index && *b == Some(*s))
        })?;
        self.bound[index] = Some(surface);
        Some(surface)
    }

    /// Each slot's surface, 0 for a slot never bound.
    pub fn table(&self) -> [u8; 16] {
        self.bound.map(|b| b.unwrap_or(0))
    }

    /// The surfaces there may be.
    pub fn most(&self) -> u8 {
        self.most
    }

    pub fn widen(&mut self, most: u8) {
        self.most = self.most.max(most);
    }

    /// The highest surface bound so far.
    pub fn high_water(&self) -> Option<u8> {
        self.bound.iter().flatten().max().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freed_slot_is_taken_first_and_new_ones_only_when_none_is_free() {
        let mut pool = Pool::new(4);
        assert_eq!(pool.take(), Some(0));
        assert_eq!(pool.take(), Some(1));
        assert_eq!(pool.take(), Some(2));
        pool.give(0);
        pool.give(2);
        // Last in, first out, as FFmpeg's AVBufferPool.
        assert_eq!(pool.take(), Some(2));
        assert_eq!(pool.take(), Some(0));
        assert_eq!(pool.take(), Some(3));
        assert_eq!(pool.take(), None);
        assert_eq!(pool.high_water(), Some(3));
        assert!(!pool.is_free(1));
        pool.give(1);
        pool.give(1);
        assert!(pool.is_free(1));
        assert_eq!(pool.take(), Some(1));
        assert_eq!(pool.take(), None);
    }

    #[test]
    fn a_slot_whose_picture_is_being_shown_moves_to_a_free_surface() {
        // hevc-main-bf's first ten pictures: slots 0, 1, 2, 3, 3 (its
        // surface showing), 4, 0, 0, 0 (its surface showing).
        let mut surfaces = Surfaces::new(8);
        for slot in 0..4 {
            assert_eq!(surfaces.bind(slot, &[]), Some(slot));
        }
        assert_eq!(surfaces.bind(3, &[2]), Some(3));
        assert_eq!(surfaces.bind(3, &[3]), Some(4));
        assert_eq!(surfaces.bind(4, &[1]), Some(3));
        assert_eq!(surfaces.bind(0, &[]), Some(0));
        assert_eq!(surfaces.bind(0, &[0]), Some(5));
        assert_eq!(surfaces.table()[..5], [5, 1, 2, 4, 3]);
        assert_eq!(surfaces.high_water(), Some(5));
    }
}
