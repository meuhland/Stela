//! Where the sound device's frames go, and the clock that paces them.
//!
//! A [`Sink`] consumes the stream at its own rate: its clock says how many
//! frames it has taken by a host time ([`Sink::clock`]), and the device
//! completes the guest's transfers as that clock passes their last frame, so
//! the guest's audio engine runs at the sink's rate, not at the guest's. The
//! device hands the sink each transfer's frames as its clock reaches them
//! ([`Sink::play`]), and silence for frames its clock passed with none
//! ([`Sink::silence`]).
//!
//! - [`Counting`] consumes the stream's rate of the host's counter, as
//!   `audio-main`'s M1 sink does, and keeps the frames and each report
//!   interval's peak and RMS ([`Meter`]).
//! - [`Capture`] is a counting sink that also keeps the first seconds of
//!   the guest's sound in host memory, from its first sample that is not
//!   silence, and logs once their SHA-256, peak and RMS: a boot can show
//!   that the guest's samples arrived intact without anyone listening.
//! - [`GuestAudioRing`] plays the stream on the host's guest-audio output
//!   (the HDMI route of `--guest-audio-hdmi`): its `play` pushes the frames
//!   into a ring the boot's guest-audio service takes from, and its clock
//!   follows the frames that output consumed, on the host's counter while
//!   the output does not run (`ring`).
mod ring;

pub use ring::{
    BACKLOG_TARGET_FRAMES, ClockChange, ClockSource, FollowClock, GuestAudioRing, GuestRing,
    LEVEL_STEP_FRAMES, MAX_SLEW_PPM, Published, RESYNC_FRAMES, RING_FRAMES, RingStats,
    SLEW_HORIZON_FRAMES, STALE_MILLIS, ring_frames,
};

use alloc::{boxed::Box, format, string::String, vec::Vec};
use core::fmt;
use sha2::{Digest, Sha256};

/// The stream's format as `SET_PARAMS` agreed: interleaved little-endian
/// signed samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub channels: u8,
    /// Bytes per sample: 2 (S16) or 4 (S32).
    pub sample_bytes: u8,
    /// Frames a second.
    pub rate: u32,
}

impl Format {
    pub const fn frame_bytes(self) -> u32 {
        self.channels as u32 * self.sample_bytes as u32
    }

    /// `frames` in milliseconds, rounded down.
    pub const fn millis(self, frames: u64) -> u64 {
        frames * 1000 / self.rate as u64
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}ch S{} {} Hz",
            self.channels,
            self.sample_bytes * 8,
            self.rate
        )
    }
}

/// A clock that takes `rate` frames a second of a counter ticking `hz`
/// times a second, from a position, while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateClock {
    hz: u64,
    rate: u64,
    /// The tick it started at and the position it started from, while it
    /// runs; the position it stopped at, otherwise.
    started: Option<u64>,
    base: u64,
}

impl RateClock {
    pub const fn new(hz: u64) -> Self {
        Self {
            hz,
            rate: 48_000,
            started: None,
            base: 0,
        }
    }

    /// Runs at `rate` from `position` at tick `now`.
    pub fn start(&mut self, rate: u32, now: u64, position: u64) {
        self.rate = u64::from(rate).max(1);
        self.base = position;
        self.started = Some(now);
    }

    /// Stops at tick `now`, keeping the position reached.
    pub fn stop(&mut self, now: u64) {
        self.base = self.position(now);
        self.started = None;
    }

    pub fn running(&self) -> bool {
        self.started.is_some()
    }

    /// The frames taken by tick `now`; a tick before the start takes none.
    pub fn position(&self, now: u64) -> u64 {
        let Some(started) = self.started else {
            return self.base;
        };
        let ticks = u128::from(now.saturating_sub(started));
        self.base + (ticks * u128::from(self.rate) / u128::from(self.hz.max(1))) as u64
    }

    /// The first tick at which [`Self::position`] reaches `frames`: `None`
    /// while it is stopped.
    pub fn when(&self, frames: u64) -> Option<u64> {
        let started = self.started?;
        let owed = u128::from(frames.saturating_sub(self.base));
        let ticks =
            (owed * u128::from(self.hz) + u128::from(self.rate) - 1) / u128::from(self.rate);
        Some(started.saturating_add(ticks.min(u128::from(u64::MAX)) as u64))
    }
}

/// A level in tenths of a dB below full scale, to a tenth: `-12.0dBFS`,
/// `0.0dBFS`, or `-inf` for silence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dbfs(pub Option<u32>);

impl Dbfs {
    /// A power ratio to full scale (0 to 1) as dBFS, `10 log10` without a
    /// float logarithm (`core` has none): the smallest tenth whose rounding
    /// bound the ratio reaches. Below -200 dB it is -200.0.
    pub fn of_power(ratio: f64) -> Self {
        if ratio.is_nan() || ratio <= 0.0 {
            return Self(None);
        }
        // 10^(-0.05/10) and 10^(-0.1/10): half a tenth, and a tenth.
        const HALF: f64 = 0.988_553_094_656_939_1;
        const STEP: f64 = 0.977_237_220_955_810_7;
        let mut bound = HALF;
        let mut tenths = 0;
        while ratio < bound && tenths < 2000 {
            bound *= STEP;
            tenths += 1;
        }
        Self(Some(tenths))
    }
}

