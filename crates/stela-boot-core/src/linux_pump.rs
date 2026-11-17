//! Who runs the host's Linux environment, and when.
//!
//! The ported driver's tasks, timers and interrupt work run only when a
//! processor pumps them. `stela-boot` lets any processor do it, one at a time.
//! A PE whose guest waits for an interrupt holds no guest lock, so its core
//! pumps as soon as work is ready: an event wakes one that sleeps, and a PE
//! that waits for its own posted call runs Linux for it. The boot PE, whose
//! core also serves the host, pumps as a fallback when no PE took the work.
//! It then defers as it always did, so that it does not stop guest code that
//! may hold an XNU spinlock ([`boot_decision`]).
//!
//! Linux runs on one processor at a time: the host drivers' lock covers every
//! Linux entry, and [`Turn`] decides which processor pumps, so a second one
//! does not spin on that lock for work the first is already doing. The turn
//! covers only Linux's steps: a pump gives it back before it looks at the
//! GPUs, whose waits pump again on the same processor. Work raised while a
//! processor holds it calls no other ([`Turn::wake_due`]); the holder takes
//! it, or calls one as it gives the turn back ([`Held::release`]). A clock
//! that more than one processor samples uses [`advance`], which stays
//! monotonic across their counters' small skew.
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// What the boot PE runs at a slice's end, which decides how long the
/// environment's ready work waits for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Calm {
    /// Waiting for an interrupt or at EL0: it holds no guest lock.
    Quiet,
    /// EL1 or EL2 with IRQs unmasked.
    Unmasked,
    /// IRQs masked, or a guarded level: likely inside a spinlock.
    Masked,
}

/// How long ready work may wait for the boot PE, in counter ticks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// With guest interrupts unmasked at EL1 or EL2.
    pub unmasked: u64,
    /// Whatever the boot PE runs.
    pub longest: u64,
}

/// Why the boot PE pumps now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootPump {
    /// It holds no guest lock.
    Quiet,
    /// The work waited past [`Limits::unmasked`] with interrupts unmasked.
    Unmasked,
    /// The work waited past [`Limits::longest`].
    Forced,
}

/// Whether the boot PE, `calm`, pumps work that has waited `waited` ticks:
/// at once when it holds no guest lock, and otherwise only after the
/// limits, so completions and posted calls are never starved. `None` holds
/// the work back.
pub fn boot_decision(calm: Calm, waited: u64, limits: Limits) -> Option<BootPump> {
    if waited >= limits.longest {
        return Some(BootPump::Forced);
    }
    match calm {
        Calm::Quiet => Some(BootPump::Quiet),
        Calm::Unmasked if waited >= limits.unmasked => Some(BootPump::Unmasked),
        Calm::Unmasked | Calm::Masked => None,
    }
}

/// Who takes the pump.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Taker {
    /// The boot PE between its slices, or while it waits in a guest call.
    Boot,
    /// A PE whose guest waits for an interrupt.
    Waiting,
    /// A PE that waits for a call it posted.
    Poster,
}

/// The processor that pumps now: one at a time.
pub struct Turn {
    /// The holder's processor index plus one, zero while free.
    holder: AtomicU32,
}

impl Default for Turn {
    fn default() -> Self {
        Self::new()
    }
}

impl Turn {
    pub const fn new() -> Self {
        Self {
            holder: AtomicU32::new(0),
        }
    }

    /// The turn for `processor`, unless another processor holds it.
    pub fn try_take(&self, processor: u32) -> Option<Held<'_>> {
        let mine = processor.checked_add(1)?;
        self.holder
            .compare_exchange(0, mine, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| Held { turn: self })
    }

    /// The processor holding the turn.
    pub fn holder(&self) -> Option<u32> {
        self.holder.load(Ordering::Relaxed).checked_sub(1)
    }

    /// Whether work just raised needs a processor called for it: only while
    /// no processor holds the turn. A processor called while one does finds
    /// the turn taken and does nothing; the holder takes the work in its
    /// pass, or finds it as it gives the turn back ([`Held::release`]) and
    /// calls one then. Called after the work's flag is stored.
    pub fn wake_due(&self) -> bool {
        // Paired with the fence in `Held::release`: of a raise and a release
        // that race, at least one sees the other's store.
        fence(Ordering::SeqCst);
        self.holder.load(Ordering::Relaxed) == 0
    }
}

/// A held [`Turn`], given back when dropped.
pub struct Held<'a> {
    turn: &'a Turn,
}

impl Held<'_> {
    /// Gives the turn back, then answers `pending()`: whether work is still
    /// raised, which a raiser that found the turn held called no processor
    /// for ([`Turn::wake_due`]), so the caller calls one.
    pub fn release(self, pending: impl FnOnce() -> bool) -> bool {
        drop(self);
        fence(Ordering::SeqCst);
        pending()
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.turn.holder.store(0, Ordering::Release);
    }
}

/// One wake of a sleeping PE for each episode of ready work: the boot PE,
/// finding work ready at a slice's end that it holds back, wakes a sleeper
/// for it once, not at every recheck.
pub struct Wakes {
    /// The start of the ready episode a sleeper was last woken for.
    woken_for: AtomicU64,
}

impl Default for Wakes {
    fn default() -> Self {
        Self::new()
    }
}

impl Wakes {
    pub const fn new() -> Self {
        Self {
            woken_for: AtomicU64::new(0),
        }
    }

    /// Whether to wake a sleeper for the work ready since `since`, which no
    /// wake was for yet.
    pub fn first_for(&self, since: u64) -> bool {
        since != 0 && self.woken_for.swap(since, Ordering::AcqRel) != since
    }
}

/// The next state of a counter clock sampled by more than one processor:
/// `previous`, the last raw value taken, and `elapsed`, the ticks counted so
/// far, after a read of `raw`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sample {
    /// The counter moved on: take `raw` and count `delta` more ticks.
    Moved { delta: u64 },
    /// Not moved since `previous`: the caller waits for it to move.
    Same,
    /// Read up to `skew` ticks behind `previous`, as another processor's
    /// counter may be: one tick more is counted and `previous` kept, so the
    /// clock stays strictly monotonic and never runs ahead of the counter
    /// by more than that skew.
    Behind,
    /// Further behind, or a jump of more than half the range: corrupt.
    Corrupt,
}

/// How a read of `raw` after `previous` advances the clock, allowing reads
/// up to `skew` ticks behind ([`Sample`]).
pub const fn advance(previous: u64, raw: u64, skew: u64) -> Sample {
    let delta = raw.wrapping_sub(previous);
    if delta == 0 {
        Sample::Same
    } else if delta <= u64::MAX / 2 {
        Sample::Moved { delta }
    } else if previous.wrapping_sub(raw) <= skew {
        Sample::Behind
    } else {
        Sample::Corrupt
    }
}

#[cfg(test)]
mod tests;
