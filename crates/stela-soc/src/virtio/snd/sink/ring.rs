//! The virtio stream on the host's guest-audio output: [`GuestAudioRing`]
//! pushes the guest's frames into a [`GuestRing`] that the boot's
//! guest-audio service takes from as it takes the USB speaker's, and its
//! clock ([`FollowClock`]) follows the frames that output consumed.
//!
//! **The frames.** The ring holds stereo frames as the USB speaker's ring
//! does: each sample MSB-aligned in 32 bits ([`ring_frames`]: S16 is the top
//! half, S32 as is). Silence the device's clock passed with none of the
//! guest's frames (an underrun) is pushed as zeros, so the ring keeps the
//! stream's timeline and the output's backlog its level. A full ring
//! ([`RING_FRAMES`], 200 ms) drops what does not fit, counted.
//!
//! **The clock.** The device completes the guest's transfers at this
//! clock, so the guest's audio engine runs at its rate.
//! - [`ClockSource::DeviceTicks`]: the stream's rate of the host's counter,
//!   as the counting sink's. It starts so, and runs so whenever the output
//!   does not publish.
//! - [`ClockSource::HdmiFrames`]: the output publishes the frames it
//!   consumed, in whole HDMI periods written into the HDA ring, and the host
//!   tick it did ([`GuestRing::publish`]). The clock keeps its position at
//!   the handoff and from then advances as the output does: each new
//!   publication compares where the output says the clock should be with
//!   where it is, and slews the clock's rate (at most
//!   [`MAX_SLEW_PPM`]) to close the gap over [`SLEW_HORIZON_FRAMES`]. So the
//!   guest is paced by the HDA link's 48 kHz, and the frames it sends are
//!   the frames the output takes: the backlog holds its level with no
//!   drift to pad or drop. The publication also carries the backlog, and a
//!   slow trim on it ([`LEVEL_STEP_FRAMES`]) brings a backlog that a pause
//!   left at its cap back to [`BACKLOG_TARGET_FRAMES`].
//! - **Fallback.** An output that stops (`publish_stopped`: a route
//!   paused, a sink asleep, a stream restarting, a route lost) or goes
//!   quiet for [`STALE_MILLIS`] (a service turn that cannot reach the HDA)
//!   hands the clock back to the host's counter at the position it reached:
//!   the guest never waits on the output. The next publication of a running
//!   output takes it again from there.
//! - The clock never goes back: a handoff or a new rate starts from the
//!   position reached. A gap past [`RESYNC_FRAMES`] (an output that jumped)
//!   takes the output's count from where the clock is instead.
use super::{Format, Meter, Played, Sink};
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};
use stela_contracts::virtio_snd::codes::RATE_48000;

/// Frames the ring holds: 200 ms at 48 kHz, as the USB speaker's.
pub const RING_FRAMES: usize = 48_000 / 5;
/// Frames pushed since the service last took any that ask for a service
/// turn: 5 ms, as the USB speaker's.
const DUE_FRAMES: u64 = 48_000 / 200;
/// How long the output may go without publishing before the clock falls
/// back to the host's counter: twice the service's starvation bound.
pub const STALE_MILLIS: u64 = 100;
/// The gap between the output's position and the clock's that the slew
/// closes in about this many frames: 5 s.
pub const SLEW_HORIZON_FRAMES: i64 = 5 * 48_000;
/// The most the clock's rate moves from nominal while it follows: 0.5%.
pub const MAX_SLEW_PPM: i64 = 5_000;
/// A gap past which the clock takes the output's count from where it is
/// instead of slewing: 100 ms.
pub const RESYNC_FRAMES: u64 = 48_000 / 10;
/// The backlog the output keeps of the ring's frames: 40 ms, the boot's
/// `guest_audio::TARGET_FRAMES`. While the clock follows, a backlog away
/// from it moves the clock's correspondence with the output by a frame for
/// each [`LEVEL_STEP_FRAMES`] of difference summed over the publications,
/// so a backlog left at the cap (after a pause, the guest went on while the
/// output did not) comes back to the target in some seconds instead of
/// staying there.
pub const BACKLOG_TARGET_FRAMES: u64 = 48_000 / 25;
pub const LEVEL_STEP_FRAMES: i64 = 2048;
const MILLION: i64 = 1_000_000;

