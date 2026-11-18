//! Where a host key press or pointer motion spends its time on its way to
//! the guest's screen, on the host's clock (`INPUT LATENCY`).
//!
//! One input of each kind ([`Kind`]) is followed at a time, from the host
//! poll that read it to the screen update that can show it:
//!
//! - **read**: the host's driver saw the device's transfer complete, at a
//!   poll that came `poll_gap` after the device's previous one (the report
//!   reached the host controller somewhere in that gap);
//! - **queued**: the native peripheral queued the report it made of it,
//!   under the devices' lock;
//! - **taken**: the guest's xHCI took that report from the peripheral, at
//!   its endpoint's poll;
//! - **ring**: the transfer event went onto the guest's event ring
//!   (interrupter `n`, at TRB address `at`);
//! - **acked**: the guest moved that interrupter's dequeue pointer (ERDP)
//!   past the event: its driver handled it;
//! - **latched**: the display model latched a frame after that, at a
//!   vertical blank;
//! - **shown**: the host showed the screen update holding that frame (a KMS
//!   flip, or the firmware framebuffer's frame).
//!
//! An input of a kind is followed only while none of its kind is, so a
//! window's samples are a sample of the inputs, not all of them. One that
//! does not move on for [`STALE_MILLIS`] is given up and counted where it
//! stopped (`stuck=`): a key that changes nothing on screen stops at
//! `acked` or `latched`. Every stage is a few atomic loads where no input
//! is followed, so the stages stay on.
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use stela_drivers::input::Input;

/// The kinds of input followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A press of a key that is not a modifier: what typing is.
    Keyboard,
    /// A pointer's position or a mouse's motion.
    Pointer,
}

impl Kind {
    pub const ALL: [Kind; 2] = [Kind::Keyboard, Kind::Pointer];

    const fn index(self) -> usize {
        self as usize
    }

    pub const fn name(self) -> &'static str {
        match self {
            Kind::Keyboard => "keyboard",
            Kind::Pointer => "pointer",
        }
    }

    /// The kind `input` is followed as, if any: a modifier or a release
    /// draws nothing new, and a button or wheel is not what typing or
    /// pointing waits on.
    pub fn of(input: &Input) -> Option<Kind> {
        match *input {
            Input::Key {
                usage: 0x04..=0xdd,
                pressed: true,
            } => Some(Kind::Keyboard),
            Input::Move { .. } | Input::Motion { .. } => Some(Kind::Pointer),
            _ => None,
        }
    }
}

/// Where a followed input is ([`Kind`]'s stages).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Read,
    Queued,
    Taken,
    Ring,
    Acked,
    Latched,
}

const STAGES: usize = 6;
const STAGE_NAMES: [&str; STAGES] = ["read", "queued", "taken", "ring", "acked", "latched"];

/// The stamps a followed input takes, by [`Stage`], then `shown`.
const SHOWN: usize = STAGES;
const STAMPS: usize = STAGES + 1;

/// How long a followed input may stay at one stage before it is given up.
pub const STALE_MILLIS: u64 = 1000;

/// The completed samples a report window keeps of each kind; later ones
/// are counted (`overflow=`).
const WINDOW: usize = 128;

/// One input being followed.
#[derive(Clone, Copy)]
struct Flight {
    stage: Stage,
    stamps: [u64; STAMPS],
    poll_gap: u64,
    /// Where its transfer event went: the interrupter, the event's TRB
    /// address and the interrupter's dequeue pointer then.
    interrupter: usize,
    at: u64,
    before: u64,
}

/// One input followed to the screen.
#[derive(Clone, Copy, Default)]
struct Sample {
    stamps: [u64; STAMPS],
    poll_gap: u64,
}

impl Sample {
    fn between(&self, from: usize, to: usize) -> u64 {
        self.stamps[to].saturating_sub(self.stamps[from])
    }
}

