//! A ring per PE of its recent exception entries, exception returns and
//! native-fault recoveries (`--event-ring`), for the report after a guest
//! panic: what each PE took, returned to and replayed just before it.
//!
//! Only the PE's own machine writes its ring: each record is four relaxed
//! stores and a relaxed bump of its count, which compile to plain moves, so
//! the engine paths that record pay no lock and no locked instruction. A
//! reader on another core may see an entry the owner is overwriting; the
//! dump says nothing stronger than "recent".
//!
//! Native code completes some transitions itself (SVC, ERET and GENTER/GEXIT
//! thunks, `PES TRANSITIONS`): only those that reach the engine are recorded.
//! Interrupts, aborts and native-fault recoveries always reach it.
use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering::Relaxed};

/// Entries each PE keeps.
pub const ENTRIES: usize = 1024;
/// Flagged entries (replay hazards) each PE keeps apart, so ordinary
/// traffic does not push them out.
pub const FLAGGED: usize = 64;
/// PEs the registry holds.
pub const MAX_PES: usize = 32;
const WORDS: usize = 4;

/// What an entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// An exception taken in the engine (`Machine::take`): `pc` is its
    /// ELR, `other` its FAR (or 0), `detail` its ESR, `vector` its offset.
    Exception = 1,
    /// An ERET the engine ran: `pc` is the ERET's own PC, `other` the PC
    /// it returned to, `detail` the SPSR's low word.
    Return = 2,
    /// A native fault settled by recovery: `pc` the settled PC, `other`
    /// the fault address, `detail` the native offset; `vector` holds
    /// [`RECOVERY_WRITE`] and [`RECOVERY_HAZARD`].
    Recovery = 3,
}

/// [`Kind::Recovery`]: the host access that faulted was a write.
pub const RECOVERY_WRITE: u8 = 1;
/// [`Kind::Recovery`]: the settled instruction cannot have made the access
/// that faulted ([`replay_hazard`]): the resume replays guest work.
pub const RECOVERY_HAZARD: u8 = 2;

/// One recorded event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub pe: u8,
    pub from_el: u8,
    pub to_el: u8,
    /// The exception vector's offset / 0x80, or recovery flags.
    pub vector: u8,
    pub detail: u32,
    pub pc: u64,
    pub other: u64,
    /// The PE's retired instruction count.
    pub retired: u64,
}

impl Entry {
    fn pack(&self) -> [u64; WORDS] {
        [
            u64::from(self.kind as u8)
                | u64::from(self.pe) << 8
                | u64::from(self.from_el & 0xf) << 16
                | u64::from(self.to_el & 0xf) << 20
                | u64::from(self.vector) << 24
                | u64::from(self.detail) << 32,
            self.pc,
            self.other,
            self.retired,
        ]
    }

    fn unpack(words: [u64; WORDS]) -> Option<Self> {
        let kind = match words[0] as u8 {
            1 => Kind::Exception,
            2 => Kind::Return,
            3 => Kind::Recovery,
            _ => return None,
        };
        Some(Self {
            kind,
            pe: (words[0] >> 8) as u8,
            from_el: (words[0] >> 16) as u8 & 0xf,
            to_el: (words[0] >> 20) as u8 & 0xf,
            vector: (words[0] >> 24) as u8,
            detail: (words[0] >> 32) as u32,
            pc: words[1],
            other: words[2],
            retired: words[3],
        })
    }

    /// Whether the entry is a recovery that replays guest work.
    pub fn hazard(&self) -> bool {
        self.kind == Kind::Recovery && self.vector & RECOVERY_HAZARD != 0
    }
}

struct Slots {
    count: AtomicU64,
    words: Box<[AtomicU64]>,
}

