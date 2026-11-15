//! Rules for the host's Linux environment when more than one processor runs
//! it: one at a time, under the host drivers' lock. A clock that more than
//! one processor samples uses [`advance`], which stays monotonic across
//! their counters' small skew.

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
