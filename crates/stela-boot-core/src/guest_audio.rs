//! The host's side of the guest's USB speaker (`stela_soc::usb::audio`):
//! how many of its frames a service turn takes, and the line that reports
//! them.
//!
//! The guest's frames come at 48 kHz of guest time; the host consumes them
//! at 48 kHz of its own clock, the counter for a sink or the HDA link for
//! an HDMI stream. Nothing resamples. A turn that owes more frames than wait
//! plays silence for the rest, and frames beyond [`BACKLOG_FRAMES`] after a
//! turn are dropped, oldest first, so the guest's sound never trails the
//! host's clock by more than that.
use core::fmt;

/// The only sample rate.
pub const RATE: u64 = 48_000;
/// The guest's frames a turn leaves waiting at most: 20 ms.
pub const BACKLOG_FRAMES: usize = RATE as usize / 50;
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

/// A turn that owes `due` frames with `queued` waiting: it plays what is
/// there up to `due`, silence for the rest, and first drops the oldest
/// frames that would leave more than [`BACKLOG_FRAMES`] waiting after it.
pub fn step(due: u64, queued: usize) -> Step {
    let take = usize::try_from(due).unwrap_or(usize::MAX).min(queued);
    Step {
        dropped: (queued - take).saturating_sub(BACKLOG_FRAMES),
        take,
        silent: due - take as u64,
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

    #[test]
    fn a_step_plays_what_waits_pads_silence_and_trims_the_backlog() {
        // Enough waiting: play what is owed, keep the rest.
        assert_eq!(
            step(480, 600),
            Step {
                dropped: 0,
                take: 480,
                silent: 0
            }
        );
        // Too little: silence for the rest.
        assert_eq!(
            step(480, 100),
            Step {
                dropped: 0,
                take: 100,
                silent: 380
            }
        );
        // More than the backlog would remain: the oldest go first.
        assert_eq!(
            step(480, 480 + BACKLOG_FRAMES + 7),
            Step {
                dropped: 7,
                take: 480,
                silent: 0
            }
        );
        assert_eq!(step(0, 0), Step::default());
        let mut counters = Counters::default();
        counters.add(step(480, 100));
        counters.add(step(480, 480 + BACKLOG_FRAMES + 7));
        assert_eq!(
            counters,
            Counters {
                consumed: 580,
                silent: 380,
                trimmed: 7
            }
        );
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
        };
        assert_eq!(
            report.to_string(),
            "GUEST AUDIO frames=96000 consumed=95000 silent=10 dropped=7 split=0 queued=240 \
             bits=16 muted=false volume=-12.5dB"
        );
        let unity = Report {
            volume: 0,
            muted: true,
            bits: 0,
            ..report
        };
        assert!(
            unity
                .to_string()
                .ends_with("bits=0 muted=true volume=0.0dB")
        );
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