impl Slots {
    fn new(entries: usize) -> Self {
        Self {
            count: AtomicU64::new(0),
            words: (0..entries * WORDS).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn entries(&self) -> usize {
        self.words.len() / WORDS
    }

    #[inline]
    fn push(&self, words: [u64; WORDS]) {
        let count = self.count.load(Relaxed);
        let base = (count as usize % self.entries()) * WORDS;
        for (slot, word) in self.words[base..base + WORDS].iter().zip(words) {
            slot.store(word, Relaxed);
        }
        self.count.store(count.wrapping_add(1), Relaxed);
    }

    /// The last `limit` entries, oldest first, each with its sequence number.
    fn recent(&self, limit: usize) -> Vec<(u64, Entry)> {
        let count = self.count.load(Relaxed);
        let kept = count.min(self.entries() as u64).min(limit as u64);
        (count - kept..count)
            .filter_map(|sequence| {
                let base = (sequence as usize % self.entries()) * WORDS;
                let words = core::array::from_fn(|word| self.words[base + word].load(Relaxed));
                Entry::unpack(words).map(|entry| (sequence, entry))
            })
            .collect()
    }
}

/// One PE's ring.
pub struct EventRing {
    pe: usize,
    all: Slots,
    flagged: Slots,
}

impl EventRing {
    pub fn new(pe: usize, entries: usize, flagged: usize) -> Self {
        Self {
            pe,
            all: Slots::new(entries.max(1)),
            flagged: Slots::new(flagged.max(1)),
        }
    }

    pub fn pe(&self) -> usize {
        self.pe
    }

    /// Record `entry`; a hazard also goes to the flagged ring.
    #[inline]
    pub fn record(&self, entry: &Entry) {
        let words = entry.pack();
        self.all.push(words);
        if entry.hazard() {
            self.flagged.push(words);
        }
    }

    /// Entries recorded so far, wrapped or not.
    pub fn count(&self) -> u64 {
        self.all.count.load(Relaxed)
    }

    /// The last `limit` entries, oldest first, with their sequence numbers.
    pub fn recent(&self, limit: usize) -> Vec<(u64, Entry)> {
        self.all.recent(limit)
    }

    /// Every flagged entry still kept, oldest first.
    pub fn flagged(&self) -> Vec<(u64, Entry)> {
        self.flagged.recent(usize::MAX)
    }

    /// Flagged entries recorded so far.
    pub fn flagged_count(&self) -> u64 {
        self.flagged.count.load(Relaxed)
    }
}

static RINGS: [AtomicPtr<EventRing>; MAX_PES] =
    [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_PES];

/// PE `pe`'s ring, made and registered the first time: it lives as long as
/// the boot, so any core may read it after a panic. `None` past
/// [`MAX_PES`].
pub fn ring_for(pe: usize) -> Option<&'static EventRing> {
    let slot = RINGS.get(pe)?;
    let existing = slot.load(Relaxed);
    if !existing.is_null() {
        // SAFETY: registered rings are leaked and never freed.
        return Some(unsafe { &*existing });
    }
    let ring = Box::into_raw(Box::new(EventRing::new(pe, ENTRIES, FLAGGED)));
    match slot.compare_exchange(core::ptr::null_mut(), ring, Relaxed, Relaxed) {
        // SAFETY: just leaked into the registry, never freed.
        Ok(_) => Some(unsafe { &*ring }),
        Err(other) => {
            // SAFETY: `ring` lost the race and was never shared.
            drop(unsafe { Box::from_raw(ring) });
            // SAFETY: as above, the winner is leaked for good.
            Some(unsafe { &*other })
        }
    }
}

/// Every registered ring, by PE.
pub fn registered() -> impl Iterator<Item = &'static EventRing> {
    RINGS.iter().filter_map(|slot| {
        let ring = slot.load(Relaxed);
        // SAFETY: registered rings are leaked and never freed.
        (!ring.is_null()).then(|| unsafe { &*ring })
    })
}

fn vector_name(vector: u8) -> &'static str {
    match vector {
        0 => "sync",
        1 => "irq",
        2 => "fiq",
        3 => "serror",
        _ => "?",
    }
}

/// One entry as a report line: `EVENT RING pe=<n> #<sequence> ...`.
pub fn format(sequence: u64, entry: &Entry) -> String {
    let mut line = format!("EVENT RING pe={} #{sequence} ", entry.pe);
    let _ = match entry.kind {
        Kind::Exception => write!(
            line,
            "EXC {} el{}->el{} elr={:#x} esr={:#x} far={:#x}",
            vector_name(entry.vector),
            entry.from_el,
            entry.to_el,
            entry.pc,
            entry.detail,
            entry.other
        ),
        Kind::Return => write!(
            line,
            "ERET el{}->el{} at={:#x} to={:#x} spsr={:#x}",
            entry.from_el, entry.to_el, entry.pc, entry.other, entry.detail
        ),
        Kind::Recovery => write!(
            line,
            "RECOVER{} el{} settled={:#x} fault={:#x} {} native+{:#x}",
            if entry.hazard() { " REPLAY-HAZARD" } else { "" },
            entry.from_el,
            entry.pc,
            entry.other,
            if entry.vector & RECOVERY_WRITE != 0 {
                "write"
            } else {
                "read"
            },
            entry.detail
        ),
    };
    let _ = write!(line, " retired={}", entry.retired);
    line
}

/// The report lines for every registered ring: a header per PE, its
/// flagged entries, then its last `limit` entries.
pub fn report(limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for ring in registered() {
        lines.push(format!(
            "EVENT RING pe={} recorded={} flagged={} showing={}",
            ring.pe(),
            ring.count(),
            ring.flagged_count(),
            ring.count().min(limit as u64).min(ENTRIES as u64)
        ));
        for (sequence, entry) in ring.flagged() {
            lines.push(format!("{} (flagged)", format(sequence, &entry)));
        }
        for (sequence, entry) in ring.recent(limit) {
            lines.push(format(sequence, &entry));
        }
    }
    lines
}

