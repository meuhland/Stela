//! The host's side of the guest's USB speaker (`stela_soc::usb::audio`):
//! how many of its frames a service turn takes, and the line that reports
//! them.
//!
//! The guest's frames come at 48 kHz of guest time; the host consumes them
//! at 48 kHz of its own clock, the counter for a sink or the HDA link for
//! an HDMI stream. Nothing resamples. The host keeps a backlog
//! ([`Backlog`]): a stream fills to [`TARGET_FRAMES`] before it plays, and
//! again after it ran dry; a turn that owes more frames than wait plays
//! silence for the rest, and frames beyond [`CAP_FRAMES`] after a turn are
//! dropped, oldest first, so the guest's sound never trails the host's
//! clock by more than that. An asynchronous speaker is told the rate the
//! host consumes ([`Feedback`]), so the guest's own resampler matches it
//! and the backlog stays near its target.
use core::fmt;

/// The only sample rate.
pub const RATE: u64 = 48_000;
/// The backlog the host keeps of the guest's frames: 40 ms. A stream fills
/// to it before it plays, and the feedback holds it there.
pub const TARGET_FRAMES: usize = RATE as usize / 25;
/// The guest's frames a turn leaves waiting at most: 80 ms. Delivery
/// jitter up to the target is absorbed without padding, and a guest
/// faster than the output loses frames only past this.
pub const CAP_FRAMES: usize = 2 * TARGET_FRAMES;
/// The most one turn owes a sink: 200 ms, the guest ring's size. A longer
/// gap in the service is not paid back with that much silence.
pub const MAX_STEP_FRAMES: u64 = RATE / 5;

/// The periods of `period_frames` an HDMI playback's ring holds: a service
/// gap the guest's own ring rides out ([`MAX_STEP_FRAMES`], 200 ms) also
/// leaves the HDA ring's DMA fed. The service refills a period only once
/// the DMA completed it, so the refilled frontier lags the DMA by up to a
/// period, and the controller keeps its guard (the FIFO and one link
/// packet, a few ms) out of the ring: one period more for each. A longer
/// ring would only add latency, as a longer gap already loses the guest's
/// frames at its own ring.
pub const fn hdmi_periods(period_frames: usize) -> usize {
    (MAX_STEP_FRAMES as usize).div_ceil(period_frames) + 2
}

/// Why the HDMI playback's stream stopped, and so how it starts again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    /// The DMA ran past the refilled periods: a service gap longer than
    /// the ring. The stream alone restarts.
    Underrun,
    /// The pin sensed no sink with a valid ELD: the monitor sleeps or was
    /// unplugged. The route pauses, prepared, until it returns.
    SinkAway,
    /// A codec answer the route did not set (a readback, an unanswered
    /// sense, a verb timeout). After a rejected readback the route's verbs
    /// are set again first; the stream restarts, a bounded number of times
    /// in a row.
    CodecFault,
    /// The stream's DMA reported a FIFO error alone (SDnSTS FIFOE, no
    /// descriptor error): its memory reads fell behind the link, as on a
    /// congested PCIe link. Since M9 the driver acknowledges it and the
    /// stream plays on, as Linux does (`fifo_errors=` counts them), so no
    /// HDMI stop has this reason any more; the count stays in the report.
    DmaFault,
    /// The route itself was lost (another ELD, a failed access, the bound
    /// of codec faults, a DMA descriptor error). It is prepared again, the
    /// codec walked, after a controller reset when it needs one.
    RouteLost,
}

impl RestartReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Underrun => "underrun",
            Self::SinkAway => "sink_away",
            Self::CodecFault => "codec_fault",
            Self::DmaFault => "dma_fault",
            Self::RouteLost => "route_lost",
        }
    }

    /// What the playback does about it, for the stop line.
    pub const fn action(self) -> &'static str {
        match self {
            Self::Underrun => {
                "the DMA ran past the refilled periods (a service gap longer than the ring), or \
                 its position stopped for as long as the ring plays; restarting the stream only"
            }
            Self::SinkAway => {
                "the pin senses no monitor with a valid ELD (asleep or unplugged); paused, \
                 the route stays prepared until it returns"
            }
            Self::CodecFault => {
                "the codec answered what the route did not set; setting the route's verbs again \
                 after a rejected readback (no codec walk), then restarting the stream only"
            }
            Self::DmaFault => {
                "the stream's DMA fell behind its FIFO (FIFOE, no descriptor error); restarting \
                 the stream only"
            }
            Self::RouteLost => "the route was lost; preparing it again",
        }
    }
}

/// Whether the `count`th event of a kind gets lines of its own: the first
/// four, then each power of two; the report's counts cover the rest.
pub const fn logged(count: u64) -> bool {
    count <= 4 || count.is_power_of_two()
}

/// Whether a count that went from `last` to `now` (it may grow by more
/// than one between two looks) passed one that is [`logged`].
pub const fn crossed(last: u64, now: u64) -> bool {
    now > last && (last < 4 || now.ilog2() > last.ilog2())
}

/// The HDMI playback's restarts, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restarts {
    pub underrun: u64,
    pub sink_away: u64,
    pub codec_fault: u64,
    pub dma_fault: u64,
    pub route_lost: u64,
}

impl Restarts {
    /// Counts one restart for `reason`: its reason's count after it.
    pub fn add(&mut self, reason: RestartReason) -> u64 {
        let count = match reason {
            RestartReason::Underrun => &mut self.underrun,
            RestartReason::SinkAway => &mut self.sink_away,
            RestartReason::CodecFault => &mut self.codec_fault,
            RestartReason::DmaFault => &mut self.dma_fault,
            RestartReason::RouteLost => &mut self.route_lost,
        };
        *count += 1;
        *count
    }

    pub const fn total(&self) -> u64 {
        self.underrun + self.sink_away + self.codec_fault + self.dma_fault + self.route_lost
    }
}

impl fmt::Display for Restarts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "restarts={} underrun={} sink_away={} codec_fault={} dma_fault={} route_lost={}",
            self.total(),
            self.underrun,
            self.sink_away,
            self.codec_fault,
            self.dma_fault,
            self.route_lost
        )
    }
}

/// The test tone's frequency (`--guest-audio-hdmi-tone`): 48 samples a
/// cycle at [`RATE`], so a 10 ms period holds ten whole cycles.
pub const TONE_HZ: u64 = 1_000;
/// The test tone's peak, -12 dBFS of 16-bit full scale (32768):
/// 32768 x 10^(-12/20) = 8231.1, rounded.
pub const TONE_PEAK: i16 = 8231;
/// The test tone's level in tenths of a dB below full scale.
pub const TONE_TENTHS_DBFS: u32 = 120;
const TONE_SAMPLES: usize = (RATE / TONE_HZ) as usize;