/// One stereo frame, packed: the left sample's bits low, the right's high.
fn pack([left, right]: [i32; 2]) -> u64 {
    u64::from(left as u32) | u64::from(right as u32) << 32
}

fn unpack(word: u64) -> [i32; 2] {
    [word as u32 as i32, (word >> 32) as u32 as i32]
}

/// Interleaved little-endian stereo samples of `sample_bytes` (2 or 4) as
/// the ring's frames, each sample MSB-aligned in 32 bits.
pub fn ring_frames(pcm: &[u8], sample_bytes: u8) -> impl Iterator<Item = [i32; 2]> + '_ {
    let wide = sample_bytes == 4;
    let frame = if wide { 8 } else { 4 };
    pcm.chunks_exact(frame).map(move |bytes| {
        let sample = |raw: &[u8]| {
            if wide {
                i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
            } else {
                i32::from(i16::from_le_bytes([raw[0], raw[1]])) << 16
            }
        };
        let (left, right) = bytes.split_at(frame / 2);
        [sample(left), sample(right)]
    })
}

/// The ring's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingStats {
    /// Frames pushed: the guest's, and silence.
    pub pushed: u64,
    /// Frames taken by the output.
    pub taken: u64,
    /// Frames a full ring dropped.
    pub dropped: u64,
}

/// The output's consumption as it last published it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Published {
    /// Frames the output consumed in all, silence included.
    pub frames: u64,
    /// The host tick it had consumed them by.
    pub tick: u64,
    /// The ring's frames waiting at the output then (its backlog).
    pub backlog: u64,
    /// The output runs; `false` once it stopped.
    pub live: bool,
    /// Each publication's own number.
    pub generation: u64,
}

/// What the sink and the boot's guest-audio service share: the frames, the
/// stream's state, and the output's publication. One producer (the sound
/// device's step, under the board's lock) and one consumer (the service
/// turn); each side only reads the other's counts.
pub struct GuestRing {
    slots: Box<[AtomicU64]>,
    pushed: AtomicU64,
    taken: AtomicU64,
    dropped: AtomicU64,
    /// The stream runs, and its samples' bits (16 or 32), 0 while stopped.
    bits: AtomicU8,
    /// [`DUE_FRAMES`] were pushed since the last ask.
    due: AtomicBool,
    /// The output's publication, under a sequence count: odd while it is
    /// written.
    sequence: AtomicU64,
    frames: AtomicU64,
    tick: AtomicU64,
    backlog: AtomicU64,
    live: AtomicBool,
}

