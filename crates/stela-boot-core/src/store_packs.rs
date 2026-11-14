//! Which translation pack file a translator without one takes, and which
//! becomes the next spare, in `stela/store` on the USB's data partition.
//!
//! The firmware grows a file only by writing every byte (minutes over USB),
//! so `stela.efi` makes no pack file: a new translator renames one. `stela
//! setup` prepares a spare (`spare.bin`) for that, but an `stela.efi`
//! replaced without setup finds the spare gone after the first new
//! translator took it, and every later one ran with no store. So a
//! translator with no pack and no spare takes the pack of the least
//! recently modified other translator ([`recycled`]): the journal keeps
//! each commit under its translator's id and reuses other ids' space, so
//! the old content reads as empty and is overwritten. Once it has a pack, an
//! older pack becomes the next spare ([`spare`]), the newest other pack
//! staying for the build that wrote it.

/// A pack file in the store's directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackFile<'a> {
    /// Its 8.3 name, as the directory gives it.
    pub name: &'a str,
    pub bytes: u64,
    /// When it was last modified (or taken), as a stamp that sorts by time
    /// ([`stamp`]).
    pub modified: u64,
}

/// A stamp of a time that sorts as the time does.
pub fn stamp(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> u64 {
    let date = (u64::from(year) * 13 + u64::from(month)) * 32 + u64::from(day);
    ((date * 24 + u64::from(hour)) * 60 + u64::from(minute)) * 60 + u64::from(second)
}

/// The packs of other translators than `own` that hold a store's `bound`
/// bytes, least recently modified first, by name for a tie.
fn others<'a>(packs: &[PackFile<'a>], own: &str, bound: u64) -> alloc::vec::Vec<usize> {
    let mut others: alloc::vec::Vec<usize> = (0..packs.len())
        .filter(|&index| {
            !packs[index].name.eq_ignore_ascii_case(own) && packs[index].bytes >= bound
        })
        .collect();
    others.sort_by_key(|&index| (packs[index].modified, packs[index].name));
    others
}

/// The pack `own`'s translator takes when it has none and there is no
/// spare: the least recently modified other pack of at least `bound` bytes.
/// Never `own`'s, which is not missing then anyway.
pub fn recycled(packs: &[PackFile<'_>], own: &str, bound: u64) -> Option<usize> {
    if packs.iter().any(|pack| pack.name.eq_ignore_ascii_case(own)) {
        return None;
    }
    others(packs, own, bound).first().copied()
}

/// The pack that becomes the next spare when there is none, once `own`'s
/// translator has its pack: the least recently modified other pack of at
/// least `bound` bytes, while another stays for the build that wrote it.
pub fn spare(packs: &[PackFile<'_>], own: &str, bound: u64) -> Option<usize> {
    let others = others(packs, own, bound);
    (others.len() >= 2).then(|| others[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUND: u64 = 3 << 30;

    fn pack(name: &str, bytes: u64, day: u8) -> PackFile<'_> {
        PackFile {
            name,
            bytes,
            modified: stamp(2026, 11, day, 12, 0, 0),
        }
    }

    #[test]
    fn a_translator_without_a_pack_takes_the_least_recently_used_other() {
        let packs = [
            pack("846cebb8.pak", BOUND, 12),
            pack("FAC05158.PAK", BOUND, 11),
            pack("bc6b91b3.pak", BOUND, 13),
        ];
        assert_eq!(recycled(&packs, "d94617af.pak", BOUND), Some(1));
        // Then the next oldest becomes the spare; the newest stays.
        let after = [
            pack("846cebb8.pak", BOUND, 12),
            pack("d94617af.pak", BOUND, 14),
            pack("bc6b91b3.pak", BOUND, 13),
        ];
        assert_eq!(spare(&after, "d94617af.pak", BOUND), Some(0));
        let last = [
            pack("d94617af.pak", BOUND, 14),
            pack("bc6b91b3.pak", BOUND, 13),
        ];
        assert_eq!(spare(&last, "d94617af.pak", BOUND), None);
        // One other pack is still taken for a translator with none.
        assert_eq!(
            recycled(&[pack("bc6b91b3.pak", BOUND, 13)], "d94617af.pak", BOUND),
            Some(0)
        );
    }

    #[test]
    fn the_own_pack_is_never_taken() {
        let packs = [
            pack("D94617AF.PAK", BOUND, 1),
            pack("bc6b91b3.pak", BOUND, 13),
        ];
        assert_eq!(recycled(&packs, "d94617af.pak", BOUND), None);
        let packs = [
            pack("d94617af.pak", BOUND, 1),
            pack("bc6b91b3.pak", BOUND, 13),
            pack("846cebb8.pak", BOUND, 12),
        ];
        assert_eq!(spare(&packs, "d94617af.pak", BOUND), Some(2));
    }

    #[test]
    fn a_pack_smaller_than_the_bound_or_none_at_all_gives_nothing() {
        let packs = [pack("bc6b91b3.pak", BOUND - 1, 1)];
        assert_eq!(recycled(&packs, "d94617af.pak", BOUND), None);
        assert_eq!(recycled(&[], "d94617af.pak", BOUND), None);
        let packs = [
            pack("846cebb8.pak", BOUND - 1, 1),
            pack("bc6b91b3.pak", BOUND, 2),
            pack("fac05158.pak", BOUND, 3),
        ];
        assert_eq!(recycled(&packs, "d94617af.pak", BOUND), Some(1));
    }

    #[test]
    fn stamps_sort_as_times_do() {
        assert!(stamp(2026, 11, 13, 23, 59, 59) < stamp(2026, 11, 14, 0, 0, 0));
        assert!(stamp(2026, 12, 31, 0, 0, 0) < stamp(2027, 1, 1, 0, 0, 0));
    }
}