/// sin(x) for x in [-pi, pi], by its Taylor series to x^21 (an error
/// far under a 16-bit step), usable in a const.
const fn sine(x: f64) -> f64 {
    let mut term = x;
    let mut sum = x;
    let mut n = 1;
    while n < 11 {
        term = -term * x * x / ((2 * n) as f64 * (2 * n + 1) as f64);
        sum += term;
        n += 1;
    }
    sum
}

/// One cycle of the tone, from phase 0.
const TONE: [i16; TONE_SAMPLES] = {
    let mut table = [0i16; TONE_SAMPLES];
    let mut k = 0;
    while k < TONE_SAMPLES {
        // The phase in [-pi, pi], where the series holds.
        let turns = k as f64 / TONE_SAMPLES as f64;
        let turns = if turns > 0.5 { turns - 1.0 } else { turns };
        let value = TONE_PEAK as f64 * sine(2.0 * core::f64::consts::PI * turns);
        let rounded = if value < 0.0 {
            value - 0.5
        } else {
            value + 0.5
        };
        table[k] = rounded as i16;
        k += 1;
    }
    table
};

/// The built-in test tone (`--guest-audio-hdmi-tone`): a [`TONE_HZ`] sine
/// at -12 dBFS ([`TONE_PEAK`]), the same on both channels, its phase
/// carried from one period to the next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tone {
    phase: usize,
}

impl Tone {
    /// Writes the next frames of the tone into `out` as 16-bit
    /// little-endian stereo, whole 4-byte frames.
    pub fn fill(&mut self, out: &mut [u8]) {
        for slot in out.as_chunks_mut::<4>().0 {
            let sample = TONE[self.phase].to_le_bytes();
            slot[..2].copy_from_slice(&sample);
            slot[2..].copy_from_slice(&sample);
            self.phase = (self.phase + 1) % TONE_SAMPLES;
        }
    }
}

/// The largest absolute sample of 16-bit little-endian PCM, 0..=32768.
pub fn peak(pcm: &[u8]) -> u32 {
    pcm.as_chunks::<2>()
        .0
        .iter()
        .map(|sample| i16::from_le_bytes(*sample).unsigned_abs().into())
        .max()
        .unwrap_or(0)
}

/// A peak sample ([`peak`]) as dBFS to a tenth of a dB, full scale 32768:
/// `-12.0dBFS`, `0.0dBFS`, or `-inf` for silence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dbfs(pub u32);

impl Dbfs {
    /// Tenths of a dB below full scale, rounded; `None` for silence.
    pub fn tenths(self) -> Option<u32> {
        if self.0 == 0 {
            return None;
        }
        // The smallest t whose rounding bound 32768 x 10^(-(t + 0.5)/200)
        // the peak reaches: 20 log10 without a float logarithm.
        const HALF: f64 = 0.994_260_073_953_2; // 10^(-0.5/200)
        const STEP: f64 = 0.988_553_094_656_939_1; // 10^(-1/200)
        let peak = f64::from(self.0);
        let mut bound = 32768.0 * HALF;
        let mut tenths = 0;
        while peak < bound && tenths < 2000 {
            bound *= STEP;
            tenths += 1;
        }
        Some(tenths)
    }
}

impl fmt::Display for Dbfs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.tenths() {
            None => f.write_str("-inf"),
            Some(0) => f.write_str("0.0dBFS"),
            Some(tenths) => write!(f, "-{}.{}dBFS", tenths / 10, tenths % 10),
        }
    }
}

/// Paces a consumer at [`RATE`] frames a second of a counter that ticks
/// `hz` times a second.
#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    hz: u64,
    last: Option<u64>,
    /// Ticks times [`RATE`] not yet a whole frame.
    remainder: u64,
}

impl Pacer {
    pub const fn new(hz: u64) -> Self {
        Self {
            hz,
            last: None,
            remainder: 0,
        }
    }

    /// The stream stopped: the next [`Self::due`] starts over.
    pub fn stop(&mut self) {
        self.last = None;
        self.remainder = 0;
    }

    /// Frames due since the last call, at counter `now`: none at the first
    /// call after [`Self::new`] or [`Self::stop`], and at most
    /// [`MAX_STEP_FRAMES`]. A counter that went back owes nothing.
    pub fn due(&mut self, now: u64) -> u64 {
        let Some(last) = self.last.replace(now) else {
            self.remainder = 0;
            return 0;
        };
        let hz = u128::from(self.hz.max(1));
        let scaled =
            u128::from(now.saturating_sub(last)) * u128::from(RATE) + u128::from(self.remainder);
        let frames = scaled / hz;
        if frames >= u128::from(MAX_STEP_FRAMES) {
            self.remainder = 0;
            return MAX_STEP_FRAMES;
        }
        self.remainder = (scaled % hz) as u64;
        frames as u64
    }
}

/// What one turn does with the guest's frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// The oldest frames dropped first, past the backlog.
    pub dropped: usize,
    /// Frames played, after the dropped ones.
    pub take: usize,
    /// Frames owed that the guest had not sent: silence.
    pub silent: u64,
}

/// The host's backlog of the guest's frames: whether it plays or fills.
///
/// A stream fills first: the output plays silence until [`TARGET_FRAMES`]
/// wait, so delivery jitter up to that much never shows as padding. It
/// then plays what waits; a turn that finds fewer frames than it owes ran
/// dry, plays what there is and fills again. A guest that stopped
/// streaming has what it left played out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backlog {
    playing: bool,
}

impl Backlog {
    /// Whether it fills, playing silence until the target waits.
    pub const fn filling(&self) -> bool {
        !self.playing
    }

    /// Frames to play of `due` owed, with `queued` waiting; `streaming`
    /// whether the guest streams.
    pub fn take(&mut self, due: usize, queued: usize, streaming: bool) -> usize {
        if !self.playing && (queued >= TARGET_FRAMES || !streaming) {
            self.playing = true;
        }
        if !self.playing {
            return 0;
        }
        let take = due.min(queued);
        if take < due && streaming {
            self.playing = false;
        }
        take
    }

    /// A turn that owes `due` frames with `queued` waiting: it plays what
    /// [`Self::take`] gives, silence for the rest, and first drops the
    /// oldest frames that would leave more than [`CAP_FRAMES`] waiting
    /// after it.
    pub fn step(&mut self, due: u64, queued: usize, streaming: bool) -> Step {
        let take = self.take(
            usize::try_from(due).unwrap_or(usize::MAX),
            queued,
            streaming,
        );
        Step {
            dropped: (queued - take).saturating_sub(CAP_FRAMES),
            take,
            silent: due - take as u64,
        }
    }
}

/// The backlog's level over a report period: frames waiting after each
/// turn, at least, on average and at most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Level {
    min: usize,
    max: usize,
    sum: u64,
    samples: u64,
}

/// A [`Level`]'s summary, in frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

impl Level {
    pub fn observe(&mut self, frames: usize) {
        if self.samples == 0 {
            self.min = frames;
            self.max = frames;
        }
        self.min = self.min.min(frames);
        self.max = self.max.max(frames);
        self.sum += frames as u64;
        self.samples += 1;
    }