impl GuestRing {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: (0..RING_FRAMES).map(|_| AtomicU64::new(0)).collect(),
            pushed: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            bits: AtomicU8::new(0),
            due: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            tick: AtomicU64::new(0),
            backlog: AtomicU64::new(0),
            live: AtomicBool::new(false),
        })
    }

    /// The producer: pushes `frames` while there is room; the number
    /// pushed. The rest is dropped, counted.
    fn push(&self, frames: impl Iterator<Item = [i32; 2]>) -> u64 {
        let pushed = self.pushed.load(Ordering::Relaxed);
        let room = RING_FRAMES as u64 - (pushed - self.taken.load(Ordering::Acquire));
        let mut count = 0;
        let mut dropped = 0;
        for frame in frames {
            if count == room {
                dropped += 1;
                continue;
            }
            self.slots[((pushed + count) % RING_FRAMES as u64) as usize]
                .store(pack(frame), Ordering::Relaxed);
            count += 1;
        }
        self.pushed.store(pushed + count, Ordering::Release);
        if dropped != 0 {
            self.dropped.fetch_add(dropped, Ordering::Relaxed);
        }
        count
    }

    /// Frames waiting for the output.
    pub fn queued(&self) -> usize {
        (self.pushed.load(Ordering::Acquire) - self.taken.load(Ordering::Relaxed)) as usize
    }

    /// The consumer: moves up to `out.len()` frames to `out`; the number
    /// moved.
    pub fn take(&self, out: &mut [[i32; 2]]) -> usize {
        let taken = self.taken.load(Ordering::Relaxed);
        let count = out.len().min(self.queued());
        for (index, slot) in out[..count].iter_mut().enumerate() {
            *slot = unpack(
                self.slots[((taken + index as u64) % RING_FRAMES as u64) as usize]
                    .load(Ordering::Relaxed),
            );
        }
        self.taken.store(taken + count as u64, Ordering::Release);
        count
    }

    /// The stream's samples' bits while it runs, 0 while stopped.
    pub fn bits(&self) -> u8 {
        self.bits.load(Ordering::Acquire)
    }

    fn set_bits(&self, bits: u8) {
        self.bits.store(bits, Ordering::Release);
    }

    pub fn stats(&self) -> RingStats {
        RingStats {
            pushed: self.pushed.load(Ordering::Acquire),
            taken: self.taken.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Whether frames wait for a service turn; cleared by asking.
    pub fn take_due(&self) -> bool {
        self.due.swap(false, Ordering::AcqRel)
    }

    fn write(&self, frames: u64, tick: u64, backlog: u64, live: bool) {
        // One writer: the service turn.
        self.sequence.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.frames.store(frames, Ordering::Relaxed);
        self.tick.store(tick, Ordering::Relaxed);
        self.backlog.store(backlog, Ordering::Relaxed);
        self.live.store(live, Ordering::Relaxed);
        self.sequence.fetch_add(1, Ordering::Release);
    }

    /// The output runs and had consumed `frames` in all (silence included)
    /// by host tick `tick`, with `backlog` of the ring's frames waiting:
    /// the clock follows it.
    pub fn publish(&self, frames: u64, tick: u64, backlog: u64) {
        self.write(frames, tick, backlog, true);
    }

    /// The output stopped (paused, restarting, lost): the clock goes back
    /// to the host's counter.
    pub fn publish_stopped(&self) {
        let frames = self.frames.load(Ordering::Relaxed);
        let tick = self.tick.load(Ordering::Relaxed);
        let backlog = self.backlog.load(Ordering::Relaxed);
        self.write(frames, tick, backlog, false);
    }

    /// The last publication, `None` before the first (or while one is
    /// being written, after a few tries).
    pub fn published(&self) -> Option<Published> {
        for _ in 0..64 {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let frames = self.frames.load(Ordering::Relaxed);
            let tick = self.tick.load(Ordering::Relaxed);
            let backlog = self.backlog.load(Ordering::Relaxed);
            let live = self.live.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == before {
                return (before != 0).then_some(Published {
                    frames,
                    tick,
                    backlog,
                    live,
                    generation: before / 2,
                });
            }
        }
        None
    }
}

/// What the clock runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// The stream's rate of the host's counter.
    DeviceTicks,
    /// The frames the output consumed.
    HdmiFrames,
}

impl ClockSource {
    pub const fn name(self) -> &'static str {
        match self {
            Self::DeviceTicks => "device-ticks",
            Self::HdmiFrames => "hdmi-frames",
        }
    }
}

/// A change [`FollowClock::advance`] made, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockChange {
    /// It follows the output from `position`, the output at `frames`.
    Following { position: u64, frames: u64 },
    /// It went back to the host's counter at `position`: the output
    /// stopped, or (`stale`) did not publish for [`STALE_MILLIS`].
    Fallback { position: u64, stale: bool },
    /// The output was `gap` frames from the clock (ahead when positive):
    /// its count is taken again from where the clock is.
    Resync { position: u64, gap: i64 },
}