impl fmt::Display for Dbfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("-inf"),
            Some(0) => f.write_str("0.0dBFS"),
            Some(tenths) => write!(f, "-{}.{}dBFS", tenths / 10, tenths % 10),
        }
    }
}

/// Full scale of a sample brought to 32 bits.
const FULL: f64 = 2_147_483_648.0;

/// The peak and the mean square of samples, each brought to 32 bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Meter {
    /// The largest magnitude, up to 2^31.
    pub peak: u64,
    pub squares: u128,
    pub samples: u64,
}

impl Meter {
    /// Adds interleaved little-endian samples of `sample_bytes` (2 or 4).
    pub fn add(&mut self, pcm: &[u8], sample_bytes: u8) {
        let mut each = |value: i64| {
            let magnitude = value.unsigned_abs();
            self.peak = self.peak.max(magnitude);
            self.squares += u128::from(magnitude) * u128::from(magnitude);
            self.samples += 1;
        };
        if sample_bytes == 2 {
            for sample in pcm.as_chunks::<2>().0 {
                each(i64::from(i16::from_le_bytes(*sample)) << 16);
            }
        } else {
            for sample in pcm.as_chunks::<4>().0 {
                each(i64::from(i32::from_le_bytes(*sample)));
            }
        }
    }

    /// Samples of silence.
    pub fn add_silence(&mut self, samples: u64) {
        self.samples += samples;
    }

    pub fn peak_dbfs(&self) -> Dbfs {
        let ratio = self.peak as f64 / FULL;
        Dbfs::of_power(ratio * ratio)
    }

    pub fn rms_dbfs(&self) -> Dbfs {
        if self.samples == 0 {
            return Dbfs(None);
        }
        Dbfs::of_power(self.squares as f64 / self.samples as f64 / (FULL * FULL))
    }
}

/// What a sink played, since the device was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Played {
    /// The guest's frames.
    pub frames: u64,
    /// Frames of silence where the guest had none.
    pub silent: u64,
}

/// The consumer of a stream.
pub trait Sink {
    /// What it is, for the device's placement line.
    fn describe(&self) -> String;
    /// The stream runs in `format` from frame `position` at host tick `now`.
    fn start(&mut self, format: Format, now: u64, position: u64);
    /// The stream stops at host tick `now`.
    fn stop(&mut self, now: u64);
    /// The frame position its clock reached by host tick `now`.
    fn clock(&self, now: u64) -> u64;
    /// The first host tick its clock reaches `frames`, `None` while stopped.
    fn when(&self, frames: u64) -> Option<u64>;
    /// The guest's next frames, interleaved as the format of the last
    /// [`Self::start`].
    fn play(&mut self, pcm: &[u8]);
    /// `frames` its clock passed with none of the guest's.
    fn silence(&mut self, frames: u64);
    fn played(&self) -> Played;
    /// The level since the last call.
    fn take_level(&mut self) -> Meter;
    /// The stream was released, or the device reset.
    fn release(&mut self) {}
    /// Lines of its own for the log since the last call.
    fn take_notes(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Looks at its output at host tick `now`, at the start of each device
    /// step, before the step reads its clock: a sink whose clock follows
    /// another one moves it here.
    fn advance(&mut self, _now: u64) {}
    /// The rates it plays, as `virtio_snd` `RATE_*` bits: the device offers
    /// only these and refuses the others at `SET_PARAMS`.
    fn rates(&self) -> u64 {
        super::RATES
    }
    /// What its clock runs on, for the report line, when it can change.
    fn clock_source(&self) -> Option<&'static str> {
        None
    }
    /// Whether frames wait for a host service that takes them; cleared by
    /// asking.
    fn take_due(&mut self) -> bool {
        false
    }
    /// Counters of its own, by name, for the device's report.
    fn counters(&self) -> Vec<(&'static str, u64)> {
        Vec::new()
    }
}

/// Consumes the stream's rate of a host counter of `hz`, counts and meters.
pub struct Counting {
    clock: RateClock,
    format: Option<Format>,
    played: Played,
    meter: Meter,
}

impl Counting {
    pub fn new(hz: u64) -> Self {
        Self {
            clock: RateClock::new(hz),
            format: None,
            played: Played::default(),
            meter: Meter::default(),
        }
    }
}

impl Sink for Counting {
    fn describe(&self) -> String {
        "a counting sink at the stream's rate of the host's clock".into()
    }

    fn start(&mut self, format: Format, now: u64, position: u64) {
        self.format = Some(format);
        self.clock.start(format.rate, now, position);
    }

    fn stop(&mut self, now: u64) {
        self.clock.stop(now);
    }

    fn clock(&self, now: u64) -> u64 {
        self.clock.position(now)
    }

    fn when(&self, frames: u64) -> Option<u64> {
        self.clock.when(frames)
    }

