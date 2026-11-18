//! `GPU PERF`: the GPU's performance state as GSP-RM reports it. nouveau
//! asks RM every second for the current P-state
//! (`nvkm/subdev/gsp/rm/r535/perf.c`) and adds it to a report this module
//! takes once a beat. The line comes at the first beat after GSP starts and
//! then only when the state changed, so an idle desktop stays quiet. If RM
//! refused the control, every line names its errno, as the state is unknown.
//! Each round also reads the GPU's PCIe link, which RM moves with the
//! P-state, so the line shows whether the link follows load too. Clocks are
//! not read: GSP-RM 570.144's headers define no clock control, and RM refused
//! the 615.78.08 one.
//!
//! **Now** ([`Report::current`]): the P-state and link as last sampled,
//! without taking anything, for lines that time the GPU's own work (NVENC's
//! self-test frames and session reports): `pstate=P8 pcie=gen1x16`.
//!
//! **Boost** ([`Report::request_boost`], `--gpu-boost`): the rounds ask RM
//! to hold the GPU at its highest performance level
//! (`NV2080_CTRL_CMD_PERF_BOOST`) while the host asks, and the lines say
//! RM's answer: `boost=max`, `boost=cleared` or `boost=refused(error E)`.
use alloc::{format, string::String};
use core::{
    fmt,
    sync::atomic::{AtomicI32, AtomicU32, Ordering},
};

/// `include/nvkm/core/counts.h`: `struct nvkm_perf_report`, which nouveau
/// updates only atomically.
#[repr(C)]
pub struct Report {
    started: AtomicU32,
    rounds: AtomicU32,
    pstate: AtomicU32,
    pstates: AtomicU32,
    pstate_status: AtomicI32,
    link_status: AtomicU32,
    link_control: AtomicU32,
    link_speeds: AtomicU32,
    link_cap: AtomicU32,
    /// Set by the host: nouveau's rounds ask RM for the boost while it is
    /// nonzero.
    boost: AtomicU32,
    /// RM's answer to the boost ([`Boost`]).
    boost_status: AtomicI32,
}

impl Report {
    /// As nouveau defines it: nothing sampled yet.
    pub const fn new() -> Self {
        Self {
            started: AtomicU32::new(0),
            rounds: AtomicU32::new(0),
            pstate: AtomicU32::new(0),
            pstates: AtomicU32::new(0),
            pstate_status: AtomicI32::new(0),
            link_status: AtomicU32::new(0),
            link_control: AtomicU32::new(0),
            link_speeds: AtomicU32::new(0),
            link_cap: AtomicU32::new(0),
            boost: AtomicU32::new(0),
            boost_status: AtomicI32::new(0),
        }
    }

    /// Has nouveau's next rounds ask RM to hold the GPU at its highest
    /// performance level (`on`), or to let go.
    pub fn request_boost(&self, on: bool) {
        self.boost.store(u32::from(on), Ordering::Relaxed);
    }

    /// The P-state, link and boost as nouveau last sampled them, taking
    /// nothing; `None` before GSP started.
    pub fn current(&self) -> Option<Current> {
        if self.started.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(Current {
            pstate: self.pstate.load(Ordering::Relaxed),
            status: self.pstate_status.load(Ordering::Relaxed),
            link_status: self.link_status.load(Ordering::Relaxed) as u16,
            boost: Boost(self.boost_status.load(Ordering::Relaxed)),
        })
    }