/// The ring sink's clock: the stream's rate of the host's counter, or the
/// output's consumption once it publishes, slewed to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowClock {
    hz: u64,
    rate: u64,
    stale_ticks: u64,
    running: bool,
    /// The position `base` at host tick `since`, from where it runs at its
    /// rate moved by `ppm` parts per million.
    since: u64,
    base: u64,
    ppm: i64,
    source: ClockSource,
    /// While it follows: the output's count and the clock's position that
    /// correspond.
    anchor: (u64, i128),
    /// The publication last looked at.
    seen: u64,
    /// The backlog's difference from its target, summed over the
    /// publications, not yet a whole [`LEVEL_STEP_FRAMES`].
    level: i64,
    handoffs: u64,
    resyncs: u64,
}

impl FollowClock {
    /// A clock on a host counter of `hz`.
    pub fn new(hz: u64) -> Self {
        Self {
            hz: hz.max(1),
            rate: 48_000,
            stale_ticks: hz * STALE_MILLIS / 1000,
            running: false,
            since: 0,
            base: 0,
            ppm: 0,
            source: ClockSource::DeviceTicks,
            anchor: (0, 0),
            seen: 0,
            level: 0,
            handoffs: 0,
            resyncs: 0,
        }
    }

    pub fn source(&self) -> ClockSource {
        self.source
    }

    /// The rate's slew, in parts per million.
    pub fn ppm(&self) -> i64 {
        self.ppm
    }

    pub fn running(&self) -> bool {
        self.running
    }

    /// Handoffs between the two sources, and resyncs, since it was made.
    pub fn handoffs(&self) -> u64 {
        self.handoffs
    }

    pub fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Frames a second times a million, at the current slew.
    fn scaled_rate(&self) -> u128 {
        (self.rate as i128 * (MILLION + self.ppm) as i128) as u128
    }

    /// Runs at `rate` from `position` at host tick `now`, on the host's
    /// counter until the output publishes.
    pub fn start(&mut self, rate: u32, now: u64, position: u64) {
        self.rate = u64::from(rate).max(1);
        self.running = true;
        self.since = now;
        self.base = position;
        self.ppm = 0;
        self.source = ClockSource::DeviceTicks;
    }

    /// Stops at host tick `now`, keeping the position reached; its source
    /// stays the one it ran on, for the stop's report line.
    pub fn stop(&mut self, now: u64) {
        self.base = self.position(now);
        self.since = now.max(self.since);
        self.running = false;
        self.ppm = 0;
    }

    /// The position at host tick `now`, which never goes back: a tick
    /// before the last change gives that change's position.
    pub fn position(&self, now: u64) -> u64 {
        if !self.running {
            return self.base;
        }
        let ticks = u128::from(now.saturating_sub(self.since));
        let frames = ticks * self.scaled_rate() / (u128::from(self.hz) * MILLION as u128);
        self.base + frames.min(u128::from(u64::MAX - self.base)) as u64
    }

    /// The position the current rate gives at `tick`, before the last
    /// change as well (for the gap to a publication made before it).
    fn position_at(&self, tick: u64) -> i128 {
        let ticks = tick as i128 - self.since as i128;
        self.base as i128
            + ticks * self.scaled_rate() as i128 / (i128::from(self.hz) * MILLION as i128)
    }

    /// The first host tick the position reaches `frames`, `None` while
    /// stopped.
    pub fn when(&self, frames: u64) -> Option<u64> {
        if !self.running {
            return None;
        }
        let owed = u128::from(frames.saturating_sub(self.base));
        let rate = self.scaled_rate().max(1);
        let ticks = (owed * u128::from(self.hz) * MILLION as u128).div_ceil(rate);
        Some(
            self.since
                .saturating_add(ticks.min(u128::from(u64::MAX)) as u64),
        )
    }

    /// From host tick `now` on, runs at `ppm` from the position reached.
    /// The new rate counts from the first tick that position was reached,
    /// so the part of a frame the old rate ran past it is kept rather than
    /// dropped at each change.
    fn change(&mut self, now: u64, ppm: i64) {
        let position = self.position(now);
        self.since = self.when(position).unwrap_or(now).max(self.since);
        self.base = position;
        self.ppm = ppm;
    }