    /// The period's summary, and a new period; `None` without a turn.
    pub fn take(&mut self) -> Option<Levels> {
        let level = core::mem::take(self);
        (level.samples != 0).then(|| Levels {
            min: level.min,
            avg: (level.sum / level.samples) as usize,
            max: level.max,
        })
    }
}

/// Frames as milliseconds to a tenth: `40.0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Millis(pub usize);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.0 as u64 * 10_000 / RATE;
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}

impl fmt::Display for Levels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}",
            Millis(self.min),
            Millis(self.avg),
            Millis(self.max)
        )
    }
}

/// Microframes a second at high speed.
const MICROFRAMES: u64 = 8000;
/// The nominal feedback: [`RATE`] as 16.16 frames per microframe, 6.0
/// (`stela_soc::usb::audio::FEEDBACK_NOMINAL`).
pub const FEEDBACK_NOMINAL: u32 = ((RATE << 16) / MICROFRAMES) as u32;
/// How far the feedback moves from nominal at most: 5%.
pub const FEEDBACK_SPAN: u32 = FEEDBACK_NOMINAL / 20;
/// The guest's system counts (24 MHz) per microframe.
pub const GUEST_TICKS_PER_MICROFRAME: u64 = 3000;
/// The first measurement of the output's rate against the guest's time
/// spans a second of guest time, the later ones two.
const FIRST_WINDOW_MICROFRAMES: u64 = MICROFRAMES;
const WINDOW_MICROFRAMES: u64 = 2 * MICROFRAMES;
/// Proportional gain: a backlog this many frames off its target moves the
/// feedback by all of nominal, so the whole target (1920 frames) moves it
/// by 2.5%.
const PROPORTIONAL_FRAMES: i128 = 76_800;
/// Integral gain: an error of e frames held while the output consumes c
/// frames adds e x c / this of nominal: 10 ms off target for 10 s of
/// output is 1%. With the proportional gain this is about critically
/// damped, settling in some 15 s.
const INTEGRAL_FRAMES2: i128 = 100 * 480 * 480_000;

/// One look at the stream for [`Feedback::update`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The guest's system count (24 MHz) at the speaker's last bus service.
    pub guest_ticks: u64,
    /// Frames the output consumed since it was made, silence included:
    /// HDMI periods written, or what the sink's clock owed.
    pub output_frames: u64,
    /// Frames waiting.
    pub backlog: usize,
    /// The guest streams and the output consumed without a break since the
    /// last look; otherwise the look only ends the measurement.
    pub running: bool,
}

/// The rate the host's output consumes, told to the guest as the speaker's
/// explicit feedback (16.16 frames per microframe of guest time).
///
/// Two terms: the output's rate measured against the guest's time (frames
/// consumed over guest microframes, over windows of 1 then 2 s, smoothed by
/// a quarter each window), which is what the guest must send; and a PI
/// loop on the backlog's distance from [`TARGET_FRAMES`], which brings it
/// back there and absorbs what the measurement misses. The sum is held to
/// [`FEEDBACK_SPAN`] either side of [`FEEDBACK_NOMINAL`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Feedback {
    value: u32,
    estimate: Option<u32>,
    /// The measurement's start: guest count and output frames.
    window: Option<(u64, u64)>,
    /// Output frames at the last running look.
    last: Option<u64>,
    /// The sum of backlog error times frames consumed, frames squared.
    integral: i128,
}

impl Default for Feedback {
    fn default() -> Self {
        Self {
            value: FEEDBACK_NOMINAL,
            estimate: None,
            window: None,
            last: None,
            integral: 0,
        }
    }
}

impl Feedback {
    /// The feedback to send.
    pub const fn value(&self) -> u32 {
        self.value
    }

    /// The output's rate as last measured against the guest's time.
    pub const fn estimate(&self) -> Option<u32> {
        self.estimate
    }

    /// Looks at the stream once a turn; the feedback to send.
    pub fn update(&mut self, sample: Sample) -> u32 {
        if !sample.running {
            self.window = None;
            self.last = None;
            return self.value;
        }
        let (ticks, frames) = (sample.guest_ticks, sample.output_frames);
        match self.window {
            Some((t0, f0)) if ticks >= t0 && frames >= f0 => {
                let span = if self.estimate.is_some() {
                    WINDOW_MICROFRAMES
                } else {
                    FIRST_WINDOW_MICROFRAMES
                };
                let elapsed = ticks - t0;
                if elapsed >= span * GUEST_TICKS_PER_MICROFRAME {
                    let measured = (u128::from(frames - f0) << 16)
                        * u128::from(GUEST_TICKS_PER_MICROFRAME)
                        / u128::from(elapsed);
                    let measured = clamp(measured.min(u128::from(u32::MAX)) as i128);
                    self.estimate = Some(match self.estimate {
                        None => measured,
                        Some(old) => {
                            (i128::from(old) + (i128::from(measured) - i128::from(old)) / 4) as u32
                        }
                    });
                    self.window = Some((ticks, frames));
                }
            }
            _ => self.window = Some((ticks, frames)),
        }
        let consumed = self.last.map_or(0, |last| frames.saturating_sub(last));
        self.last = Some(frames);
        let error = TARGET_FRAMES as i128 - sample.backlog as i128;
        let nominal = i128::from(FEEDBACK_NOMINAL);
        let span = i128::from(FEEDBACK_SPAN);
        // Anti-windup: the integral term alone stays within the span.
        let bound = span * INTEGRAL_FRAMES2 / nominal;
        self.integral = (self.integral + error * i128::from(consumed)).clamp(-bound, bound);
        let proportional = error * nominal / PROPORTIONAL_FRAMES;
        let integral = self.integral * nominal / INTEGRAL_FRAMES2;
        let base = i128::from(self.estimate.unwrap_or(FEEDBACK_NOMINAL));
        self.value = clamp(base + proportional + integral);
        self.value
    }
}

/// `value` held to [`FEEDBACK_SPAN`] of nominal.
fn clamp(value: i128) -> u32 {
    let nominal = i128::from(FEEDBACK_NOMINAL);
    let span = i128::from(FEEDBACK_SPAN);
    value.clamp(nominal - span, nominal + span) as u32
}

/// A 16.16 frames-per-microframe rate for the log:
/// `0x00061eb8=48960.0Hz(+2.00%)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate(pub u32);

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths_hz = (u64::from(self.0) * MICROFRAMES * 10) >> 16;
        write!(
            f,
            "{:#010x}={}.{}Hz({})",
            self.0,
            tenths_hz / 10,
            tenths_hz % 10,
            Deviation(self.0)
        )
    }
}

/// A 16.16 rate's distance from nominal, in percent to a hundredth:
/// `+2.00%`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deviation(pub u32);

impl fmt::Display for Deviation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nominal = i64::from(FEEDBACK_NOMINAL);
        let hundredths = (i64::from(self.0) - nominal) * 10_000 / nominal;
        let sign = if hundredths < 0 { "-" } else { "+" };
        let magnitude = hundredths.unsigned_abs();
        write!(f, "{sign}{}.{:02}%", magnitude / 100, magnitude % 100)
    }
}