/// One kind's samples since the last report.
#[derive(Clone, Copy)]
struct Window {
    samples: [Sample; WINDOW],
    count: usize,
    overflow: u64,
    /// Inputs given up, by the stage they stopped at.
    stuck: [u64; STAGES],
    /// Inputs that made no report: a key already held, or a peripheral the
    /// guest has not configured.
    dropped: u64,
}

impl Window {
    const EMPTY: Self = Self {
        samples: [Sample {
            stamps: [0; STAMPS],
            poll_gap: 0,
        }; WINDOW],
        count: 0,
        overflow: 0,
        stuck: [0; STAGES],
        dropped: 0,
    };
}

struct Tracker {
    flights: [Option<Flight>; 2],
    windows: [Window; 2],
    /// The kind whose report the guest's controller just took: its
    /// transfer event is the next one posted ([`tag_event`]).
    tag: Option<Kind>,
}

/// The tracker, which one core at a time holds ([`with`]).
struct Shared {
    held: AtomicBool,
    tracker: UnsafeCell<Tracker>,
}

// SAFETY: `held` admits one user of `tracker` at a time ([`with`]).
unsafe impl Sync for Shared {}

static TRACKER: Shared = Shared {
    held: AtomicBool::new(false),
    tracker: UnsafeCell::new(Tracker {
        flights: [None; 2],
        windows: [Window::EMPTY; 2],
        tag: None,
    }),
};

/// Which kinds sit at which stage, by bit `kind * 8 + stage`, and
/// [`TAG_BIT`] while a transfer event waits for its tag: what each stage's
/// hook reads before it takes the tracker, so a hook with nothing to do
/// costs one load.
static ARMED: AtomicU32 = AtomicU32::new(0);
const TAG_BIT: u32 = 1 << 31;

const fn bit(kind: Kind, stage: Stage) -> u32 {
    1 << (kind.index() * 8 + stage as usize)
}

fn armed(mask: u32) -> bool {
    ARMED.load(Ordering::Acquire) & mask != 0
}

/// The host clock every stage is stamped on, as a `fn() -> u64` (none
/// until [`set_clock`]), and its ticks per second.
static CLOCK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static HZ: AtomicU64 = AtomicU64::new(0);

/// Stamp the stages on `clock`, which runs at `hz` and reads alike on every
/// core. Until then nothing is followed.
pub fn set_clock(clock: fn() -> u64, hz: u64) {
    HZ.store(hz, Ordering::Relaxed);
    CLOCK.store(clock as *mut (), Ordering::Release);
    #[cfg(test)]
    ACTIVE.with(|active| active.set(true));
}

/// Whether the hooks act on this thread: always, but in a test build only
/// on a thread that set the clock, as the other tests run alongside and
/// would move the one tracker's inputs on.
#[cfg(not(test))]
const fn active() -> bool {
    true
}

