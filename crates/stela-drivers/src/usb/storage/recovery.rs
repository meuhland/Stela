// SPDX-License-Identifier: GPL-2.0-only
//! The retries of a USB storage command its transport failed, and their
//! log records. Linux usb-storage answers a transport error
//! (`USB_STOR_TRANSPORT_ERROR`) with a reset and completes the command with
//! `DID_ERROR`; the SCSI midlayer then requeues it while it has retries
//! left (`scsi_decide_disposition`'s `maybe_retry`: sd allows
//! `SD_MAX_RETRIES`) and has not run past its runtime limit. The reset
//! ladder itself is the transport's and its owner's: the xHCI's soft retries
//! of a transaction error, a halt's clear on a stall, the Bulk-Only class
//! reset with both halts cleared, and a port reset when that fails
//! ([`super::media::Owner`]). This module counts the runs and keeps one
//! record per command the ladder ran for.
use super::{SD_MAX_RETRIES, SD_RUNTIME_US};
use crate::block::BlockError;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Whether `error` is a failure of the transport rather than an answer of
/// the unit: Linux's `DID_ERROR` (a transfer error, a malformed wrapper or
/// status, a failed auto-sense) and `DID_TIME_OUT`.
pub(super) fn transport_error(error: &BlockError) -> bool {
    matches!(
        error,
        BlockError::Device { .. } | BlockError::Timeout | BlockError::CheckCondition
    ) || *error == super::bot::PROTOCOL_ERROR
        || *error == super::uas::PROTOCOL_ERROR
}

/// What ended a run of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// An xHCI completion code.
    Device(u32),
    Timeout,
    /// CHECK CONDITION without readable sense data.
    NoSense,
    /// A malformed wrapper, status or information unit.
    Protocol,
    /// A reset run for another command ended it.
    Reset,
    /// Any other answer, such as the unit's sense or a refusal.
    Other,
    /// The device left its port: a port reset found it gone.
    Departed,
}
impl Cause {
    fn of(error: &BlockError) -> Self {
        match error {
            BlockError::Device { status } => Self::Device(*status),
            BlockError::Timeout => Self::Timeout,
            BlockError::CheckCondition => Self::NoSense,
            _ if *error == super::bot::PROTOCOL_ERROR || *error == super::uas::PROTOCOL_ERROR => {
                Self::Protocol
            }
            _ if *error == super::media::RESET_ENDED => Self::Reset,
            _ if crate::usb::xhci::departed(error) => Self::Departed,
            _ => Self::Other,
        }
    }
    fn encode(self) -> u64 {
        let (kind, status) = match self {
            Self::Device(status) => (1, status.min(0xffff)),
            Self::Timeout => (2, 0),
            Self::NoSense => (3, 0),
            Self::Protocol => (4, 0),
            Self::Reset => (5, 0),
            Self::Other => (6, 0),
            Self::Departed => (7, 0),
        };
        (kind << 16 | u64::from(status)) & 0x7_ffff
    }
    fn decode(bits: u64) -> Self {
        let status = (bits & 0xffff) as u32;
        match bits >> 16 & 7 {
            1 => Self::Device(status),
            2 => Self::Timeout,
            3 => Self::NoSense,
            4 => Self::Protocol,
            5 => Self::Reset,
            7 => Self::Departed,
            _ => Self::Other,
        }
    }
}
impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(status) => f.write_str(&crate::usb::xhci::explain(&BlockError::Device {
                status: *status,
            })),
            Self::Timeout => f.write_str("a time-out"),
            Self::NoSense => f.write_str("a failed status without sense data"),
            Self::Protocol => f.write_str("a malformed wrapper or status"),
            Self::Reset => f.write_str("a device reset for another command"),
            Self::Other => f.write_str("the unit's answer"),
            Self::Departed => f.write_str("the device gone from its port"),
        }
    }
}

/// The furthest step of the ladder a command's runs went through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    /// The transport took the command again as it was.
    Again,
    /// A Bulk-Only class reset and both halts' clears.
    ClassReset,
    /// A port reset and a fresh transport bound to the same device.
    PortReset,
}
impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Again => "running it again",
            Self::ClassReset => "a class reset and both halt clears",
            Self::PortReset => "a port reset and rebind",
        })
    }
}

