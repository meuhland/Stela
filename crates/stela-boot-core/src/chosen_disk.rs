//! Whether the partition Install macOS chose for the Mac's SSD
//! (`EFI/stela/disk.cfg`, `SERIAL:GUID`) is still the one to boot.
//!
//! The USB keeps that record when the disk is later replaced, wiped,
//! repartitioned or unplugged, and when the USB moves to another PC. Such a
//! record is stale: the macOS choice is left out, Install macOS chooses a
//! disk again, and the record is replaced only when a new disk is
//! confirmed. Only a store that cannot be read on a partition that is
//! otherwise the chosen one, with a Mac system on it, still stops the boot.
//!
//! The firmware reports no serial for some disks: OVMF's virtio-blk driver
//! has none to give, where Stela's own driver reads it (`VIRTIO_BLK_T_GET_ID`)
//! once it takes the device. When no disk reports the serial, a disk that
//! reports none and whose GPT holds the chosen partition's unique GUID,
//! which Install macOS wrote fresh, is the chosen disk, if it is the only
//! one ([`choose`]). A disk that reports another serial is never taken.

/// What a search for the chosen partition found: every whole disk
/// reporting the serial, then its GPT partitions with the GUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    /// No whole disk reports the serial.
    NoDisk,
    /// Several whole disks report the serial.
    SeveralDisks,
    /// No disk reports the serial, and several that report none hold the
    /// partition's GUID.
    SeveralUnnamed,
    /// The disk has no GPT, or none that can be read: a blank or wiped disk.
    NoTable,
    /// The GPT has no partition with the GUID.
    NoPartition,
    /// Several of the GPT's partitions have the GUID.
    SeveralPartitions,
    /// The partition with the GUID is an EFI system partition now.
    EspPartition,
    /// Exactly one partition, which is not an EFI system partition.
    Found,
}

/// Why the chosen partition can no longer be booted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stale {
    /// No disk on this PC reports the chosen serial: the disk was replaced
    /// or unplugged, or the USB moved to another PC.
    Absent,
    /// Several disks report the serial, so none can be told to be it.
    Ambiguous,
    /// Several disks that report no serial hold the chosen partition's
    /// GUID, so none can be told to be it.
    AmbiguousUnnamed,
    /// The disk is there but has no partition table: it was wiped.
    Wiped,
    /// The disk is there but the chosen partition is not: it was deleted
    /// or the disk repartitioned.
    PartitionGone,
    /// The partition is there but holds no Mac system and its NVRAM store
    /// cannot be read, as an interrupted install leaves it.
    NoMacOs,
}

impl Stale {
    /// The record for a search that did not find exactly the chosen
    /// partition; `None` when it did.
    pub fn of(search: Search) -> Option<Self> {
        match search {
            Search::NoDisk => Some(Self::Absent),
            Search::SeveralDisks => Some(Self::Ambiguous),
            Search::SeveralUnnamed => Some(Self::AmbiguousUnnamed),
            Search::NoTable => Some(Self::Wiped),
            Search::NoPartition | Search::SeveralPartitions | Search::EspPartition => {
                Some(Self::PartitionGone)
            }
            Search::Found => None,
        }
    }

    /// The case, for the one log line (`CHOSEN DISK stale: ...`).
    pub fn case(self) -> &'static str {
        match self {
            Self::Absent => "no disk on this PC has the chosen serial",
            Self::Ambiguous => "several disks on this PC report the chosen serial",
            Self::AmbiguousUnnamed => {
                "no disk reports the chosen serial and several without one hold the chosen partition"
            }
            Self::Wiped => "the chosen disk has no partition table",
            Self::PartitionGone => "the chosen disk no longer has the chosen partition",
            Self::NoMacOs => "the chosen partition holds no macOS and no readable NVRAM",
        }
    }

    /// Why the macOS choice is left out of the menu.
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoMacOs => "the disk chosen before has no macOS",
            _ => "the disk chosen before is not there",
        }
    }

    /// The two lines the menu shows while it waits for Install macOS.
    pub fn screen(self) -> &'static str {
        match self {
            Self::NoMacOs => {
                "The disk chosen before has no macOS.\nInstall macOS will choose again."
            }
            _ => "The disk chosen before is not there.\nInstall macOS will choose again.",
        }
    }
}