#[cfg(test)]
std::thread_local! {
    static ACTIVE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[cfg(test)]
fn active() -> bool {
    ACTIVE.with(core::cell::Cell::get)
}

fn now() -> Option<u64> {
    if !active() {
        return None;
    }
    let clock = CLOCK.load(Ordering::Acquire);
    if clock.is_null() {
        return None;
    }
    // SAFETY: only `set_clock` stores here, and only a `fn() -> u64`.
    let clock = unsafe { core::mem::transmute::<*mut (), fn() -> u64>(clock) };
    Some(clock())
}

/// The clock's ticks per second, zero before [`set_clock`].
pub fn hz() -> u64 {
    HZ.load(Ordering::Relaxed)
}

/// Run `f` on the tracker, after any other core's use of it: every use is
/// a few stores, never a wait on anything else.
fn with<R>(f: impl FnOnce(&mut Tracker) -> R) -> R {
    while TRACKER
        .held
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: `held` excludes every other reference until it is released.
    let tracker = unsafe { &mut *TRACKER.tracker.get() };
    let result = f(tracker);
    ARMED.store(tracker.armed(), Ordering::Release);
    TRACKER.held.store(false, Ordering::Release);
    result
}

impl Tracker {
    fn armed(&self) -> u32 {
        let mut armed = if self.tag.is_some() { TAG_BIT } else { 0 };
        for kind in Kind::ALL {
            if let Some(flight) = &self.flights[kind.index()] {
                armed |= bit(kind, flight.stage);
            }
        }
        armed
    }

    /// Move `kind` from stage `from` to `to` at `now`; false when it is not
    /// at `from`.
    fn advance(&mut self, kind: Kind, from: Stage, to: Stage, now: u64) -> bool {
        match &mut self.flights[kind.index()] {
            Some(flight) if flight.stage == from => {
                flight.stage = to;
                flight.stamps[to as usize] = now;
                true
            }
            _ => false,
        }
    }

    fn finish(&mut self, kind: Kind, now: u64) {
        let Some(mut flight) = self.flights[kind.index()].take() else {
            return;
        };
        flight.stamps[SHOWN] = now;
        let window = &mut self.windows[kind.index()];
        if window.count == WINDOW {
            window.overflow += 1;
        } else {
            window.samples[window.count] = Sample {
                stamps: flight.stamps,
                poll_gap: flight.poll_gap,
            };
            window.count += 1;
        }
    }

    /// Give up `kind`'s input if it stayed at its stage past
    /// [`STALE_MILLIS`] by `now`; whether none is followed afterwards.
    fn expire(&mut self, kind: Kind, now: u64, hz: u64) -> bool {
        let Some(flight) = &self.flights[kind.index()] else {
            return true;
        };
        let at = flight.stamps[flight.stage as usize];
        if now.saturating_sub(at) < hz / 1000 * STALE_MILLIS {
            return false;
        }
        self.windows[kind.index()].stuck[flight.stage as usize] += 1;
        if self.tag == Some(kind) {
            self.tag = None;
        }
        self.flights[kind.index()] = None;
        true
    }
}

/// Follow a `kind` input the host's driver read at `read_at`, a poll that
/// came `poll_gap` ticks after the device's previous one, unless one of
/// its kind is followed already. Called just before the input reaches the
/// native peripheral ([`delivering`]).
pub fn begin(kind: Kind, read_at: u64, poll_gap: u64) -> bool {
    let Some(now) = now() else {
        return false;
    };
    let hz = hz();
    with(|tracker| {
        if !tracker.expire(kind, now, hz) {
            return false;
        }
        let mut stamps = [0; STAMPS];
        stamps[Stage::Read as usize] = read_at;
        tracker.flights[kind.index()] = Some(Flight {
            stage: Stage::Read,
            stamps,
            poll_gap,
            interrupter: 0,
            at: 0,
            before: 0,
        });
        true
    })
}

/// Whether the input the native peripheral is about to apply is the
/// followed one of `kind`.
pub fn delivering(kind: Kind) -> bool {
    armed(bit(kind, Stage::Read)) && active()
}

/// The peripheral queued the followed `kind` input's report.
pub fn queued(kind: Kind) {
    let Some(now) = now() else { return };
    with(|tracker| tracker.advance(kind, Stage::Read, Stage::Queued, now));
}

/// The followed `kind` input made no report; it is counted (`dropped=`).
pub fn dropped(kind: Kind) {
    if !active() {
        return;
    }
    with(|tracker| {
        if tracker.flights[kind.index()].is_some_and(|f| f.stage == Stage::Read) {
            tracker.flights[kind.index()] = None;
            tracker.windows[kind.index()].dropped += 1;
        }
    });
}

/// The peripheral's queue of the followed `kind` input's report was
/// emptied (a reset or a new configuration): the input is not followed.
pub fn abandon(kind: Kind) {
    if !armed(bit(kind, Stage::Queued)) || !active() {
        return;
    }
    with(|tracker| {
        if tracker.flights[kind.index()].is_some_and(|f| f.stage == Stage::Queued) {
            tracker.flights[kind.index()] = None;
        }
    });
}

/// The guest's controller took the followed `kind` input's report: the
/// next transfer event it posts is that report's ([`tag_event`]).
pub fn taken(kind: Kind) {
    let Some(now) = now() else { return };
    with(|tracker| {
        if tracker.advance(kind, Stage::Queued, Stage::Taken, now) {
            tracker.tag = Some(kind);
        }
    });
}

/// The kind of the followed input whose transfer event the guest's
/// controller is posting, if this event is one.
pub fn tag_event() -> Option<Kind> {
    if !armed(TAG_BIT) || !active() {
        return None;
    }
    with(|tracker| tracker.tag.take())
}

/// The followed `kind` input's transfer event went onto interrupter
/// `interrupter`'s event ring at TRB address `at`, whose dequeue pointer
/// was `before`.
pub fn on_ring(kind: Kind, interrupter: usize, at: u64, before: u64) {
    let Some(now) = now() else { return };
    with(|tracker| {
        if tracker.advance(kind, Stage::Taken, Stage::Ring, now)
            && let Some(flight) = &mut tracker.flights[kind.index()]
        {
            flight.interrupter = interrupter;
            flight.at = at;
            flight.before = before;
        }
    });
}

/// Whether dequeue pointer `dequeue` lies on the stretch from `before` to
/// `at` that the guest has still to read, both ends included: the guest
/// has not yet handled the event at `at`. A ring that wrapped between the
/// two is the stretch around its end.
fn unread(before: u64, at: u64, dequeue: u64) -> bool {
    if before <= at {
        (before..=at).contains(&dequeue)
    } else {
        dequeue >= before || dequeue <= at
    }
}

/// The guest wrote interrupter `interrupter`'s ERDP, its dequeue pointer
/// now `dequeue`: a followed event it moved past was handled.
pub fn erdp(interrupter: usize, dequeue: u64) {
    if !armed(bit(Kind::Keyboard, Stage::Ring) | bit(Kind::Pointer, Stage::Ring)) {
        return;
    }
    let Some(now) = now() else { return };
    with(|tracker| {
        for kind in Kind::ALL {
            let handled = tracker.flights[kind.index()].is_some_and(|flight| {
                flight.stage == Stage::Ring
                    && flight.interrupter == interrupter
                    && !unread(flight.before, flight.at, dequeue)
            });
            if handled {
                tracker.advance(kind, Stage::Ring, Stage::Acked, now);
            }
        }
    });
}

/// The display model latched a frame.
pub fn latched() {
    if !armed(bit(Kind::Keyboard, Stage::Acked) | bit(Kind::Pointer, Stage::Acked)) {
        return;
    }
    let Some(now) = now() else { return };
    with(|tracker| {
        for kind in Kind::ALL {
            tracker.advance(kind, Stage::Acked, Stage::Latched, now);
        }
    });
}

/// The host showed a screen update: a frame latched before it is on screen.
pub fn shown() {
    if !armed(bit(Kind::Keyboard, Stage::Latched) | bit(Kind::Pointer, Stage::Latched)) {
        return;
    }
    let Some(now) = now() else { return };
    with(|tracker| {
        for kind in Kind::ALL {
            if tracker.flights[kind.index()].is_some_and(|f| f.stage == Stage::Latched) {
                tracker.finish(kind, now);
            }
        }
    });
}

/// p50, p90 and the largest of `values`, which this sorts; zeros for none.
pub fn percentiles(values: &mut [u64]) -> [u64; 3] {
    if values.is_empty() {
        return [0; 3];
    }
    values.sort_unstable();
    let at = |percent: usize| values[(values.len() - 1) * percent / 100];
    [at(50), at(90), values[values.len() - 1]]
}

/// One kind's report window: what [`take`] returns.
pub struct Report {
    kind: Kind,
    hz: u64,
    window: Window,
}

impl Report {
    /// Inputs followed to the screen in the window.
    pub fn samples(&self) -> usize {
        self.window.count
    }

    /// Whether the window holds anything to report.
    pub fn is_empty(&self) -> bool {
        self.window.count == 0
            && self.window.dropped == 0
            && self.window.overflow == 0
            && self.window.stuck.iter().all(|&count| count == 0)
    }

    /// ` name=p50/p90/max` of each sample's `span`, in microseconds.
    fn stage(
        &self,
        f: &mut impl fmt::Write,
        name: &str,
        span: impl Fn(&Sample) -> u64,
    ) -> fmt::Result {
        let mut ticks = [0u64; WINDOW];
        let samples = &self.window.samples[..self.window.count];
        for (tick, sample) in ticks.iter_mut().zip(samples) {
            *tick = span(sample);
        }
        let [p50, p90, max] = percentiles(&mut ticks[..samples.len()]);
        let us = |ticks: u64| u128::from(ticks) * 1_000_000 / u128::from(self.hz.max(1));
        write!(f, " {name}={}/{}/{}", us(p50), us(p90), us(max))
    }
}

/// `INPUT LATENCY kind n=N poll_gap_us=… report_to_ring_us=…
/// ring_to_ack_us=… ack_to_present_us=… total_ms=… deliver_us=…
/// guest_poll_us=… ack_to_latch_us=… latch_to_flip_us=… dropped=N
/// overflow=N stuck=read:N,…`, each span as p50/p90/max over the window's
/// samples.
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let read = Stage::Read as usize;
        let ring = Stage::Ring as usize;
        let acked = Stage::Acked as usize;
        write!(
            f,
            "INPUT LATENCY {} n={}",
            self.kind.name(),
            self.window.count
        )?;
        self.stage(f, "poll_gap_us", |s| s.poll_gap)?;
        self.stage(f, "report_to_ring_us", |s| s.between(read, ring))?;
        self.stage(f, "ring_to_ack_us", |s| s.between(ring, acked))?;
        self.stage(f, "ack_to_present_us", |s| s.between(acked, SHOWN))?;
        let mut total = [0u64; WINDOW];
        let samples = &self.window.samples[..self.window.count];
        for (tick, sample) in total.iter_mut().zip(samples) {
            *tick = sample.between(read, SHOWN);
        }
        let [p50, p90, max] = percentiles(&mut total[..samples.len()]);
        let tenths = |ticks: u64| u128::from(ticks) * 10_000 / u128::from(self.hz.max(1));
        let ms = |ticks: u64| {
            let tenths = tenths(ticks);
            (tenths / 10, tenths % 10)
        };
        let (a, b, c) = (ms(p50), ms(p90), ms(max));
        write!(
            f,
            " total_ms={}.{}/{}.{}/{}.{}",
            a.0, a.1, b.0, b.1, c.0, c.1
        )?;
        self.stage(f, "deliver_us", |s| s.between(read, Stage::Queued as usize))?;
        self.stage(f, "guest_poll_us", |s| {
            s.between(Stage::Queued as usize, Stage::Taken as usize)
        })?;
        self.stage(f, "ack_to_latch_us", |s| {
            s.between(acked, Stage::Latched as usize)
        })?;
        self.stage(f, "latch_to_flip_us", |s| {
            s.between(Stage::Latched as usize, SHOWN)
        })?;
        write!(
            f,
            " dropped={} overflow={} stuck=",
            self.window.dropped, self.window.overflow
        )?;
        for (index, (name, count)) in STAGE_NAMES.iter().zip(self.window.stuck).enumerate() {
            let comma = if index == 0 { "" } else { "," };
            write!(f, "{comma}{name}:{count}")?;
        }
        Ok(())
    }
}

/// `kind`'s window since the last call, a new one starting; an input
/// followed past [`STALE_MILLIS`] is given up first.
pub fn take(kind: Kind) -> Report {
    let hz = hz();
    let now = now();
    let window = with(|tracker| {
        if let Some(now) = now {
            tracker.expire(kind, now, hz);
        }
        core::mem::replace(&mut tracker.windows[kind.index()], Window::EMPTY)
    });
    Report { kind, hz, window }
}

/// Forget everything followed: for tests, which share the one tracker.
#[cfg(test)]
fn reset() {
    with(|tracker| {
        tracker.flights = [None; 2];
        tracker.windows = [Window::EMPTY; 2];
        tracker.tag = None;
    });
}

#[cfg(test)]
mod tests;