/// How near an upload a FIFO error counts as near: within this many ms
/// before the status read's window, as well as inside it.
pub const UPLOAD_NEAR_MS: u64 = 10;

/// The GPU's frame uploads as an audio turn sees them (`stela-boot`
/// `gpu::display`): uploads finished, when the last one finished (host
/// counter), and whether one runs now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Uploads {
    pub count: u64,
    pub last_end: u64,
    pub busy: bool,
}

/// Whether the HDMI stream's FIFO errors cluster around the GPU's frame
/// uploads (diagnostics only). Each turn that read the running stream's
/// status has a window, from the previous such read to its own; it is near
/// an upload when an upload ran in that window or ended within `near`
/// counts before it. FIFO errors first seen in a turn count near or apart
/// by its window, and every turn counts too, as the baseline: errors that
/// do not care about uploads land near them in the share the turns do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FifoCorrelation {
    /// FIFO errors seen in a turn near an upload, and in one apart.
    pub near: u64,
    pub apart: u64,
    /// Turns near an upload, of all turns counted.
    pub turns_near: u64,
    pub turns: u64,
    /// The previous read: its counter and the uploads finished by then.
    last: Option<(u64, u64)>,
}

impl FifoCorrelation {
    /// A turn read the stream's status at `now`, seeing `uploads` and
    /// `errors` FIFO errors new since the last read; whether its window was
    /// near an upload (`None` for the first read, which has no window).
    pub fn turn(&mut self, now: u64, uploads: Uploads, errors: u64, near: u64) -> Option<bool> {
        let (start, count) = self.last.replace((now, uploads.count))?;
        let close = uploads.busy
            || uploads.count != count
            || (uploads.count != 0 && uploads.last_end.saturating_add(near) >= start);
        self.turns += 1;
        self.turns_near += u64::from(close);
        if close {
            self.near += errors;
        } else {
            self.apart += errors;
        }
        Some(close)
    }

    /// The stream's status went unread (it stopped): the next read starts
    /// a window.
    pub fn interrupt(&mut self) {
        self.last = None;
    }
}

impl fmt::Display for FifoCorrelation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "fifo_near_upload={} fifo_apart={} turns_near_upload={}/{}",
            self.near, self.apart, self.turns_near, self.turns
        )
    }
}

/// The host side's counts since the stream was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    pub consumed: u64,
    pub silent: u64,
    /// Frames the host dropped past the backlog.
    pub trimmed: u64,
}

impl Counters {
    pub fn add(&mut self, step: Step) {
        self.consumed += step.take as u64;
        self.silent += step.silent;
        self.trimmed += step.dropped as u64;
    }
}

/// The one-time line naming the speaker's endpoint mode.
pub const fn endpoint_mode(asynchronous: bool) -> &'static str {
    if asynchronous {
        "GUEST AUDIO endpoint: asynchronous with explicit feedback (--guest-audio-async): \
         isochronous OUT 0x01, up to 54 frames a 1 ms packet, paired with the isochronous IN \
         feedback endpoint 0x81 (16.16 frames per microframe, every 1 ms); the feedback is the \
         output's rate measured against the guest's time plus a PI loop on the backlog (target \
         40 ms, cap 80 ms), within 5% of 48 kHz"
    } else {
        "GUEST AUDIO endpoint: adaptive, no feedback: the guest's clock paces its frames (up to 49 \
         a 1 ms packet); the backlog fills to 40 ms before it plays, a dry one plays silence and \
         more than 80 ms is dropped; the feedback the output's rate asks is reported unsent \
         (--guest-audio-async sends it)"
    }
}

/// The `GUEST AUDIO` line: the function's counts and controls beside the
/// host's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    /// The streaming setting's bit resolution, 0 while the guest is idle.
    pub bits: u8,
    /// Frames the guest sent.
    pub received: u64,
    /// Frames the function dropped at its full ring.
    pub overflowed: u64,
    /// Bytes of packets that split a frame.
    pub split: u64,
    pub queued: usize,
    pub muted: bool,
    /// 1/256 dB.
    pub volume: i16,
    pub host: Counters,
    /// Since the last report period: silence played while the guest
    /// streamed, and frames dropped (at the function's ring or past the
    /// cap).
    pub interval_silent: u64,
    pub interval_dropped: u64,
    /// The backlog's level over the period, `None` without a turn.
    pub backlog: Option<Levels>,
    /// The speaker's endpoint is asynchronous, so the feedback is sent.
    pub asynchronous: bool,
    /// The feedback (16.16 frames per microframe), and the output's rate
    /// as measured against the guest's time.
    pub feedback: u32,
    pub measured: Option<u32>,
    /// Packets that carried frames, and feedback packets the guest read.
    pub packets: u64,
    pub feedback_reads: u64,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let volume = i32::from(self.volume);
        let (sign, magnitude) = if volume < 0 {
            ("-", -volume)
        } else {
            ("", volume)
        };
        // Tenths of a dB, rounded toward zero: the steps are 0.5 dB.
        let tenths = magnitude * 10 / 256;
        write!(
            f,
            "GUEST AUDIO frames={} consumed={} silent={} dropped={} split={} queued={} \
             bits={} muted={} volume={sign}{}.{}dB",
            self.received,
            self.host.consumed,
            self.host.silent,
            self.overflowed + self.host.trimmed,
            self.split,
            self.queued,
            self.bits,
            self.muted,
            tenths / 10,
            tenths % 10,
        )?;
        write!(
            f,
            " +silent={} +dropped={} backlog_ms=",
            self.interval_silent, self.interval_dropped
        )?;
        match self.backlog {
            Some(levels) => write!(f, "{levels}")?,
            None => f.write_str("-")?,
        }
        write!(
            f,
            " feedback={}{} measured=",
            Rate(self.feedback),
            if self.asynchronous {
                ""
            } else {
                "(unsent:adaptive)"
            }
        )?;
        match self.measured {
            Some(rate) => write!(f, "{}", Deviation(rate))?,
            None => f.write_str("-")?,
        }
        write!(
            f,
            " packets={} feedback_reads={}",
            self.packets, self.feedback_reads
        )
    }
}

/// The speakers an ELD's allocation byte names (CEA-861 Speaker
/// Allocation Data Block): `FL/FR LFE FC`, or `none`.
pub fn speakers(allocation: u8) -> alloc::string::String {
    const NAMES: [&str; 7] = ["FL/FR", "LFE", "FC", "RL/RR", "RC", "FLC/FRC", "RLC/RRC"];
    let names: alloc::vec::Vec<&str> = NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| allocation & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect();
    if names.is_empty() {
        alloc::string::String::from("none")
    } else {
        names.join(" ")
    }
}