    /// What nouveau sampled since the last take, cleared; `None` before GSP
    /// started or when no round ran since.
    pub fn take(&self) -> Option<Taken> {
        if self.started.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let rounds = self.rounds.swap(0, Ordering::Relaxed);
        let pstates = self.pstates.swap(0, Ordering::Relaxed);
        let link_speeds = self.link_speeds.swap(0, Ordering::Relaxed);
        (rounds != 0).then(|| Taken {
            rounds,
            pstate: self.pstate.load(Ordering::Relaxed),
            pstates,
            status: self.pstate_status.load(Ordering::Relaxed),
            link: Link {
                status: self.link_status.load(Ordering::Relaxed) as u16,
                control: self.link_control.load(Ordering::Relaxed) as u16,
                speeds: link_speeds,
                cap: self.link_cap.load(Ordering::Relaxed) as u16,
            },
            boost: Boost(self.boost_status.load(Ordering::Relaxed)),
        })
    }
}

/// RM's answer to the boost, as nouveau keeps it: 0 before anything was
/// asked, 1 while RM holds it, 2 once it let go, else the errno of RM's
/// refusal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Boost(pub i32);

/// ` boost=max`, ` boost=cleared`, ` boost=refused(error E)`, or nothing
/// before anything was asked.
impl fmt::Display for Boost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => Ok(()),
            1 => f.write_str(" boost=max"),
            2 => f.write_str(" boost=cleared"),
            error => write!(f, " boost=refused(error {error})"),
        }
    }
}

/// The GPU's state as nouveau last sampled it ([`Report::current`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Current {
    /// The current P-state's bit, zero while unknown.
    pub pstate: u32,
    /// The errno of the P-state control RM refused, else zero.
    pub status: i32,
    /// Link Status as last read, zero when nothing was.
    pub link_status: u16,
    pub boost: Boost,
}

/// `pstate=P8 pcie=gen1x16`, `pstate=unknown pcie=unknown` before a round
/// read either, or `pstate=unknown(error E) …` once RM refused the control;
/// then the boost, once asked.
impl fmt::Display for Current {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.status, self.pstate) {
            (0, 0) => f.write_str("pstate=unknown")?,
            (0, pstate) => write!(f, "pstate={}", Pstates(pstate))?,
            (error, _) => write!(f, "pstate=unknown(error {error})")?,
        }
        let link = Link {
            status: self.link_status,
            ..Link::default()
        };
        if link.read() {
            write!(f, " pcie={}", Lane(link.speed(), link.width()))?;
        } else {
            f.write_str(" pcie=unknown")?;
        }
        write!(f, "{}", self.boost)
    }
}

impl Default for Report {
    fn default() -> Self {
        Self::new()
    }
}

/// One beat's samples.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Taken {
    /// Rounds that asked RM.
    pub rounds: u32,
    /// The current P-state's bit (`NV2080_CTRL_PERF_PSTATES_P0` is bit 0),
    /// zero while unknown.
    pub pstate: u32,
    /// Every P-state bit a round saw.
    pub pstates: u32,
    /// The errno of the control RM refused, else zero.
    pub status: i32,
    /// The GPU's PCIe link.
    pub link: Link,
    /// RM's answer to the boost.
    pub boost: Boost,
}

impl Taken {
    /// What a change is judged by: the P-states, the status, the link and
    /// the boost, not the count of rounds.
    fn state(&self) -> [u64; 3] {
        let link = &self.link;
        [
            u64::from(self.pstate) << 32 | u64::from(self.pstates),
            u64::from(self.status as u32) << 32
                | u64::from(link.speed()) << 24
                | u64::from(link.width()) << 16
                | u64::from(link.aspm()) << 14
                | u64::from(link.speeds & 0x3ff),
            u64::from(self.boost.0 as u32),
        ]
    }
}

/// The GPU's PCIe link as its capability reported it (PCIe Base 6.0,
/// 7.5.3.6-7.5.3.8), all zero when nothing was read.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Link {
    /// Link Status as last read.
    pub status: u16,
    /// Link Control as last read.
    pub control: u16,
    /// A bit per Link Status speed code seen; bit 1 is 2.5 GT/s.
    pub speeds: u32,
    /// Link Capabilities' low word: maximum speed and width.
    pub cap: u16,
}