/// How a command the ladder ran for ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Completed,
    /// Its runs reached sd's retries.
    Retries,
    /// It ran past sd's runtime limit.
    Runtime,
    /// Its unit is no longer published: the device was lost, its medium
    /// changed, or its transport needs a rebind the owner cannot run.
    Lost,
    /// A run ended with an answer that is no transport error.
    Answered,
    /// The port reset the ladder ran failed, with the record's `last`
    /// cause; its unit is no longer published.
    ResetFailed,
}
impl End {
    fn encode(self) -> u64 {
        self as u64
    }
    fn decode(bits: u64) -> Self {
        match bits & 7 {
            0 => Self::Completed,
            1 => Self::Retries,
            2 => Self::Runtime,
            3 => Self::Lost,
            5 => Self::ResetFailed,
            _ => Self::Answered,
        }
    }
}

/// One command the recovery ladder ran for: what first failed it, the
/// furthest step it took, how many times it ran and how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recovery {
    pub first: Cause,
    pub last: Cause,
    pub step: Step,
    pub runs: u32,
    pub end: End,
}
impl fmt::Display for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = SD_MAX_RETRIES + 1;
        write!(f, "{} failed a command; ", self.first)?;
        match self.end {
            End::Completed => write!(
                f,
                "{} recovered it, and it completed on run {} of {total}",
                self.step, self.runs
            ),
            End::Retries => write!(
                f,
                "after {} runs with {} it still failed with {}: retries exhausted",
                self.runs, self.step, self.last
            ),
            End::Runtime => write!(
                f,
                "after {} runs with {} it failed with {}: past the {} s runtime limit",
                self.runs,
                self.step,
                self.last,
                SD_RUNTIME_US / 1_000_000
            ),
            End::Lost if self.runs <= 1 => write!(
                f,
                "the device needs a reset this owner cannot run, or left the bus: the disk \
                 fails until it is bound again"
            ),
            End::Lost => write!(
                f,
                "after {} runs with {} it failed with {}: the device needs a reset this \
                 owner cannot run, or left the bus",
                self.runs, self.step, self.last
            ),
            End::Answered => write!(
                f,
                "after {} runs with {} the unit answered with an error",
                self.runs, self.step
            ),
            End::ResetFailed => write!(
                f,
                "after {} runs the port reset failed with {}: the disk fails until it is \
                 bound again",
                self.runs, self.last
            ),
        }
    }
}

/// Records kept between two [`take_recoveries`]; a later one replaces the
/// oldest past that.
const KEPT: usize = 16;
const VALID: u64 = 1 << 63;
static RECORDS: [AtomicU64; KEPT] = [const { AtomicU64::new(0) }; KEPT];
static NEXT: AtomicU32 = AtomicU32::new(0);
static DROPPED: AtomicU32 = AtomicU32::new(0);
const SEQUENCE_BITS: u32 = 15;

impl Recovery {
    fn encode(&self, sequence: u32) -> u64 {
        VALID
            | u64::from(sequence & ((1 << SEQUENCE_BITS) - 1)) << 48
            | self.end.encode() << 45
            | (u64::from(self.runs.min(15))) << 41
            | (self.step as u64) << 38
            | self.last.encode() << 19
            | self.first.encode()
    }
    fn decode(bits: u64) -> (u32, Self) {
        let step = match bits >> 38 & 3 {
            0 => Step::Again,
            1 => Step::ClassReset,
            _ => Step::PortReset,
        };
        (
            (bits >> 48) as u32 & ((1 << SEQUENCE_BITS) - 1),
            Self {
                first: Cause::decode(bits & 0x7_ffff),
                last: Cause::decode(bits >> 19 & 0x7_ffff),
                step,
                runs: (bits >> 41 & 15) as u32,
                end: End::decode(bits >> 45),
            },
        )
    }
}