/// What a guest instruction does to memory, as far as a replay check needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEffect {
    /// No memory access at all.
    None,
    /// Reads memory and writes none.
    Reads,
    /// Writes memory (and perhaps reads it).
    Writes,
    /// Not classified: no conclusion.
    Unknown,
}

/// The memory effect of `word`.
pub fn memory_effect(word: u32) -> MemoryEffect {
    use stela_compiler::{
        GuestAddress, Lifter,
        aarch64::{Decoder, MemoryTransfer, Operation, exclusive, ordered, simd},
    };
    let Ok(instruction) = Decoder.lift(GuestAddress(0), word) else {
        return MemoryEffect::Unknown;
    };
    let transfer = |transfer: MemoryTransfer| match transfer {
        MemoryTransfer::Load { .. } => MemoryEffect::Reads,
        MemoryTransfer::Store => MemoryEffect::Writes,
    };
    let loads = |load: bool| {
        if load {
            MemoryEffect::Reads
        } else {
            MemoryEffect::Writes
        }
    };
    // Loads native code may make single-copy atomic with a locked
    // compare-exchange, which faults as a write (an LDP's 16 bytes, an
    // exclusive or acquire load), draw no conclusion.
    match instruction.operation {
        Operation::Memory { transfer: t, .. } => transfer(t),
        Operation::MemoryPair {
            transfer: MemoryTransfer::Store,
            ..
        } => MemoryEffect::Writes,
        Operation::MemoryRegisterOffset(operand) => transfer(operand.transfer),
        Operation::MemoryUnprivileged(operand) => transfer(operand.transfer),
        Operation::MemoryNonTemporal(operand) => loads(operand.load),
        Operation::Atomic(_) => MemoryEffect::Writes,
        Operation::Exclusive(exclusive::Operand::Transfer {
            kind: exclusive::Kind::Store { .. },
            ..
        }) => MemoryEffect::Writes,
        Operation::Ordered(ordered::Operand::Transfer {
            kind: ordered::Kind::Release,
            ..
        }) => MemoryEffect::Writes,
        Operation::Simd(
            simd::Operand::Memory { load, .. } | simd::Operand::MemoryPair { load, .. },
        ) => loads(load),
        Operation::Simd(simd::Operand::MemoryRegisterOffset(operand)) => loads(operand.load),
        Operation::Nop
        | Operation::PcRelative(_)
        | Operation::MoveWide { .. }
        | Operation::AddSub { .. }
        | Operation::AddSubRegister { .. }
        | Operation::LogicalRegister { .. }
        | Operation::LogicalImmediate { .. }
        | Operation::Bitfield(_)
        | Operation::ConditionalSelect(_)
        | Operation::ConditionalCompare(_) => MemoryEffect::None,
        _ => MemoryEffect::Unknown,
    }
}

/// Whether a native fault settled before the instruction `word` replays
/// guest work: a recovery resumes at the start of `word`, so the access that
/// faulted must be one `word` makes. A write fault at an instruction that
/// only reads, or any access fault at one that makes none, belongs to a later
/// instruction of the same native range, whose earlier effects (an
/// `ldrb; add; stlrb` unlock, say) the resume runs again.
pub fn replay_hazard(word: u32, write_fault: bool) -> bool {
    match memory_effect(word) {
        MemoryEffect::None => true,
        MemoryEffect::Reads => write_fault,
        MemoryEffect::Writes | MemoryEffect::Unknown => false,
    }
}

/// The lock address a `Ticket spinlock[0x...]` panic message names.
pub fn ticket_lock_address(message: &str) -> Option<u64> {
    let (_, rest) = message.split_once("Ticket spinlock[0x")?;
    let (hex, _) = rest.split_once(']')?;
    u64::from_str_radix(hex, 16).ok()
}

/// The report line for the 64-byte line at `line_address` holding the
/// ticket lock at `lock`: its bytes, and the lock's fields. The panic prints
/// the `hw_lck_ticket_t` (the `lck_ticket_t` plus 8): its word is type,
/// flags, cticket and nticket, a byte each, and the owner's ctid follows it
/// (`lck_ticket_unlock` clears `[lck, #0xc]` and bumps `[lck, #0xa]`).
pub fn format_lock_line(lock: u64, line_address: u64, bytes: &[u8; 64]) -> String {
    let mut line = format!("PANIC LOCK va={lock:#x} line={line_address:#x} bytes=");
    for byte in bytes {
        let _ = write!(line, "{byte:02x}");
    }
    let offset = (lock - line_address) as usize;
    if offset + 8 <= bytes.len() {
        let word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"));
        let owner =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().expect("4 bytes"));
        let _ = write!(
            line,
            " type={:#x} flags={:#x} cticket={:#x} nticket={:#x} owner_ctid={owner:#x}",
            word & 0xff,
            word >> 8 & 0xff,
            word >> 16 & 0xff,
            word >> 24
        );
    }
    line
}

#[cfg(test)]
mod tests;