impl Link {
    /// Current Link Speed: 1 for 2.5 GT/s (gen1) to 6 for 64 GT/s (gen6).
    fn speed(&self) -> u16 {
        self.status & 0xf
    }

    /// Negotiated Link Width.
    fn width(&self) -> u16 {
        self.status >> 4 & 0x3f
    }

    /// ASPM Control: bit 0 L0s, bit 1 L1.
    fn aspm(&self) -> u16 {
        self.control & 3
    }

    fn read(&self) -> bool {
        self.status != 0
    }
}

/// `gen5x16` for a speed code and a width.
struct Lane(u16, u16);

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gen{}x{}", self.0, self.1)
    }
}

/// `pcie=gen5x16 pcie_seen=gen1+gen5 pcie_max=gen5x16 aspm=L1`, or
/// `pcie=unknown` when nothing was read.
impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.read() {
            return f.write_str("pcie=unknown");
        }
        write!(f, "pcie={} pcie_seen=", Lane(self.speed(), self.width()))?;
        let mut first = true;
        for speed in (1..16).filter(|speed| self.speeds & 1 << speed != 0) {
            if !first {
                f.write_str("+")?;
            }
            first = false;
            write!(f, "gen{speed}")?;
        }
        if first {
            f.write_str("-")?;
        }
        write!(
            f,
            " pcie_max={} aspm={}",
            Lane(self.cap & 0xf, self.cap >> 4 & 0x3f),
            ["off", "L0s", "L1", "L0s+L1"][usize::from(self.aspm())]
        )
    }
}

/// `P0+P8` for a set of P-state bits.
struct Pstates(u32);

impl fmt::Display for Pstates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for index in (0..16).filter(|index| self.0 & 1 << index != 0) {
            if !first {
                f.write_str("+")?;
            }
            first = false;
            write!(f, "P{index}")?;
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

/// `pstate=P8 seen=P5+P8 <link> rounds=N`, or
/// `pstate=unknown(error E) <link> rounds=N`, then the boost once asked.
impl fmt::Display for Taken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            0 => write!(
                f,
                "pstate={} seen={}",
                Pstates(self.pstate),
                Pstates(self.pstates)
            )?,
            error => write!(f, "pstate=unknown(error {error})")?,
        }
        write!(f, " {} rounds={}{}", self.link, self.rounds, self.boost)
    }
}

/// The state the last line reported.
pub struct Reported {
    last: spin_cell::Cell,
}

impl Reported {
    pub const fn new() -> Self {
        Self {
            last: spin_cell::Cell::new(),
        }
    }

    /// `GPU PERF …` for `taken` when it is the first or its state changed
    /// since the last line.
    pub fn line(&self, taken: &Taken) -> Option<String> {
        self.last
            .replace(taken.state())
            .is_none_or(|last| last != taken.state())
            .then(|| format!("GPU PERF {taken}"))
    }
}

impl Default for Reported {
    fn default() -> Self {
        Self::new()
    }
}

/// The last reported state behind a spin lock: a beat's report is the only
/// writer, so it never waits in practice.
mod spin_cell {
    use core::{
        cell::UnsafeCell,
        sync::atomic::{AtomicBool, Ordering},
    };

    pub struct Cell {
        held: AtomicBool,
        value: UnsafeCell<Option<[u64; 3]>>,
    }

    // SAFETY: `replace` holds `held` for every access to the value.
    unsafe impl Sync for Cell {}

    impl Cell {
        pub const fn new() -> Self {
            Self {
                held: AtomicBool::new(false),
                value: UnsafeCell::new(None),
            }
        }

        /// Stores `value`, giving the one before.
        pub fn replace(&self, value: [u64; 3]) -> Option<[u64; 3]> {
            while self
                .held
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
            // SAFETY: `held` excludes every other access until released.
            let last = unsafe { (*self.value.get()).replace(value) };
            self.held.store(false, Ordering::Release);
            last
        }
    }
}

#[cfg(test)]
mod tests;