    /// Looks at the output's last publication at host tick `now`: hands
    /// the clock to it, slews toward it, or falls back to the host's
    /// counter. What changed, for the log.
    pub fn advance(&mut self, now: u64, published: Option<Published>) -> Option<ClockChange> {
        // Nothing published yet, or a publication being written: the next
        // step looks again.
        let (true, Some(p)) = (self.running, published) else {
            return None;
        };
        let stale = now.saturating_sub(p.tick) > self.stale_ticks;
        let fresh = (p.live && !stale).then_some(p);
        match (self.source, fresh) {
            (ClockSource::DeviceTicks, Some(p)) => {
                self.change(now, 0);
                self.anchor = (p.frames, self.position_at(p.tick));
                self.seen = p.generation;
                self.level = 0;
                self.source = ClockSource::HdmiFrames;
                self.handoffs += 1;
                Some(ClockChange::Following {
                    position: self.base,
                    frames: p.frames,
                })
            }
            (ClockSource::HdmiFrames, Some(p)) if p.generation != self.seen => {
                self.seen = p.generation;
                // A backlog over its target moves the correspondence back
                // (the clock slows), one under it forward.
                self.level += BACKLOG_TARGET_FRAMES as i64 - p.backlog.min(1 << 40) as i64;
                let steps = self.level / LEVEL_STEP_FRAMES;
                self.level -= steps * LEVEL_STEP_FRAMES;
                self.anchor.1 += i128::from(steps);
                let (frames, position) = self.anchor;
                let at = self.position_at(p.tick);
                let wanted = position + (p.frames as i128 - frames as i128);
                let gap = (wanted - at).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
                if gap.unsigned_abs() > RESYNC_FRAMES {
                    self.change(now, 0);
                    self.anchor = (p.frames, self.position_at(p.tick));
                    self.level = 0;
                    self.resyncs += 1;
                    return Some(ClockChange::Resync {
                        position: self.base,
                        gap,
                    });
                }
                let ppm = (gap * MILLION / SLEW_HORIZON_FRAMES).clamp(-MAX_SLEW_PPM, MAX_SLEW_PPM);
                self.change(now, ppm);
                None
            }
            (ClockSource::HdmiFrames, None) => {
                self.change(now, 0);
                self.source = ClockSource::DeviceTicks;
                self.handoffs += 1;
                Some(ClockChange::Fallback {
                    position: self.base,
                    stale: p.live,
                })
            }
            _ => None,
        }
    }
}

/// The sink of `--virtio-snd` with `--guest-audio-hdmi`: the guest's frames
/// into the [`GuestRing`] the boot's guest-audio service plays on the HDMI
/// route, at a [`FollowClock`]. It counts and meters as the counting sink
/// does, and with a capture keeps its fingerprint as well.
pub struct GuestAudioRing {
    /// Counts, meters and captures.
    inner: Box<dyn Sink>,
    ring: Arc<GuestRing>,
    clock: FollowClock,
    format: Option<Format>,
    capture: Option<u32>,
    /// The ring's pushes at the last time it asked for a service.
    due_mark: u64,
    notes: Vec<String>,
    /// Clock changes noted, of each kind.
    noted: u64,
}

/// Whether the `count`th change gets its note: the first four, then each
/// power of two.
fn noted(count: u64) -> bool {
    count <= 4 || count.is_power_of_two()
}

impl GuestAudioRing {
    /// Into `ring`, on a host counter of `hz`; with `capture`, keeping that
    /// many seconds of the first sound for its fingerprint.
    pub fn new(ring: Arc<GuestRing>, hz: u64, capture: Option<u32>) -> Self {
        Self {
            inner: super::boxed(hz, capture),
            ring,
            clock: FollowClock::new(hz),
            format: None,
            capture,
            due_mark: 0,
            notes: Vec::new(),
            noted: 0,
        }
    }