/// One CEA-861 Short Audio Descriptor as the ELD's parse gives it:
/// `LPCM 2ch 32/44.1/48kHz 16/20/24-bit`, or the coding type's number for
/// a compressed format.
pub fn audio_descriptor(
    coding: u8,
    channels: u8,
    rates: u8,
    sample_bits: u8,
) -> alloc::string::String {
    use core::fmt::Write;
    const RATES: [&str; 7] = ["32", "44.1", "48", "88.2", "96", "176.4", "192"];
    const BITS: [&str; 3] = ["16", "20", "24"];
    let pick = |mask: u8, names: &[&'static str]| -> alloc::vec::Vec<&'static str> {
        names
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect()
    };
    let mut text = alloc::string::String::new();
    let _ = match coding {
        1 => write!(text, "LPCM"),
        coding => write!(text, "coding{coding}"),
    };
    let _ = write!(text, " {channels}ch {}kHz", pick(rates, &RATES).join("/"));
    if coding == 1 {
        let _ = write!(text, " {}-bit", pick(sample_bits, &BITS).join("/"));
    }
    text
}

/// Writes `frames`, 32-bit MSB-aligned stereo, into `out` as 16-bit
/// little-endian stereo (each sample's top 16 bits), and silence after
/// them; the frames written. `out` holds whole 4-byte frames, and frames
/// beyond it are left unread.
pub fn s16_period(frames: impl IntoIterator<Item = [i32; 2]>, out: &mut [u8]) -> usize {
    let mut written = 0;
    let mut frames = frames.into_iter();
    for slot in out.as_chunks_mut::<4>().0 {
        let [left, right] = match frames.next() {
            Some(frame) => {
                written += 1;
                frame.map(|sample| (sample >> 16) as i16)
            }
            None => [0, 0],
        };
        slot[..2].copy_from_slice(&left.to_le_bytes());
        slot[2..].copy_from_slice(&right.to_le_bytes());
    }
    written
}

/// An HD Audio stream descriptor's status byte (SDnSTS, HDA 1.0a 3.3.36),
/// in hex with its bits named, for the log: BCIS (bit 2), FIFOE (bit 3, a
/// FIFO error: an output stream's DMA did not keep its FIFO fed), DESE
/// (bit 4, a descriptor error: a BDL or buffer fetch failed) and FIFORDY
/// (bit 5); bits 0, 1, 6 and 7 are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamStatus(pub u8);

impl StreamStatus {
    /// A FIFO error (FIFOE) without a descriptor error (DESE): the HDMI
    /// stream restarts alone ([`RestartReason::DmaFault`]).
    pub const fn fifo_error_only(self) -> bool {
        self.0 & (1 << 3) != 0 && self.0 & (1 << 4) == 0
    }
}

impl fmt::Display for StreamStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const BITS: [(u8, &str); 4] = [
            (1 << 2, "BCIS (buffer completion)"),
            (
                1 << 3,
                "FIFOE (FIFO error: the DMA did not keep the output FIFO fed)",
            ),
            (
                1 << 4,
                "DESE (descriptor error: a BDL or buffer fetch failed)",
            ),
            (1 << 5, "FIFORDY (FIFO ready)"),
        ];
        write!(f, "{:#04x} =", self.0)?;
        let mut first = true;
        for (bit, name) in BITS {
            if self.0 & bit != 0 {
                write!(f, "{} {name}", if first { "" } else { " |" })?;
                first = false;
            }
        }
        let reserved = self.0 & !0x3c;
        if reserved != 0 {
            write!(
                f,
                "{} reserved {reserved:#04x}",
                if first { "" } else { " |" }
            )?;
            first = false;
        }
        if first {
            f.write_str(" none")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn a_pacer_owes_48_frames_a_millisecond_and_keeps_the_remainder() {
        // A 3 MHz counter: one frame is 62.5 ticks.
        let mut pacer = Pacer::new(3_000_000);
        assert_eq!(pacer.due(1_000), 0, "the first call starts the clock");
        assert_eq!(pacer.due(4_000), 48);
        // 125 ticks are two frames; 63 then 62 ticks are one each, as the
        // half frame carries over.
        assert_eq!(pacer.due(4_063), 1);
        assert_eq!(pacer.due(4_125), 1);
        // A second in many small steps is exactly 48000 frames.
        let mut pacer = Pacer::new(3_000_000);
        pacer.due(0);
        let owed: u64 = (1..=3_000).map(|n| pacer.due(n * 1_000)).sum();
        assert_eq!(owed, RATE);
    }

    #[test]
    fn a_stream_status_is_printed_in_hex_with_its_bits_named() {
        // audio7's StreamDma { status: 40 } is decimal: 0x28.
        assert_eq!(
            StreamStatus(40).to_string(),
            "0x28 = FIFOE (FIFO error: the DMA did not keep the output FIFO fed) | FIFORDY \
             (FIFO ready)"
        );
        assert_eq!(
            StreamStatus(0x14).to_string(),
            "0x14 = BCIS (buffer completion) | DESE (descriptor error: a BDL or buffer fetch \
             failed)"
        );
        assert_eq!(StreamStatus(0x40).to_string(), "0x40 = reserved 0x40");
        assert_eq!(StreamStatus(0).to_string(), "0x00 = none");
        // FIFOE alone restarts the stream; with DESE, or without FIFOE, not.
        assert!(StreamStatus(0x28).fifo_error_only());
        assert!(StreamStatus(0x0c).fifo_error_only());
        assert!(!StreamStatus(0x18).fifo_error_only());
        assert!(!StreamStatus(0x14).fifo_error_only());
        assert!(!StreamStatus(0x20).fifo_error_only());
    }

    #[test]
    fn the_test_tone_is_a_1_khz_sine_at_minus_12_dbfs_on_both_channels() {
        let mut tone = Tone::default();
        // One second, in 10 ms periods, as the HDMI ring is refilled.
        let mut second = alloc::vec![0u8; RATE as usize * 4];
        for period in second.chunks_mut(480 * 4) {
            tone.fill(period);
        }
        let frames: alloc::vec::Vec<[i16; 2]> = second
            .as_chunks::<4>()
            .0
            .iter()
            .map(|f| {
                [
                    i16::from_le_bytes([f[0], f[1]]),
                    i16::from_le_bytes([f[2], f[3]]),
                ]
            })
            .collect();
        // Both channels carry the same samples.
        assert!(frames.iter().all(|[left, right]| left == right));
        let samples: alloc::vec::Vec<i16> = frames.iter().map(|f| f[0]).collect();
        // Frequency: 1000 rising zero crossings a second, one every 48
        // samples, the phase carried across periods.
        let rising: alloc::vec::Vec<usize> = (1..samples.len())
            .filter(|&k| samples[k - 1] < 0 && samples[k] >= 0)
            .collect();
        assert_eq!(rising.len() + 1, TONE_HZ as usize);
        assert!(rising.windows(2).all(|pair| pair[1] - pair[0] == 48));
        assert_eq!(rising[0], 48);
        // Level: the peak is -12 dBFS, at a quarter cycle; the shape a sine.
        assert_eq!(samples[12], TONE_PEAK);
        assert_eq!(samples[36], -TONE_PEAK);
        assert_eq!((samples[0], samples[24]), (0, 0));
        assert_eq!(peak(&second), TONE_PEAK as u32);
        assert_eq!(Dbfs(peak(&second)).tenths(), Some(TONE_TENTHS_DBFS));
        assert_eq!(Dbfs(peak(&second)).to_string(), "-12.0dBFS");
        // sin(30 degrees) = 1/2, within a rounding step.
        assert!((i32::from(samples[4]) - i32::from(TONE_PEAK) / 2).abs() <= 1);
        // RMS: the peak over the square root of two, -15.0 dBFS.
        let mean_square = samples
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / samples.len() as f64;
        let ratio = mean_square / (f64::from(TONE_PEAK) * f64::from(TONE_PEAK) / 2.0);
        assert!((ratio - 1.0).abs() < 1e-3, "{ratio}");
    }

    #[test]
    fn a_peak_is_printed_in_dbfs_to_a_tenth() {
        let pcm = |samples: &[i16]| -> alloc::vec::Vec<u8> {
            samples.iter().flat_map(|s| s.to_le_bytes()).collect()
        };
        assert_eq!(peak(&pcm(&[3, -700, 12, 699])), 700);
        assert_eq!(peak(&pcm(&[i16::MIN, 5])), 32768);
        assert_eq!(peak(&[]), 0);
        for (sample, text) in [
            (32768, "0.0dBFS"),
            (32767, "0.0dBFS"),
            (16384, "-6.0dBFS"),
            (8231, "-12.0dBFS"),
            (3277, "-20.0dBFS"),
            (1, "-90.3dBFS"),
            (0, "-inf"),
        ] {
            assert_eq!(Dbfs(sample).to_string(), text, "{sample}");
        }
    }

    #[test]
    fn a_pacer_caps_a_long_gap_and_restarts_after_a_stop() {
        let mut pacer = Pacer::new(1_000_000);
        pacer.due(0);
        assert_eq!(pacer.due(5_000_000), MAX_STEP_FRAMES);
        assert_eq!(pacer.due(5_001_000), 48);
        // A counter that went back owes nothing.
        assert_eq!(pacer.due(5_000_500), 0);
        pacer.stop();
        assert_eq!(pacer.due(9_000_000), 0);
        assert_eq!(pacer.due(9_010_000), 480);
    }

    /// A playing backlog after it filled.
    fn playing() -> Backlog {
        let mut backlog = Backlog::default();
        assert_eq!(backlog.take(0, TARGET_FRAMES, true), 0);
        assert!(!backlog.filling());
        backlog
    }

    #[test]
    fn a_step_plays_what_waits_pads_silence_and_trims_past_the_cap() {
        assert_eq!((TARGET_FRAMES, CAP_FRAMES), (1920, 3840));
        // Enough waiting: play what is owed, keep the rest.
        assert_eq!(
            playing().step(480, 600, true),
            Step {
                dropped: 0,
                take: 480,
                silent: 0
            }
        );
        // Too little: silence for the rest.
        assert_eq!(
            playing().step(480, 100, true),
            Step {
                dropped: 0,
                take: 100,
                silent: 380
            }
        );
        // More than the cap would remain: the oldest go first.
        assert_eq!(
            playing().step(480, 480 + CAP_FRAMES + 7, true),
            Step {
                dropped: 7,
                take: 480,
                silent: 0
            }
        );
        // Up to the cap waits: nothing dropped.
        assert_eq!(playing().step(480, 480 + CAP_FRAMES, true).dropped, 0);
        assert_eq!(playing().step(0, 0, true), Step::default());
        let mut counters = Counters::default();
        counters.add(playing().step(480, 100, true));
        counters.add(playing().step(480, 480 + CAP_FRAMES + 7, true));
        assert_eq!(
            counters,
            Counters {
                consumed: 580,
                silent: 380,
                trimmed: 7
            }
        );
    }

    /// A stream fills to the 40 ms target before it plays, and again after
    /// it ran dry; jitter up to the target then never pads. A guest that
    /// stopped streaming has its last frames played out.
    #[test]
    fn the_backlog_fills_to_its_target_then_rides_out_jitter() {
        let mut backlog = Backlog::default();
        assert!(backlog.filling());
        // Filling: 10 ms periods of silence while the frames gather.
        assert_eq!(
            backlog.step(480, 1000, true),
            Step {
                dropped: 0,
                take: 0,
                silent: 480
            }
        );
        assert_eq!(backlog.step(480, TARGET_FRAMES - 1, true).take, 0);
        // At the target it plays.
        assert_eq!(backlog.step(480, TARGET_FRAMES, true).take, 480);
        // Bursty delivery: 0 to 30 ms a turn, 10 ms on average, against 10
        // ms periods. The 40 ms backlog absorbs it: no silence.
        let mut queued = TARGET_FRAMES - 480;
        let mut silent = 0;
        for turn in 0..1000 {
            queued += [0, 1440, 0, 480, 0, 960, 0, 960][turn % 8];
            let step = backlog.step(480, queued, true);
            queued -= step.take + step.dropped;
            silent += step.silent;
        }
        assert_eq!(silent, 0);
        assert!(!backlog.filling());
        // Run dry: what waits plays, then it fills again.
        assert_eq!(backlog.step(480, 100, true).take, 100);
        assert!(backlog.filling());
        assert_eq!(backlog.step(480, 900, true).take, 0);
        // The guest stopped: what it left plays out, without filling.
        assert_eq!(backlog.step(480, 900, false).take, 480);
        assert_eq!(backlog.step(480, 420, false).take, 420);
        assert!(!backlog.filling());
    }

    #[test]
    fn the_level_is_summed_up_per_report_period() {
        let mut level = Level::default();
        assert_eq!(level.take(), None);
        for frames in [1920, 960, 2880, 1920] {
            level.observe(frames);
        }
        let levels = level.take().unwrap();
        assert_eq!(
            levels,
            Levels {
                min: 960,
                avg: 1920,
                max: 2880
            }
        );
        assert_eq!(levels.to_string(), "20.0/40.0/60.0");
        assert_eq!(level.take(), None, "a new period");
        level.observe(5);
        assert_eq!(level.take().unwrap().to_string(), "0.1/0.1/0.1");
    }

    /// The sink of a simulation: 48 kHz of wall time, against a guest whose
    /// time runs `ratio` of wall time (under 1: the engine held it while a
    /// PE served), sending the feedback's frames per microframe of its
    /// time, delivered to the host in bursts. Returns the feedback, the
    /// mean backlog of the last 10 s, and the silence and drops of the
    /// last 20 s, after `seconds`.
    fn simulate(ratio: f64, seconds: u64) -> (u32, f64, u64, u64) {
        const TURN_US: u64 = 5_000;
        let mut feedback = Feedback::default();
        let mut backlog = Backlog::default();
        let mut queued = 0usize;
        let (mut guest_ticks, mut guest_frames) = (0f64, 0f64);
        let mut sink_owed = 0f64;
        let mut output = 0u64;
        let (mut level, mut levels) = (0f64, 0u64);
        let (mut silent, mut dropped) = (0u64, 0u64);
        let turns = seconds * 1_000_000 / TURN_US;
        for turn in 0..turns {
            // Turns 2 to 8 ms apart, 5 ms on average.
            let wall_us = [2_000, 8_000, 5_000, 3_000, 7_000][(turn % 5) as usize] as f64;
            let microframes = wall_us / 125.0 * ratio;
            guest_ticks += microframes * GUEST_TICKS_PER_MICROFRAME as f64;
            guest_frames += microframes * f64::from(feedback.value()) / 65536.0;
            let delivered = guest_frames.floor();
            guest_frames -= delivered;
            queued += delivered as usize;
            sink_owed += wall_us * RATE as f64 / 1e6;
            let due = sink_owed.floor();
            sink_owed -= due;
            output += due as u64;
            let step = backlog.step(due as u64, queued, true);
            queued -= step.take + step.dropped;
            feedback.update(Sample {
                guest_ticks: guest_ticks as u64,
                output_frames: output,
                backlog: queued,
                running: true,
            });
            if turn >= turns - 20_000_000 / TURN_US {
                silent += step.silent;
                dropped += step.dropped as u64;
            }
            if turn >= turns - 10_000_000 / TURN_US {
                level += queued as f64;
                levels += 1;
            }
        }
        (feedback.value(), level / levels as f64, silent, dropped)
    }

    /// The feedback converges on what the output consumes per microframe
    /// of the guest's time, for a guest 3.5% slow (audio9's case: guest
    /// time held while PEs served) or 3% fast, and holds the backlog at
    /// its 40 ms target with no silence and no drops; a guest further off
    /// than 5% pins it at the bound.
    #[test]
    fn the_feedback_loop_converges_on_a_slow_or_fast_guest_clock() {
        for ratio in [0.965, 1.03, 1.0] {
            let (value, level, silent, dropped) = simulate(ratio, 60);
            let ideal = f64::from(FEEDBACK_NOMINAL) / ratio;
            let off = (f64::from(value) - ideal) / ideal;
            assert!(off.abs() < 0.002, "ratio {ratio}: {value:#x} vs {ideal}");
            assert!(
                (level - TARGET_FRAMES as f64).abs() < 96.0,
                "ratio {ratio}: level {level}"
            );
            assert_eq!((silent, dropped), (0, 0), "ratio {ratio}");
        }
        // 10% slow: the feedback stops at +5%, the rest still pads.
        let (value, _, silent, _) = simulate(0.9, 30);
        assert_eq!(value, FEEDBACK_NOMINAL + FEEDBACK_SPAN);
        assert!(silent > 0);
    }

    #[test]
    fn the_feedback_measures_the_output_against_the_guests_time() {
        let mut feedback = Feedback::default();
        assert_eq!(feedback.value(), 6 << 16);
        let at = |ms: u64, frames: u64, backlog: usize| Sample {
            guest_ticks: ms * 8 * GUEST_TICKS_PER_MICROFRAME,
            output_frames: frames,
            backlog,
            running: true,
        };
        // On target: the measurement alone. 49000 frames in a second of
        // guest time: 6.125 a microframe.
        feedback.update(at(0, 0, TARGET_FRAMES));
        assert_eq!(feedback.update(at(500, 24_500, TARGET_FRAMES)), 6 << 16);
        assert_eq!(feedback.estimate(), None);
        assert_eq!(feedback.update(at(1000, 49_000, TARGET_FRAMES)), 0x6_2000);
        assert_eq!(feedback.estimate(), Some(0x6_2000));
        // A break ends the measurement and freezes the value.
        let stopped = Sample {
            running: false,
            ..at(5000, 0, 0)
        };
        assert_eq!(feedback.update(stopped), 0x6_2000);
        // Later windows span 2 s and move the estimate a quarter of the
        // way: 48000 frames a second now.
        feedback.update(at(10_000, 100_000, TARGET_FRAMES));
        feedback.update(at(11_000, 148_000, TARGET_FRAMES));
        assert_eq!(feedback.estimate(), Some(0x6_2000));
        feedback.update(at(12_000, 196_000, TARGET_FRAMES));
        assert_eq!(feedback.estimate(), Some(0x6_1800));
        // A backlog 20 ms short asks for more at once: 960/76800 = 1.25%.
        let more = feedback.update(at(12_001, 196_048, TARGET_FRAMES - 960));
        assert!(
            more > 0x6_1800 + 4900 && more < 0x6_1800 + 5000,
            "{more:#x}"
        );
        // An empty backlog asks for 2.5% more than the estimate; a full
        // one would ask for far less, held at 5% under nominal.
        let most = feedback.update(at(12_002, 196_096, 0));
        assert!(
            most > 0x6_1800 + 9800 && most < 0x6_1800 + 9900,
            "{most:#x}"
        );
        assert_eq!(
            feedback.update(at(12_003, 196_144, 100_000)),
            FEEDBACK_NOMINAL - FEEDBACK_SPAN
        );
    }

    #[test]
    fn a_rate_is_printed_in_hz_and_percent_of_nominal() {
        assert_eq!(Rate(6 << 16).to_string(), "0x00060000=48000.0Hz(+0.00%)");
        assert_eq!(Rate(0x6_2000).to_string(), "0x00062000=49000.0Hz(+2.08%)");
        assert_eq!(Rate(0x5_e000).to_string(), "0x0005e000=47000.0Hz(-2.08%)");
        assert_eq!(
            Rate(FEEDBACK_NOMINAL + FEEDBACK_SPAN).to_string(),
            "0x00064ccc=50399.9Hz(+4.99%)"
        );
        assert_eq!(Deviation(FEEDBACK_NOMINAL - 3933).to_string(), "-1.00%");
        assert_eq!(Deviation(FEEDBACK_NOMINAL - 1).to_string(), "+0.00%");
    }

    /// FIFO errors count near an upload when one ran in the window since
    /// the last status read, or ended within 10 ms before it; every turn
    /// counts as the baseline. A stop starts a new window.
    #[test]
    fn fifo_errors_are_counted_near_or_apart_from_uploads() {
        const MS: u64 = 24_000;
        let near = UPLOAD_NEAR_MS * MS;
        let mut c = FifoCorrelation::default();
        let none = Uploads::default();
        // The first read has no window.
        assert_eq!(c.turn(100 * MS, none, 1, near), None);
        // No upload ever: apart.
        assert_eq!(c.turn(105 * MS, none, 2, near), Some(false));
        // One upload finished inside the window.
        let one = Uploads {
            count: 1,
            last_end: 107 * MS,
            busy: false,
        };
        assert_eq!(c.turn(110 * MS, one, 1, near), Some(true));
        // It ended 3, then 8 ms before the window: still near.
        assert_eq!(c.turn(115 * MS, one, 0, near), Some(true));
        assert_eq!(c.turn(118 * MS, one, 0, near), Some(true));
        // 11 ms before: apart.
        assert_eq!(c.turn(130 * MS, one, 4, near), Some(false));
        // One under way.
        let busy = Uploads { busy: true, ..one };
        assert_eq!(c.turn(135 * MS, busy, 3, near), Some(true));
        assert_eq!(
            c,
            FifoCorrelation {
                near: 4,
                apart: 6,
                turns_near: 4,
                turns: 6,
                last: Some((135 * MS, 1)),
            }
        );
        assert_eq!(
            c.to_string(),
            "fifo_near_upload=4 fifo_apart=6 turns_near_upload=4/6"
        );
        c.interrupt();
        assert_eq!(c.turn(500 * MS, busy, 9, near), None);
        assert_eq!(c.turns, 6);
    }

    #[test]
    fn the_report_line_names_every_count_and_the_volume_in_db() {
        let report = Report {
            bits: 16,
            received: 96_000,
            overflowed: 3,
            split: 0,
            queued: 240,
            muted: false,
            volume: -12 * 256 - 128,
            host: Counters {
                consumed: 95_000,
                silent: 10,
                trimmed: 4,
            },
            interval_silent: 10,
            interval_dropped: 3,
            backlog: Some(Levels {
                min: 960,
                avg: 1920,
                max: 2400,
            }),
            asynchronous: true,
            feedback: 0x6_2000,
            measured: Some(0x6_1800),
            packets: 2000,
            feedback_reads: 1990,
        };
        assert_eq!(
            report.to_string(),
            "GUEST AUDIO frames=96000 consumed=95000 silent=10 dropped=7 split=0 queued=240 \
             bits=16 muted=false volume=-12.5dB +silent=10 +dropped=3 backlog_ms=20.0/40.0/50.0 \
             feedback=0x00062000=49000.0Hz(+2.08%) measured=+1.56% packets=2000 \
             feedback_reads=1990"
        );
        let unity = Report {
            volume: 0,
            muted: true,
            bits: 0,
            backlog: None,
            asynchronous: false,
            measured: None,
            ..report
        };
        assert!(
            unity
                .to_string()
                .contains("bits=0 muted=true volume=0.0dB +silent=10 +dropped=3 backlog_ms=- ")
        );
        assert!(
            unity
                .to_string()
                .contains("feedback=0x00062000=49000.0Hz(+2.08%)(unsent:adaptive) measured=- ")
        );
        assert!(endpoint_mode(true).contains("asynchronous with explicit feedback"));
        assert!(endpoint_mode(false).starts_with("GUEST AUDIO endpoint: adaptive, no feedback"));
    }

    #[test]
    fn speakers_and_descriptors_read_as_cea_861_names_them() {
        assert_eq!(speakers(0x01), "FL/FR");
        assert_eq!(speakers(0x0b), "FL/FR LFE RL/RR");
        assert_eq!(speakers(0x7f), "FL/FR LFE FC RL/RR RC FLC/FRC RLC/RRC");
        assert_eq!(speakers(0), "none");
        assert_eq!(
            audio_descriptor(1, 2, 0x07, 0x07),
            "LPCM 2ch 32/44.1/48kHz 16/20/24-bit"
        );
        assert_eq!(
            audio_descriptor(1, 8, 0x7f, 0x01),
            "LPCM 8ch 32/44.1/48/88.2/96/176.4/192kHz 16-bit"
        );
        assert_eq!(audio_descriptor(2, 6, 0x04, 0), "coding2 6ch 48kHz");
    }

    #[test]
    fn the_hdmi_ring_rides_out_what_the_guests_ring_does() {
        // 10 ms periods: 200 ms of the guest's ring, a period of refill lag
        // and one of guard.
        assert_eq!(hdmi_periods(480), 22);
        for period in [128, 256, 480, 960, 1920] {
            let periods = hdmi_periods(period);
            // Less the refill lag and the guard, the ring still covers
            // the guest's ring...
            assert!((periods - 2) * period >= MAX_STEP_FRAMES as usize);
            // ...and not a period more.
            assert!((periods - 3) * period < MAX_STEP_FRAMES as usize);
        }
    }

    #[test]
    fn restarts_count_by_reason() {
        let mut restarts = Restarts::default();
        assert_eq!(restarts.add(RestartReason::Underrun), 1);
        assert_eq!(restarts.add(RestartReason::Underrun), 2);
        assert_eq!(restarts.add(RestartReason::SinkAway), 1);
        assert_eq!(restarts.add(RestartReason::CodecFault), 1);
        assert_eq!(restarts.add(RestartReason::DmaFault), 1);
        assert_eq!(restarts.total(), 5);
        assert_eq!(
            restarts.to_string(),
            "restarts=5 underrun=2 sink_away=1 codec_fault=1 dma_fault=1 route_lost=0"
        );
        assert_eq!(RestartReason::RouteLost.name(), "route_lost");
        assert_eq!(RestartReason::DmaFault.name(), "dma_fault");
        assert!(
            RestartReason::DmaFault
                .action()
                .ends_with("restarting the stream only")
        );
        assert!(
            RestartReason::Underrun
                .action()
                .ends_with("restarting the stream only")
        );
    }

    #[test]
    fn a_count_is_logged_at_its_first_four_then_each_power_of_two_however_it_grows() {
        let logged_counts: alloc::vec::Vec<u64> = (1..=40).filter(|&n| logged(n)).collect();
        assert_eq!(logged_counts, [1, 2, 3, 4, 8, 16, 32]);
        // One at a time, `crossed` is `logged` of the new count.
        for n in 1..=1000 {
            assert_eq!(crossed(n - 1, n), logged(n), "{n}");
        }
        // Several at once: a logged count passed in between still prints.
        assert!(crossed(5, 9));
        assert!(crossed(2, 7));
        assert!(!crossed(9, 15));
        assert!(crossed(9, 40));
        assert!(!crossed(7, 7));
        assert!(!crossed(8, 3));
    }

    #[test]
    fn a_period_keeps_each_samples_top_16_bits_and_pads_silence() {
        let mut out = [0xaa; 16];
        let frames = [[0x1234_5678, -0x1234_5678], [i32::MIN, i32::MAX]];
        assert_eq!(s16_period(frames, &mut out), 2);
        assert_eq!(
            out,
            [
                0x34, 0x12, 0xcb, 0xed, 0x00, 0x80, 0xff, 0x7f, 0, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        // More frames than the period holds: the rest stay unread.
        let mut out = [0; 4];
        let mut frames = [[1 << 16, 2 << 16], [3 << 16, 4 << 16]].into_iter();
        assert_eq!(s16_period(frames.by_ref(), &mut out), 1);
        assert_eq!(out, [1, 0, 2, 0]);
        assert_eq!(frames.next(), Some([3 << 16, 4 << 16]));
    }
}