/// One whole disk as the firmware lists it, for [`choose`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listed<'a> {
    /// The serial it reports, if any.
    pub serial: Option<&'a str>,
    /// Its GPT partitions with the chosen GUID, `None` without a table that
    /// can be read (or one that was not read: a disk of another serial).
    pub matching: Option<usize>,
    /// Whether the one partition with the GUID is an EFI system partition.
    pub esp: bool,
}

/// The chosen disk among `disks`, by its index, for the chosen `serial`, or
/// what was found instead. The one disk reporting the serial; when none
/// does, the one disk reporting no serial whose GPT holds the partition's
/// GUID. Then exactly one partition with the GUID, which is no EFI system
/// partition. The serial a disk found by its GUID alone has is for Stela's
/// own driver to read: the engine finds it by its serial, or by the GUID on
/// exactly one disk (`stela_runtime::pc::storage`).
pub fn choose(disks: &[Listed<'_>], serial: &str) -> Result<usize, Search> {
    let reporting: alloc::vec::Vec<usize> = (0..disks.len())
        .filter(|&index| disks[index].serial == Some(serial))
        .collect();
    let index = match reporting[..] {
        [index] => index,
        [] => {
            let holding: alloc::vec::Vec<usize> = (0..disks.len())
                .filter(|&index| {
                    disks[index].serial.is_none()
                        && disks[index].matching.is_some_and(|count| count > 0)
                })
                .collect();
            match holding[..] {
                [index] => index,
                [] => return Err(Search::NoDisk),
                _ => return Err(Search::SeveralUnnamed),
            }
        }
        _ => return Err(Search::SeveralDisks),
    };
    let disk = &disks[index];
    match disk.matching {
        None => Err(Search::NoTable),
        Some(0) => Err(Search::NoPartition),
        Some(1) if disk.esp => Err(Search::EspPartition),
        Some(1) => Ok(index),
        Some(_) => Err(Search::SeveralPartitions),
    }
}

/// What a chosen partition whose NVRAM store could not be read is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// The partition holds no Mac system container: an install that did not
    /// finish, which Install macOS repeats.
    Unprepared,
    /// The partition holds a Mac system, or whether it does could not be
    /// read: the boot stops with [`UNREADABLE_STORE`].
    Fatal,
}

/// A store that could not be read on the chosen partition, given whether
/// that partition holds no Mac system container (`blank`, `None` when that
/// could not be read either).
pub fn unreadable_store(blank: Option<bool>) -> Unreadable {
    match blank {
        Some(true) => Unreadable::Unprepared,
        Some(false) | None => Unreadable::Fatal,
    }
}