    /// The clock, for tests and the report.
    pub fn follow_clock(&self) -> &FollowClock {
        &self.clock
    }

    fn pushed(&mut self, count: u64) {
        let pushed = self.ring.pushed.load(Ordering::Relaxed);
        if count != 0 && pushed - self.due_mark >= DUE_FRAMES {
            self.due_mark = pushed;
            self.ring.due.store(true, Ordering::Release);
        }
    }
}

impl Sink for GuestAudioRing {
    fn describe(&self) -> String {
        format!(
            "the guest-audio ring (48 kHz only) to the HDMI output, its clock following the HDMI \
             periods written (hdmi-frames), the host's device clock while HDMI does not run \
             (device-ticks){}",
            self.capture
                .map(|seconds| format!(
                    ", keeping the first {seconds} s of sound for its fingerprint"
                ))
                .unwrap_or_default()
        )
    }

    fn start(&mut self, format: Format, now: u64, position: u64) {
        self.format = Some(format);
        self.inner.start(format, now, position);
        self.clock.start(format.rate, now, position);
        self.ring.set_bits(format.sample_bytes * 8);
    }

    fn stop(&mut self, now: u64) {
        self.inner.stop(now);
        self.clock.stop(now);
        self.ring.set_bits(0);
    }

    fn clock(&self, now: u64) -> u64 {
        self.clock.position(now)
    }

    fn when(&self, frames: u64) -> Option<u64> {
        self.clock.when(frames)
    }

    fn play(&mut self, pcm: &[u8]) {
        self.inner.play(pcm);
        let Some(format) = self.format else {
            return;
        };
        let count = self.ring.push(ring_frames(pcm, format.sample_bytes));
        self.pushed(count);
    }

    fn silence(&mut self, frames: u64) {
        self.inner.silence(frames);
        let fits = frames.min(RING_FRAMES as u64);
        let count = self.ring.push((0..fits).map(|_| [0, 0]));
        self.pushed(count);
    }

    fn played(&self) -> Played {
        self.inner.played()
    }

    fn take_level(&mut self) -> Meter {
        self.inner.take_level()
    }

    fn release(&mut self) {
        self.inner.release();
        self.ring.set_bits(0);
    }

    fn take_notes(&mut self) -> Vec<String> {
        let mut notes = core::mem::take(&mut self.notes);
        notes.extend(self.inner.take_notes());
        notes
    }

    fn advance(&mut self, now: u64) {
        let Some(change) = self.clock.advance(now, self.ring.published()) else {
            return;
        };
        self.noted += 1;
        if !noted(self.noted) {
            return;
        }
        let note = match change {
            ClockChange::Following { position, frames } => format!(
                "clock: hdmi-frames from frame {position}: follows the HDMI periods written (the \
                 output at {frames} frames)"
            ),
            ClockChange::Fallback { position, stale } => format!(
                "clock: device-ticks from frame {position}: the HDMI output {}; the guest is paced \
                 by the host's device clock until it runs again",
                if stale {
                    "did not publish for 100 ms"
                } else {
                    "stopped (paused, restarting or lost)"
                }
            ),
            ClockChange::Resync { position, gap } => format!(
                "clock: hdmi-frames resync at frame {position}: the output was {gap} frames from \
                 the clock; its count taken again from here"
            ),
        };
        self.notes
            .push(format!("{note} (clock change {})", self.noted));
    }

    fn rates(&self) -> u64 {
        1 << RATE_48000
    }

    fn clock_source(&self) -> Option<&'static str> {
        Some(self.clock.source().name())
    }

    fn take_due(&mut self) -> bool {
        self.ring.take_due()
    }

    fn counters(&self) -> Vec<(&'static str, u64)> {
        let stats = self.ring.stats();
        alloc::vec![
            ("ring_pushed", stats.pushed),
            ("ring_dropped", stats.dropped),
            ("clock_handoffs", self.clock.handoffs()),
            ("clock_resyncs", self.clock.resyncs()),
        ]
    }
}

#[cfg(test)]
mod tests;
