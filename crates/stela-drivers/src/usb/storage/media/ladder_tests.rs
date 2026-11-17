//! The recovery ladder of a published unit's commands, on a transport that
//! fails each step once ([`Flaky`]).
use super::super::recovery::{Cause, End, Step, take_media_changes, take_recoveries};
use super::*;
use crate::block::BlockDevice;
use alloc::collections::VecDeque;
use alloc::vec;

const TRANSACTION: BlockError = BlockError::Device { status: 4 };

/// What a data command does.
#[derive(Clone, Copy)]
enum Fail {
    /// A transaction error the transport recovers from by itself.
    Again,
    /// A transaction error after which a class reset clears both halts;
    /// the device then raises a UNIT ATTENTION, and the transport takes no
    /// command for a few looks while the reset settles.
    ClassReset,
    /// A transaction error that leaves the transport needing a port reset.
    Poison,
    /// As [`Self::Poison`], and the port reset fails.
    Lost,
}

#[derive(Default)]
struct Script {
    data: VecDeque<Fail>,
    /// Data commands run.
    runs: u32,
    port_resets: u32,
    port_reset_fails: bool,
}

/// A removable unit of eight 512-byte blocks.
struct Flaky {
    script: Rc<RefCell<Script>>,
    now: Rc<Cell<u64>>,
    generation: u64,
    poisoned: bool,
    attention: bool,
    settling: u32,
    begun: Option<Result<Vec<u8>, BlockError>>,
}
impl Flaky {
    fn new(script: Rc<RefCell<Script>>, now: Rc<Cell<u64>>) -> Self {
        Self {
            script,
            now,
            generation: 0,
            poisoned: false,
            attention: false,
            settling: 0,
            begun: None,
        }
    }
}
impl Scsi for Flaky {
    type Lun = u8;
    fn command(&mut self, _: u8, cdb: &[u8], data: Data<'_>) -> Result<Vec<u8>, BlockError> {
        if self.poisoned {
            return Err(BlockError::Unsupported(
                "USB storage interface requires rebind",
            ));
        }
        self.now.set(self.now.get() + 1_000);
        if core::mem::take(&mut self.attention) {
            return Err(BlockError::Sense(Sense {
                key: Sense::UNIT_ATTENTION,
                asc: Sense::RESET_OCCURRED,
                ascq: 0,
            }));
        }
        let bytes = |length| match data {
            Data::In(bytes) => vec![0x5a; bytes.min(length)],
            _ => Vec::new(),
        };
        match cdb[0] {
            scsi::INQUIRY => {
                let mut inquiry = vec![0; scsi::INQUIRY_BYTES];
                inquiry[1] = 0x80;
                Ok(inquiry)
            }
            scsi::READ_CAPACITY_10 => Ok(vec![0, 0, 0, 7, 0, 0, 2, 0]),
            scsi::READ_10 | scsi::WRITE_10 => {
                let mut script = self.script.borrow_mut();
                script.runs += 1;
                match script.data.pop_front() {
                    None => Ok(bytes(usize::MAX)),
                    Some(Fail::Again) => Err(TRANSACTION),
                    Some(Fail::ClassReset) => {
                        self.generation += 1;
                        self.attention = true;
                        self.settling = 3;
                        Err(TRANSACTION)
                    }
                    Some(Fail::Poison) => {
                        self.poisoned = true;
                        Err(TRANSACTION)
                    }
                    Some(Fail::Lost) => {
                        self.poisoned = true;
                        script.port_reset_fails = true;
                        Err(TRANSACTION)
                    }
                }
            }
            _ => Ok(bytes(usize::MAX)),
        }
    }
    fn delay(&mut self, micros: u64) -> Result<(), BlockError> {
        self.now.set(self.now.get() + micros);
        Ok(())
    }
    fn now_us(&self) -> u64 {
        self.now.get()
    }
    fn poisoned(&self) -> bool {
        self.poisoned
    }
    fn take_failed_data(&mut self) -> Option<Vec<u8>> {
        None
    }
    fn queued(&self) -> bool {
        true
    }
    fn begin(&mut self, lun: u8, cdb: &[u8], data: Data<'_>) -> Option<Result<usize, BlockError>> {
        if self.busy() {
            return Some(Err(BlockError::Protocol("busy")));
        }
        let result = self.command(lun, cdb, data);
        self.begun = Some(result);
        Some(Ok(0))
    }
    fn end(&mut self, _: usize) -> Option<Result<Vec<u8>, BlockError>> {
        self.begun.take()
    }
    fn busy(&mut self) -> bool {
        if self.settling > 0 && self.begun.is_none() {
            self.settling -= 1;
            return true;
        }
        self.begun.is_some()
    }
    fn write_protect_page(&self) -> Option<u8> {
        None
    }
    fn caching(&self) -> Caching {
        Caching::WriteThrough
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn reset_device(
        &mut self,
        _: &mut dyn crate::bus::dma::DmaAllocator,
    ) -> Option<Result<Self, (String, BlockError)>> {
        let mut script = self.script.borrow_mut();
        script.port_resets += 1;
        Some(if script.port_reset_fails {
            Err(("port reset".into(), TRANSACTION))
        } else {
            let mut fresh = Flaky::new(self.script.clone(), self.now.clone());
            fresh.attention = true;
            Ok(fresh)
        })
    }
}

/// A published disk on a [`Flaky`] transport that runs `data` for its
/// data commands, and its script.
fn published(data: &[Fail]) -> (Disk<Owner<Flaky>>, Rc<RefCell<Script>>) {
    let script = Rc::new(RefCell::new(Script::default()));
    let now = Rc::new(Cell::new(0));
    let owner = Rc::new(RefCell::new(Owner::new(
        Flaky::new(script.clone(), now),
        "port 1 interface 0".into(),
        "serial".into(),
    )));
    let mut failures = Vec::new();
    let mut disks = scan_bot(owner, &mut failures).unwrap();
    assert!(failures.is_empty());
    script.borrow_mut().data = data.iter().copied().collect();
    (disks.pop().unwrap(), script)
}

fn recoveries() -> Vec<(Cause, Step, u32, End)> {
    take_recoveries()
        .0
        .into_iter()
        .map(|recovery| (recovery.first, recovery.step, recovery.runs, recovery.end))
        .collect()
}

#[test]
fn a_transaction_error_recovered_by_a_class_reset_runs_the_read_again() {
    let (mut disk, script) = published(&[Fail::ClassReset]);
    let mut block = [0; 512];
    assert_eq!(disk.read(0, &mut block), Ok(()));
    assert_eq!(block, [0x5a; 512]);
    // The reset's UNIT ATTENTION is expected: the removable unit keeps its
    // medium.
    assert!(disk.media.get());
    assert_eq!(script.borrow().runs, 2);
    assert_eq!(
        recoveries(),
        [(Cause::Device(4), Step::ClassReset, 2, End::Completed)]
    );
}

#[test]
fn each_step_failing_once_climbs_the_ladder_to_a_port_reset() {
    let (mut disk, script) = published(&[Fail::Again, Fail::ClassReset, Fail::Poison]);
    assert_eq!(disk.write(1, &[7; 512]), Ok(()));
    assert!(disk.media.get());
    assert_eq!(script.borrow().runs, 4);
    assert_eq!(script.borrow().port_resets, 1);
    assert_eq!(
        recoveries(),
        [(Cause::Device(4), Step::PortReset, 4, End::Completed)]
    );
}

#[test]
fn a_device_that_keeps_failing_ends_the_command_after_sd_retries() {
    let (mut disk, script) = published(&[Fail::ClassReset; 8]);
    assert_eq!(disk.read(0, &mut [0; 512]), Err(TRANSACTION));
    assert_eq!(script.borrow().runs, SD_MAX_RETRIES + 1);
    assert_eq!(
        recoveries(),
        [(
            Cause::Device(4),
            Step::ClassReset,
            SD_MAX_RETRIES + 1,
            End::Retries
        )]
    );
    // The next command runs: the unit is still published.
    assert_eq!(disk.read(0, &mut [0; 512]), Ok(()));
}

/// A port reset that fails ends the command, recorded once with the reset's
/// own cause; the disk fails from then on without another reset or record.
#[test]
fn a_failed_port_reset_ends_the_command_and_the_disk() {
    let (mut disk, script) = published(&[Fail::Lost]);
    assert_eq!(disk.read(0, &mut [0; 512]), Err(TRANSACTION));
    assert_eq!(script.borrow().runs, 1);
    assert_eq!(script.borrow().port_resets, 1);
    let records = take_recoveries().0;
    assert_eq!(records.len(), 1);
    assert_eq!(
        (
            records[0].first,
            records[0].step,
            records[0].runs,
            records[0].end
        ),
        (Cause::Device(4), Step::PortReset, 1, End::ResetFailed)
    );
    assert_eq!(records[0].last, Cause::Device(4));
    assert!(alloc::format!("{}", records[0]).ends_with(
        "after 1 runs the port reset failed with USB transaction error (xHCI completion code \
         4): the disk fails until it is bound again"
    ));
    assert!(disk.read(0, &mut [0; 512]).is_err());
    assert_eq!(script.borrow().runs, 1);
    assert_eq!(script.borrow().port_resets, 1);
    assert!(recoveries().is_empty());
}

#[test]
fn a_queued_read_waits_for_the_class_reset_to_settle_and_runs_again() {
    let (mut disk, script) = published(&[Fail::ClassReset]);
    let tag = disk
        .start(Command::Read {
            lba: 2,
            bytes: 1024,
        })
        .unwrap();
    let (mut looks, mut waits) = (0, 0);
    let finished = loop {
        if let Some(finished) = disk.finish(tag) {
            break finished;
        }
        // A waiting command is looked at, and goes before new ones.
        if disk.bot.borrow().waiting() {
            assert!(disk.posted());
            assert!(!disk.room());
            waits += 1;
        }
        looks += 1;
        assert!(looks < 10);
    };
    assert_eq!(finished, Ok(Some(vec![0x5a; 1024])));
    assert_eq!(waits, 3, "one per look while the reset settles");
    assert!(disk.media.get());
    assert_eq!(script.borrow().runs, 2);
    assert_eq!(
        recoveries(),
        [(Cause::Device(4), Step::ClassReset, 2, End::Completed)]
    );
}

#[test]
fn an_unexpected_unit_attention_ends_a_removable_disk_and_is_counted_with_its_sense() {
    let (mut disk, _script) = published(&[]);
    take_media_changes();
    // A UNIT ATTENTION no reset of this owner raised: a removable unit's
    // medium may have changed, as Linux's sd takes it.
    disk.bot.borrow_mut().transport.attention = true;
    assert!(matches!(
        disk.read(0, &mut [0; 512]),
        Err(BlockError::Sense(_))
    ));
    assert!(!disk.media.get());
    assert!(disk.read(0, &mut [0; 512]).is_err());
    assert_eq!(
        take_media_changes(),
        (
            1,
            Some(Sense {
                key: Sense::UNIT_ATTENTION,
                asc: Sense::RESET_OCCURRED,
                ascq: 0
            })
        )
    );
    assert_eq!(take_media_changes(), (0, None));
}