/// Why the boot stops on [`Unreadable::Fatal`], and what to do.
pub const UNREADABLE_STORE: &str = "NVRAM on the disk chosen for macOS cannot be read: choose \
     Install macOS again, or run stela setup repair on this USB";

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(serial: Option<&str>, matching: Option<usize>) -> Listed<'_> {
        Listed {
            serial,
            matching,
            esp: false,
        }
    }

    #[test]
    fn a_disk_without_a_serial_is_chosen_by_its_unique_partition_guid() {
        // OVMF's virtio-blk disk reports no serial; another disk reports one.
        let disks = [
            listed(Some("USB-STICK"), None),
            listed(None, Some(1)),
            listed(None, Some(0)),
        ];
        assert_eq!(choose(&disks, "123456DRIVE"), Ok(1));
        // Two disks without a serial hold the GUID: none is taken.
        let disks = [listed(None, Some(1)), listed(None, Some(1))];
        assert_eq!(choose(&disks, "123456DRIVE"), Err(Search::SeveralUnnamed));
        assert_eq!(
            Stale::of(Search::SeveralUnnamed),
            Some(Stale::AmbiguousUnnamed)
        );
        // None holds it, or none could be read: absent, as before.
        let disks = [listed(None, Some(0)), listed(None, None)];
        assert_eq!(choose(&disks, "123456DRIVE"), Err(Search::NoDisk));
        // Its partition is still checked as a reporting disk's is.
        let mut esp = listed(None, Some(1));
        esp.esp = true;
        assert_eq!(choose(&[esp], "S"), Err(Search::EspPartition));
        assert_eq!(
            choose(&[listed(None, Some(2))], "S"),
            Err(Search::SeveralPartitions)
        );
    }

    #[test]
    fn a_disk_reporting_another_serial_is_never_taken_by_its_guid() {
        // The disk was replaced: the new one reports its own serial and
        // holds a partition with the GUID (a cloned table). Stale, as
        // before.
        let disks = [listed(Some("NEW-DISK"), Some(1))];
        assert_eq!(choose(&disks, "123456DRIVE"), Err(Search::NoDisk));
        assert_eq!(Stale::of(Search::NoDisk), Some(Stale::Absent));
        // A disk reporting the serial wins over one reporting none.
        let disks = [listed(None, Some(1)), listed(Some("123456DRIVE"), Some(1))];
        assert_eq!(choose(&disks, "123456DRIVE"), Ok(1));
        // And every rule for the reporting disk stands.
        let disks = [
            listed(Some("123456DRIVE"), Some(1)),
            listed(Some("123456DRIVE"), Some(1)),
        ];
        assert_eq!(choose(&disks, "123456DRIVE"), Err(Search::SeveralDisks));
        let cases = [
            (None, Search::NoTable),
            (Some(0), Search::NoPartition),
            (Some(2), Search::SeveralPartitions),
        ];
        for (matching, search) in cases {
            let disks = [listed(Some("D"), matching), listed(None, Some(1))];
            assert_eq!(choose(&disks, "D"), Err(search));
        }
    }

    #[test]
    fn the_found_partition_is_not_stale() {
        assert_eq!(Stale::of(Search::Found), None);
    }

    #[test]
    fn a_missing_disk_or_partition_is_stale() {
        assert_eq!(Stale::of(Search::NoDisk), Some(Stale::Absent));
        assert_eq!(Stale::of(Search::SeveralDisks), Some(Stale::Ambiguous));
        assert_eq!(Stale::of(Search::NoTable), Some(Stale::Wiped));
        for search in [
            Search::NoPartition,
            Search::SeveralPartitions,
            Search::EspPartition,
        ] {
            assert_eq!(Stale::of(search), Some(Stale::PartitionGone));
        }
    }

    #[test]
    fn each_stale_case_has_its_own_line_and_two_plain_screen_lines() {
        let all = [
            Stale::Absent,
            Stale::Ambiguous,
            Stale::AmbiguousUnnamed,
            Stale::Wiped,
            Stale::PartitionGone,
            Stale::NoMacOs,
        ];
        for (n, a) in all.iter().enumerate() {
            for b in &all[n + 1..] {
                assert_ne!(a.case(), b.case());
            }
            assert_eq!(a.screen().lines().count(), 2, "{a:?}");
            assert!(a.screen().ends_with("Install macOS will choose again."));
            assert!(a.screen().lines().all(|line| line.len() <= 40), "{a:?}");
        }
        assert!(Stale::Wiped.screen().contains("is not there"));
        assert!(Stale::NoMacOs.screen().contains("has no macOS"));
    }

    #[test]
    fn an_unreadable_store_without_a_system_is_an_unfinished_install() {
        assert_eq!(unreadable_store(Some(true)), Unreadable::Unprepared);
        assert_eq!(unreadable_store(Some(false)), Unreadable::Fatal);
        assert_eq!(unreadable_store(None), Unreadable::Fatal);
        assert!(UNREADABLE_STORE.contains("Install macOS"));
        assert!(UNREADABLE_STORE.contains("stela setup repair"));
    }
}
