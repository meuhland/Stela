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
    /// sense, a verb timeout). The stream alone restarts, a bounded number
    /// of times in a row.
    CodecFault,
    /// The route itself was lost (another ELD, a failed access, the bound
    /// of codec faults). It is prepared again, the codec walked.
    RouteLost,
}

impl RestartReason {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Underrun => "underrun",
            Self::SinkAway => "sink_away",
            Self::CodecFault => "codec_fault",
            Self::RouteLost => "route_lost",
        }
    }

    /// What the playback does about it, for the stop line.
    pub const fn action(self) -> &'static str {
        match self {
            Self::Underrun => {
                "the DMA ran past the refilled periods (a service gap longer than the ring); \
                 restarting the stream only"
            }
            Self::SinkAway => {
                "the pin senses no monitor with a valid ELD (asleep or unplugged); paused, \
                 the route stays prepared until it returns"
            }
            Self::CodecFault => {
                "the codec answered what the route did not set; restarting the stream only"
            }
            Self::RouteLost => "the route was lost; preparing it again",
        }
    }
}

/// The HDMI playback's restarts, by reason.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Restarts {
    pub underrun: u64,
    pub sink_away: u64,
    pub codec_fault: u64,
    pub route_lost: u64,
}

impl Restarts {
    /// Counts one restart for `reason`: its reason's count after it.
    pub fn add(&mut self, reason: RestartReason) -> u64 {
        let count = match reason {
            RestartReason::Underrun => &mut self.underrun,
            RestartReason::SinkAway => &mut self.sink_away,
            RestartReason::CodecFault => &mut self.codec_fault,
            RestartReason::RouteLost => &mut self.route_lost,
        };
        *count += 1;
        *count
    }

    pub const fn total(&self) -> u64 {
        self.underrun + self.sink_away + self.codec_fault + self.route_lost
    }
}

impl fmt::Display for Restarts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "restarts={} underrun={} sink_away={} codec_fault={} route_lost={}",
            self.total(),
            self.underrun,
            self.sink_away,
            self.codec_fault,
            self.route_lost
        )
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
        assert_eq!(restarts.total(), 4);
        assert_eq!(
            restarts.to_string(),
            "restarts=4 underrun=2 sink_away=1 codec_fault=1 route_lost=0"
        );
        assert_eq!(RestartReason::RouteLost.name(), "route_lost");
        assert!(
            RestartReason::Underrun
                .action()
                .ends_with("restarting the stream only")
        );
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