fn ring_record(recovery: Recovery) {
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = &RECORDS[sequence as usize % KEPT];
    if slot.swap(recovery.encode(sequence), Ordering::AcqRel) != 0 {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

fn ring_take() -> (alloc::vec::Vec<Recovery>, u32) {
    let mut taken: alloc::vec::Vec<(u32, Recovery)> = RECORDS
        .iter()
        .map(|slot| slot.swap(0, Ordering::AcqRel))
        .filter(|bits| bits & VALID != 0)
        .map(Recovery::decode)
        .collect();
    let newest = NEXT.load(Ordering::Relaxed);
    // Ages from the newest sequence number, which wrap as it does.
    let mask = (1 << SEQUENCE_BITS) - 1;
    taken.sort_by_key(|(sequence, _)| core::cmp::Reverse(newest.wrapping_sub(*sequence) & mask));
    (
        taken.into_iter().map(|(_, recovery)| recovery).collect(),
        DROPPED.swap(0, Ordering::Relaxed),
    )
}

#[cfg(not(test))]
fn record(recovery: Recovery) {
    ring_record(recovery);
}

/// The commands the recovery ladder ran for since the last call, oldest
/// first, and how many records were replaced before they were taken.
#[cfg(not(test))]
pub fn take_recoveries() -> (alloc::vec::Vec<Recovery>, u32) {
    ring_take()
}

// Tests run in parallel threads: each keeps its own records.
#[cfg(test)]
extern crate std;
#[cfg(test)]
std::thread_local! {
    static TEST_RECORDS: core::cell::RefCell<alloc::vec::Vec<Recovery>> =
        const { core::cell::RefCell::new(alloc::vec::Vec::new()) };
}
#[cfg(test)]
fn record(recovery: Recovery) {
    TEST_RECORDS.with(|records| records.borrow_mut().push(recovery));
}
#[cfg(test)]
pub fn take_recoveries() -> (alloc::vec::Vec<Recovery>, u32) {
    (TEST_RECORDS.with(|records| records.take()), 0)
}

/// Published units whose medium was found changed since the last
/// [`take_media_changes`], and the last sense that said so, packed as
/// `VALID_SENSE | key << 16 | asc << 8 | ascq`.
const VALID_SENSE: u32 = 1 << 31;
#[cfg(not(test))]
static MEDIA_CHANGES: AtomicU32 = AtomicU32::new(0);
#[cfg(not(test))]
static MEDIA_CHANGE_SENSE: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
std::thread_local! {
    static TEST_MEDIA: core::cell::Cell<(u32, u32)> = const { core::cell::Cell::new((0, 0)) };
}

fn pack(why: Option<crate::block::Sense>) -> u32 {
    why.map_or(0, |sense| {
        VALID_SENSE | u32::from(sense.key) << 16 | u32::from(sense.asc) << 8 | u32::from(sense.ascq)
    })
}
fn unpack(bits: u32) -> Option<crate::block::Sense> {
    (bits & VALID_SENSE != 0).then_some(crate::block::Sense {
        key: (bits >> 16) as u8,
        asc: (bits >> 8) as u8,
        ascq: bits as u8,
    })
}

/// A published unit's medium was found changed, by the sense `why`, or by
/// an eject without one: its disk fails until it is published again.
pub(super) fn media_changed(why: Option<crate::block::Sense>) {
    #[cfg(not(test))]
    {
        MEDIA_CHANGES.fetch_add(1, Ordering::Relaxed);
        MEDIA_CHANGE_SENSE.store(pack(why), Ordering::Relaxed);
    }
    #[cfg(test)]
    TEST_MEDIA.with(|media| media.set((media.get().0 + 1, pack(why))));
}

/// How many published units found their medium changed since the last
/// call, and the last sense that said so.
pub fn take_media_changes() -> (u32, Option<crate::block::Sense>) {
    #[cfg(not(test))]
    let (count, sense) = (
        MEDIA_CHANGES.swap(0, Ordering::Relaxed),
        MEDIA_CHANGE_SENSE.load(Ordering::Relaxed),
    );
    #[cfg(test)]
    let (count, sense) = TEST_MEDIA.with(|media| media.replace((0, 0)));
    (count, unpack(sense))
}

/// A command's runs on its way down the ladder.
#[derive(Clone, Copy, Debug)]
pub(super) struct Ladder {
    started: u64,
    /// Runs after the first.
    retries: u32,
    first: Option<Cause>,
    step: Step,
}
impl Ladder {
    pub(super) fn new(started: u64) -> Self {
        Self {
            started,
            retries: 0,
            first: None,
            step: Step::Again,
        }
    }
    /// Whether the command, which ended with `error` at `now` after the
    /// ladder ran `step` for it, runs again: a transport error, or any
    /// failure a port reset ended (Linux retries what a reset ended,
    /// `DID_RESET`), while within sd's retries and runtime and while its
    /// unit is still published on a usable transport (`usable`). Records
    /// the command's end when it does not run again.
    pub(super) fn again(&mut self, error: &BlockError, step: Step, now: u64, usable: bool) -> bool {
        let transport = transport_error(error) || step == Step::PortReset;
        if transport && self.first.is_none() {
            self.first = Some(Cause::of(error));
        }
        if transport {
            self.step = self.step.max(step);
        }
        let end = if !transport {
            End::Answered
        } else if !usable {
            End::Lost
        } else if self.retries >= SD_MAX_RETRIES {
            End::Retries
        } else if now.wrapping_sub(self.started) > SD_RUNTIME_US {
            End::Runtime
        } else {
            self.retries += 1;
            return true;
        };
        self.end(end, Cause::of(error));
        false
    }
    /// The port reset run for the command, which ended with `error`,
    /// failed with `reset`: the command ends, recorded with the reset's
    /// cause, as Linux fails the commands of a device whose reset failed.
    pub(super) fn reset_failed(&mut self, error: &BlockError, reset: &BlockError) {
        if self.first.is_none() {
            self.first = Some(Cause::of(error));
        }
        self.step = Step::PortReset;
        self.end(End::ResetFailed, Cause::of(reset));
    }
    /// The ladder ran `step` for the command since its last failure.
    pub(super) fn reached(&mut self, step: Step) {
        if self.first.is_some() {
            self.step = self.step.max(step);
        }
    }
    /// The command completed.
    pub(super) fn completed(&mut self) {
        self.end(End::Completed, Cause::Other);
    }
    fn end(&mut self, end: End, last: Cause) {
        // A command the ladder never ran for needs no record, nor one a
        // unit's answer ended at its first run.
        let Some(first) = self.first.take() else {
            return;
        };
        record(Recovery {
            first,
            last,
            step: self.step,
            runs: self.retries + 1,
            end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_survives_its_encoding() {
        let recovery = Recovery {
            first: Cause::Device(4),
            last: Cause::Timeout,
            step: Step::PortReset,
            runs: 6,
            end: End::Retries,
        };
        let (sequence, decoded) = Recovery::decode(recovery.encode(1234));
        assert_eq!((sequence, decoded), (1234, recovery));
    }

    /// The one test of the static ring: the rest keep their records apart.
    #[test]
    fn the_ring_keeps_the_newest_records_in_order_and_counts_the_replaced() {
        let recovery = |runs| Recovery {
            first: Cause::Device(4),
            last: Cause::Device(4),
            step: Step::ClassReset,
            runs,
            end: End::Completed,
        };
        ring_take();
        for runs in 0..KEPT as u32 + 3 {
            ring_record(recovery(runs % 16));
        }
        let (records, dropped) = ring_take();
        assert_eq!(dropped, 3);
        let runs: alloc::vec::Vec<u32> = records.iter().map(|r| r.runs).collect();
        let expected: alloc::vec::Vec<u32> = (3..KEPT as u32 + 3).map(|runs| runs % 16).collect();
        assert_eq!(runs, expected);
        assert_eq!(ring_take(), (alloc::vec::Vec::new(), 0));
    }

    #[test]
    fn the_ladder_retries_transport_errors_within_sd_limits_and_records_once() {
        take_recoveries();
        let error = BlockError::Device { status: 4 };
        // Five retries, then exhausted.
        let mut ladder = Ladder::new(0);
        for _ in 0..SD_MAX_RETRIES {
            assert!(ladder.again(&error, Step::ClassReset, 0, true));
        }
        assert!(!ladder.again(&error, Step::ClassReset, 0, true));
        // Past the runtime limit.
        let mut late = Ladder::new(0);
        assert!(!late.again(&BlockError::Timeout, Step::Again, SD_RUNTIME_US + 1, true));
        // A unit's answer is no transport error, and needs no record.
        let mut answered = Ladder::new(0);
        assert!(!answered.again(&BlockError::ReadOnly, Step::Again, 0, true));
        answered.completed();
        // One recovered after a retry.
        let mut recovered = Ladder::new(0);
        assert!(recovered.again(&error, Step::ClassReset, 0, true));
        recovered.completed();
        // A lost unit does not run again.
        let mut lost = Ladder::new(0);
        assert!(!lost.again(&error, Step::ClassReset, 0, false));
        let (records, dropped) = take_recoveries();
        assert_eq!(dropped, 0);
        let ends: alloc::vec::Vec<_> = records.iter().map(|r| (r.end, r.runs)).collect();
        assert_eq!(
            ends,
            [
                (End::Retries, SD_MAX_RETRIES + 1),
                (End::Runtime, 1),
                (End::Completed, 2),
                (End::Lost, 1)
            ]
        );
        assert_eq!(records[0].first, Cause::Device(4));
        assert!(std::format!("{}", records[2]).starts_with(
            "USB transaction error (xHCI completion code 4) failed a command; a class reset"
        ));
    }
}