    fn play(&mut self, pcm: &[u8]) {
        let Some(format) = self.format else {
            return;
        };
        self.played.frames += pcm.len() as u64 / u64::from(format.frame_bytes().max(1));
        self.meter.add(pcm, format.sample_bytes);
    }

    fn silence(&mut self, frames: u64) {
        self.played.silent += frames;
        let channels = self.format.map_or(2, |format| format.channels);
        self.meter.add_silence(frames * u64::from(channels));
    }

    fn played(&self) -> Played {
        self.played
    }

    fn take_level(&mut self) -> Meter {
        core::mem::take(&mut self.meter)
    }
}

/// A counting sink that keeps the first `seconds` of the guest's sound,
/// from its first sample that is not silence, and logs their fingerprint
/// once: when it has them all, or when the stream ends before.
pub struct Capture {
    counting: Counting,
    seconds: u32,
    /// The bytes kept, from the first sound on.
    kept: Vec<u8>,
    /// The bytes to keep, once the format is known.
    limit: usize,
    /// The fingerprint was logged.
    done: bool,
    notes: Vec<String>,
}

impl Capture {
    pub fn new(hz: u64, seconds: u32) -> Self {
        Self {
            counting: Counting::new(hz),
            seconds,
            kept: Vec::new(),
            limit: 0,
            done: false,
            notes: Vec::new(),
        }
    }

    /// The bytes kept so far.
    pub fn kept(&self) -> &[u8] {
        &self.kept
    }

    fn finish(&mut self, partial: bool) {
        if self.done || (partial && self.kept.is_empty()) {
            return;
        }
        self.done = true;
        let kept = core::mem::take(&mut self.kept);
        let Some(format) = self.counting.format else {
            return;
        };
        self.notes
            .push(fingerprint(&kept, format, self.seconds, partial));
    }
}

/// The capture's line: what it kept, its SHA-256, peak and RMS.
pub fn fingerprint(kept: &[u8], format: Format, seconds: u32, partial: bool) -> String {
    let frames = kept.len() as u64 / u64::from(format.frame_bytes().max(1));
    let mut meter = Meter::default();
    meter.add(kept, format.sample_bytes);
    let millis = format.millis(frames);
    format!(
        "capture{}: {} frames ({}.{:03} s, {} bytes, {format}) from the first sound{}: \
         sha256={} peak={} rms={}",
        if partial { " (partial)" } else { "" },
        frames,
        millis / 1000,
        millis % 1000,
        kept.len(),
        if partial {
            format!(", the stream ended before {seconds} s")
        } else {
            String::new()
        },
        hex::encode(Sha256::digest(kept)),
        meter.peak_dbfs(),
        meter.rms_dbfs(),
    )
}

impl Sink for Capture {
    fn describe(&self) -> String {
        format!(
            "{}, keeping the first {} s of sound for its fingerprint",
            self.counting.describe(),
            self.seconds
        )
    }

    fn start(&mut self, format: Format, now: u64, position: u64) {
        self.counting.start(format, now, position);
        if self.limit == 0 {
            self.limit =
                self.seconds as usize * format.rate as usize * format.frame_bytes() as usize;
        }
    }

    fn stop(&mut self, now: u64) {
        self.counting.stop(now);
    }

    fn clock(&self, now: u64) -> u64 {
        self.counting.clock(now)
    }

    fn when(&self, frames: u64) -> Option<u64> {
        self.counting.when(frames)
    }

    fn play(&mut self, pcm: &[u8]) {
        self.counting.play(pcm);
        if self.done || self.limit == 0 {
            return;
        }
        let pcm = if self.kept.is_empty() {
            // The capture starts at the first frame that is not silence.
            let frame = self
                .counting
                .format
                .map_or(4, |format| format.frame_bytes() as usize)
                .max(1);
            match pcm
                .chunks(frame)
                .position(|frame| frame.iter().any(|&b| b != 0))
            {
                Some(first) => {
                    self.kept.reserve_exact(self.limit);
                    &pcm[first * frame..]
                }
                None => return,
            }
        } else {
            pcm
        };
        let room = self.limit - self.kept.len();
        self.kept.extend_from_slice(&pcm[..pcm.len().min(room)]);
        if self.kept.len() == self.limit {
            self.finish(false);
        }
    }

    fn silence(&mut self, frames: u64) {
        self.counting.silence(frames);
    }

    fn played(&self) -> Played {
        self.counting.played()
    }

    fn take_level(&mut self) -> Meter {
        self.counting.take_level()
    }

    fn release(&mut self) {
        self.finish(true);
    }

    fn take_notes(&mut self) -> Vec<String> {
        core::mem::take(&mut self.notes)
    }
}

/// The sink a boot asks for: a counting sink, or a capture of `seconds`.
pub fn boxed(hz: u64, capture: Option<u32>) -> Box<dyn Sink> {
    match capture {
        Some(seconds) => Box::new(Capture::new(hz, seconds)),
        None => Box::new(Counting::new(hz)),
    }
}
