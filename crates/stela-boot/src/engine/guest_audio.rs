//! The guest's USB speaker on the host (`--guest-audio`,
//! `stela_soc::usb::audio`): each service turn takes the frames macOS sent
//! since the last one, under the devices' lock.
//!
//! The host's output is a sink that consumes 48 kHz of the host's counter
//! (`stela_boot_core::guest_audio::Pacer`): what the guest sent is counted
//! and discarded, a turn that owes more than waits counts silence, and more
//! than 80 ms left waiting is dropped, oldest first (`Backlog`: a stream
//! fills to 40 ms before it plays, and again after it ran dry). The
//! `GUEST AUDIO` line reports the counts every report period when they
//! changed, with the period's silence and drops, the backlog's level and
//! the feedback.
//!
//! Each turn also measures the output's rate against the guest's time and
//! regulates the backlog (`guest_audio::Feedback`). With
//! `--guest-audio-async` the speaker's endpoint is asynchronous and that
//! value is its explicit feedback, so macOS sends what the output consumes;
//! otherwise it is only reported.
//!
//! With the option, the HD Audio function of the GPU whose start reset its
//! bus is claimed once that start finished, and its HDMI codec's pins and
//! the monitor's ELD reported (`claim_display_audio`).
//!
//! With `--guest-audio-hdmi` the output is the monitor instead: one HDMI
//! playback on the pin and device entry whose ELD the GPU wrote, 48 kHz
//! 16-bit stereo in 22 periods of 10 ms (`Hdmi`, `HDMI_PERIODS`). Each turn
//! moves the guest's frames to a host queue under the devices' lock, then,
//! in the loader's root without waiting for the host drivers' lock, writes
//! each period the DMA completed: the queue's oldest frames, silence for
//! any it lacks (or while the backlog fills). More than 80 ms left queued
//! after that is dropped, oldest first. A DMA FIFO error is acknowledged and
//! the stream plays on; whether FIFO errors come near the GPU's frame
//! uploads is counted (`FifoCorrelation`, from `upload_started` and
//! `upload_finished`, which the display marks). A stream
//! an underrun, a stalled position or a transient codec fault (a readback
//! still wrong when read again at once) stopped restarts alone at the next
//! turn, on its route as prepared (after
//! a rejected readback, with the route's verbs set again); a monitor that
//! went away pauses the route until it returns, and its verbs are set again
//! when it does; a route that was lost is prepared again, at most once a
//! second; a controller that needs a reset gets its own CRST, at most
//! `HDMI_RESETS` times a boot, 10 s apart. Each turn reads the route back
//! once. With `--guest-audio-hdmi-tone` the route plays a 1 kHz test tone
//! instead of the guest's frames.
//!
//! With `--virtio-snd` as well (`System::guest_audio_from_virtio`) the
//! frames come from the virtio sound device instead of the USB speaker,
//! which is not attached ([`Source::Virtio`]): its sink pushes them into a
//! `GuestRing`, which each turn takes from as it takes the speaker's. After
//! each HDMI turn the ring is told the frames the output consumed, the
//! backlog, and whether the route ran through the turn
//! (`GuestRing::publish`); the virtio stream's clock follows that, so the
//! guest is paced by the HDA link (`clock=hdmi-frames`), and goes back to
//! the host's device clock while the route does not run
//! (`clock=device-ticks`).
use super::{host, output, storage::RestoreRoot};
use alloc::{collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use stela_boot_core::guest_audio::{
    self, Backlog, CAP_FRAMES, Counters, Dbfs, FEEDBACK_NOMINAL, Feedback, FifoCorrelation, Level,
    MAX_STEP_FRAMES, Pacer, Period, Report, RestartReason, Restarts, Sample, Step, Tone, Uploads,
    VirtioReport, crossed, logged, s16_period, virtio_route,
};
use stela_drivers::{
    audio::{
        self as hd_audio, PcmFormat,
        hda::hdmi::{
            self as hdmi_codec, DisplayAudioIdentity, DisplayAudioPort, DisplayAudioState, EldRead,
            HdmiEntry, SinkAudio,
        },
    },
    bus::pci::Address,
    probe::native::hda::{
        HDMI_RESET_SPACING_US, HDMI_RESETS, HdmiPlayback, HdmiResetError, HdmiResetRefusal,
        HdmiStart, Host, RetirementError,
    },
};
use stela_soc::machine::{AppleDevices, USB_AUDIO_CHANNELS, UsbAudioMode, UsbAudioStatus};
use stela_soc::virtio::snd::sink::GuestRing;

type Frame = [i32; USB_AUDIO_CHANNELS];

/// The GPU's frame uploads into its scanout buffers (`gpu::display`'s
/// presents, which copy the guest's rows through the BAR into video memory
/// and then queue the flip): how many finished, the TSC when the last did,
/// and whether one runs. For the FIFO correlation only.
static UPLOADS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_END: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BUSY: AtomicBool = AtomicBool::new(false);

/// A frame upload starts (the display's present, on any PE). Only a build
/// with the GPU display (`nvk`) uploads frames.
#[cfg_attr(not(feature = "nvk"), allow(dead_code))]
pub(super) fn upload_started() {
    UPLOAD_BUSY.store(true, Ordering::Relaxed);
}

/// The frame upload [`upload_started`] marked ended.
#[cfg_attr(not(feature = "nvk"), allow(dead_code))]
pub(super) fn upload_finished() {
    UPLOAD_END.store(host::rdtsc(), Ordering::Relaxed);
    UPLOADS.fetch_add(1, Ordering::Release);
    UPLOAD_BUSY.store(false, Ordering::Relaxed);
}

fn uploads() -> Uploads {
    Uploads {
        count: UPLOADS.load(Ordering::Acquire),
        last_end: UPLOAD_END.load(Ordering::Relaxed),
        busy: UPLOAD_BUSY.load(Ordering::Relaxed),
    }
}

/// The HDMI output's stream: 48 kHz 16-bit stereo, which every HDMI sink
/// takes (`Eld::supports`'s baseline), and all a sink whose ELD stayed
/// unreadable or malformed is given (`hdmi::BASIC_AUDIO`).
const HDMI_FORMAT: PcmFormat = hdmi_codec::BASIC_AUDIO;
/// One period: 10 ms, 1920 bytes, a multiple of the HDA's 128 bytes.
const HDMI_PERIOD_FRAMES: usize = 480;
const HDMI_PERIOD_BYTES: usize = HDMI_PERIOD_FRAMES * 4;
/// 22 periods, 220 ms (`guest_audio::hdmi_periods`): a service gap of up
/// to about 210 ms keeps the DMA fed, as long as the guest's own 200 ms
/// ring rides out. The service has no timer of its own to refill from (a
/// turn runs when a PE waits or starves), so the ring is what covers a
/// turn's lateness. Audio X ms ahead of the DMA plays X ms later: a period
/// plays about 210 ms after it is written, plus at most the 20 ms backlog.
/// Six periods (60 ms) underran 112 times in 5 minutes on VM 117.
const HDMI_PERIODS: usize = guest_audio::hdmi_periods(HDMI_PERIOD_FRAMES);
const HDMI_PERIOD_MS: usize = HDMI_PERIOD_FRAMES * 1000 / guest_audio::RATE as usize;
/// The wait before a route that could not start is tried again.
const HDMI_RETRY_TICKS: u64 = host::COUNTER_HZ;
/// How often a paused route looks for its monitor again: one pin sense.
const HDMI_PAUSED_TICKS: u64 = host::COUNTER_HZ / 4;

/// Where the guest's frames come from.
pub(super) enum Source {
    /// The USB speaker on the native hub (`--guest-audio`).
    Usb,
    /// The virtio sound device's guest-audio ring (`--virtio-snd` with
    /// `--guest-audio-hdmi`); `None` when the device was not placed.
    Virtio(Option<Arc<GuestRing>>),
}

/// The input's state at a turn: the speaker's status, or the ring's.
#[derive(Clone, Copy, Debug)]
struct Input {
    /// The samples' bits while the guest streams, 0 while idle.
    bits: u8,
    muted: bool,
    volume: i16,
    /// Frames the guest sent, and those dropped at the input's full ring.
    received: u64,
    overflowed: u64,
    split: u64,
    packets: u64,
    feedback_reads: u64,
    asynchronous: bool,
    /// The time the output's rate is measured against: the guest's (the
    /// speaker's last service), or the host's device clock for the ring.
    time: u64,
}

impl From<UsbAudioStatus> for Input {
    fn from(status: UsbAudioStatus) -> Self {
        Self {
            bits: status.bits,
            muted: status.muted,
            volume: status.volume,
            received: status.counters.received,
            overflowed: status.counters.overflowed,
            split: status.counters.split,
            packets: status.counters.packets,
            feedback_reads: status.counters.feedback_reads,
            asynchronous: status.mode == UsbAudioMode::Asynchronous,
            time: status.time,
        }
    }
}

impl Input {
    /// The virtio ring at device-clock tick `now`: the guest's volume and
    /// mute are applied in the guest (the kext's clip), so neither is the
    /// host's.
    fn of_ring(ring: &GuestRing, now: u64) -> Self {
        let stats = ring.stats();
        Self {
            bits: ring.bits(),
            muted: false,
            volume: 0,
            received: stats.pushed,
            overflowed: stats.dropped,
            split: 0,
            packets: 0,
            feedback_reads: 0,
            asynchronous: false,
            time: now,
        }
    }
}

pub(super) struct GuestAudio {
    /// The USB speaker, or the virtio sound device's ring.
    source: Source,
    pacer: Pacer,
    counters: Counters,
    /// Whether the backlog fills or plays.
    backlog: Backlog,
    /// The output's rate against the guest's time, and the backlog's PI
    /// loop: the speaker's feedback.
    feedback: Feedback,
    /// The backlog after each streaming turn, since the last report (on
    /// the monitor, once the stream's pre-roll filled it).
    level: Level,
    /// Frames the output consumed, silence included: what the sink's
    /// clock owed, or HDMI periods written.
    output_frames: u64,
    /// The silence and drops at the last report, for the period's.
    last_totals: (u64, u64),
    /// To the sink: the guest's frames left waiting after the last turn.
    left: usize,
    /// The frames a turn takes, before they leave the devices' lock.
    frames: Vec<Frame>,
    /// The input's last state and queue, from the last turn.
    status: Option<(Input, usize)>,
    /// The bit resolution last reported, 0 while idle.
    streaming: u8,
    reported: Option<Report>,
    /// The monitor, instead of the sink.
    hdmi: Option<Hdmi>,
    /// The GPU's audio functions, claimed and kept for the boot.
    _display: Option<DisplayAudio>,
}

impl GuestAudio {
    /// `tone`: the HDMI route plays the built-in test tone instead of the
    /// guest's sound (`--guest-audio-hdmi-tone`). `asynchronous`: the
    /// speaker sends the feedback (`--guest-audio-async`). `tsc_hz` times
    /// the FIFO correlation's window. `source`: the USB speaker, or the
    /// virtio sound device's ring.
    pub(super) fn new(
        display: Option<DisplayAudio>,
        root: u64,
        tone: bool,
        asynchronous: bool,
        tsc_hz: u64,
        source: Source,
    ) -> Self {
        let virtio = matches!(source, Source::Virtio(_));
        match &source {
            Source::Usb => output::arguments(format_args!(
                "{}\n",
                guest_audio::endpoint_mode(asynchronous)
            )),
            Source::Virtio(Some(_)) => {}
            Source::Virtio(None) => output::arguments(format_args!(
                "GUEST AUDIO route: virtio-snd was not placed, so no guest sound reaches the \
                 output (and the USB speaker is not attached)\n"
            )),
        }
        let (hdmi, display) = match display {
            Some(DisplayAudio {
                hosts,
                playback: Some((index, playback, place)),
            }) => {
                let mut hosts = hosts;
                let host = hosts.remove(index);
                if virtio {
                    output::arguments(format_args!("{}\n", virtio_route(Some(place.as_str()))));
                } else {
                    output::arguments(format_args!(
                        "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played on the \
                         monitor: {place}\n"
                    ));
                }
                let ring_ms = HDMI_PERIODS * HDMI_PERIOD_MS;
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI ring: {HDMI_PERIODS} periods of {HDMI_PERIOD_MS} ms \
                     ({ring_ms} ms, {} bytes), refilled each service turn: a turn up to about {} \
                     ms late keeps the DMA fed (the guest's own ring holds {} ms); a backlog of \
                     {} ms kept ahead of the ring, {} ms at most, so about {} to {} ms from the \
                     guest to the monitor\n",
                    HDMI_PERIODS * HDMI_PERIOD_BYTES,
                    ring_ms - HDMI_PERIOD_MS,
                    MAX_STEP_FRAMES * 1000 / guest_audio::RATE,
                    guest_audio::TARGET_FRAMES * 1000 / guest_audio::RATE as usize,
                    CAP_FRAMES * 1000 / guest_audio::RATE as usize,
                    ring_ms - HDMI_PERIOD_MS
                        + guest_audio::TARGET_FRAMES * 1000 / guest_audio::RATE as usize,
                    ring_ms + CAP_FRAMES * 1000 / guest_audio::RATE as usize,
                ));
                if tone {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI test tone on {place}: a {} Hz sine at -{}.{} dBFS (peak \
                         {}) on both channels from the route's start, instead of the guest's \
                         sound (--guest-audio-hdmi-tone); the guest's frames are still taken, \
                         and discarded\n",
                        guest_audio::TONE_HZ,
                        guest_audio::TONE_TENTHS_DBFS / 10,
                        guest_audio::TONE_TENTHS_DBFS % 10,
                        guest_audio::TONE_PEAK,
                    ));
                }
                (
                    Some(Hdmi {
                        host,
                        playback,
                        root,
                        place,
                        pending: VecDeque::with_capacity(MAX_STEP_FRAMES as usize),
                        period: vec![0; HDMI_PERIOD_BYTES],
                        refills: 0,
                        started: false,
                        restarts: Restarts::default(),
                        last_reason: None,
                        announce: true,
                        paused: false,
                        retry_at: 0,
                        error: None,
                        reported: None,
                        dumps: 0,
                        resume_dumped: false,
                        tone: tone.then(Tone::default),
                        peak: 0,
                        first_frames: false,
                        reset_pending: false,
                        reset_cause: None,
                        reset_waiting: false,
                        gave_up: false,
                        stats: hd_audio::hda::Stats::default(),
                        correlation: FifoCorrelation::default(),
                        near_ticks: tsc_hz / 1000 * guest_audio::UPLOAD_NEAR_MS,
                        was_running: false,
                    }),
                    Some(DisplayAudio {
                        hosts,
                        playback: None,
                    }),
                )
            }
            display => {
                if tone {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI test tone not played: no HDMI playback was set up\n"
                    ));
                }
                let claimed = display.as_ref().map_or(0, |display| display.hosts.len());
                if virtio {
                    output::arguments(format_args!(
                        "{}; HDMI audio functions claimed: {claimed}\n",
                        virtio_route(None)
                    ));
                } else {
                    output::arguments(format_args!(
                        "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played to a sink \
                         at the host's counter; HDMI audio functions claimed: {claimed}\n",
                    ));
                }
                (None, display)
            }
        };
        Self {
            source,
            pacer: Pacer::new(host::COUNTER_HZ),
            counters: Counters::default(),
            backlog: Backlog::default(),
            feedback: Feedback::default(),
            level: Level::default(),
            output_frames: 0,
            last_totals: (0, 0),
            left: 0,
            // A turn takes at most the guest ring's 200 ms.
            frames: vec![[0; USB_AUDIO_CHANNELS]; MAX_STEP_FRAMES as usize],
            status: None,
            streaming: 0,
            reported: None,
            hdmi,
            _display: display,
        }
    }

    /// One turn, under the devices' lock. To the monitor: every frame the
    /// guest sent joins the host's queue. To the sink: the frames 48 kHz of
    /// the host's counter owes since the last turn, at `now`. While the
    /// guest does not stream, what it left waiting is discarded and the
    /// sink's clock stops.
    pub(super) fn serve(&mut self, devices: &mut AppleDevices, now: u64) {
        let (status, queued) = match &self.source {
            Source::Usb => {
                let Some((status, queued)) = devices.guest_audio_status() else {
                    return;
                };
                // The feedback the last turn computed (unsent by an
                // adaptive speaker).
                devices.guest_audio_feedback(self.feedback.value());
                (Input::from(status), queued)
            }
            Source::Virtio(Some(ring)) => (Input::of_ring(ring, now), ring.queued()),
            Source::Virtio(None) => return,
        };
        self.status = Some((status, queued));
        let taken = queued.min(self.frames.len());
        // The virtio stream's last frames, pushed before its STOP, still
        // play out on the monitor; a speaker that stopped streaming has its
        // leftovers discarded.
        let plays_out = matches!(self.source, Source::Virtio(_)) && self.hdmi.is_some();
        if status.bits == 0 && !plays_out {
            self.pacer.stop();
            self.take(devices, taken);
            return;
        }
        if self.hdmi.is_some() {
            let moved = self.take(devices, taken);
            if let Some(hdmi) = self.hdmi.as_mut() {
                hdmi.pending.extend(&self.frames[..moved]);
            }
            return;
        }
        let due = self.pacer.due(now);
        let step = self.backlog.step(due, queued, true);
        self.take(devices, step.dropped.min(MAX_STEP_FRAMES as usize));
        self.take(devices, step.take);
        self.counters.add(step);
        self.output_frames += due;
        self.left = queued - step.take - step.dropped;
        self.level.observe(self.left);
    }

    /// Moves up to `count` of the input's frames into `frames`: the number
    /// moved.
    fn take(&mut self, devices: &mut AppleDevices, count: usize) -> usize {
        let out = &mut self.frames[..count];
        match &self.source {
            Source::Usb => devices.guest_audio_take(out),
            Source::Virtio(Some(ring)) => ring.take(out),
            Source::Virtio(None) => 0,
        }
    }

    /// The monitor's turn, outside the devices' lock, at counter `now`:
    /// starts its route when it is not running and is due, writes every
    /// period the DMA completed, and trims the queue to the cap. Then, on
    /// the monitor or the sink, the feedback looks at the turn.
    pub(super) fn play(&mut self, now: u64) {
        let Some((status, _)) = self.status else {
            return;
        };
        let streaming = status.bits != 0;
        let running = match self.hdmi.as_mut() {
            Some(hdmi) => {
                let turn = hdmi.play(
                    now,
                    streaming,
                    &mut self.counters,
                    &mut self.backlog,
                    &mut self.output_frames,
                );
                // The level while the stream plays, its pre-roll left out.
                if streaming && !self.backlog.pre_roll(streaming) {
                    self.level.observe(hdmi.pending.len());
                }
                // The virtio stream's clock follows a route that ran through
                // the turn, and goes back to the device clock when it did
                // not; a turn that could not look tells it nothing. A
                // backlog that fills (the stream's pre-roll) is published at
                // its target: the silence it plays builds it, not the clock.
                if let Source::Virtio(Some(ring)) = &self.source {
                    let backlog = if self.backlog.pre_roll(streaming) {
                        guest_audio::TARGET_FRAMES
                    } else {
                        hdmi.pending.len() + ring.queued()
                    };
                    match turn {
                        Some(true) => ring.publish(self.output_frames, now, backlog as u64),
                        Some(false) => ring.publish_stopped(),
                        None => {}
                    }
                }
                // A turn that wrote nothing (the loader root was busy)
                // measures nothing: the DMA ran on unseen.
                let Some(continuous) = turn else {
                    return;
                };
                streaming && continuous
            }
            None => streaming,
        };
        let backlog = match &self.hdmi {
            Some(hdmi) => hdmi.pending.len(),
            None => self.left,
        };
        self.feedback.update(Sample {
            guest_ticks: status.time,
            output_frames: self.output_frames,
            backlog,
            running,
        });
    }

    /// Reports the guest starting or stopping its stream, outside the lock.
    pub(super) fn transitions(&mut self) {
        let bits = self.status.map_or(0, |(status, _)| status.bits);
        if bits != self.streaming {
            self.streaming = bits;
            match bits {
                0 => output::arguments(format_args!(
                    "GUEST AUDIO stream stopped{}\n",
                    self.source_tag()
                )),
                bits => output::arguments(format_args!(
                    "GUEST AUDIO stream started: {bits}-bit stereo at 48 kHz{}\n",
                    self.source_tag()
                )),
            }
        }
    }

    /// The `GUEST AUDIO` counts, and the monitor's, when they changed since
    /// the last report.
    pub(super) fn report(&mut self) {
        if let Some(hdmi) = self.hdmi.as_mut() {
            hdmi.report();
        }
        let Some((status, queued)) = self.status else {
            return;
        };
        let totals = (
            self.counters.silent,
            status.overflowed + self.counters.trimmed,
        );
        let last = core::mem::replace(&mut self.last_totals, totals);
        let virtio = matches!(self.source, Source::Virtio(_));
        let report = Report {
            bits: status.bits,
            received: status.received,
            overflowed: status.overflowed,
            split: status.split,
            queued: queued + self.hdmi.as_ref().map_or(0, |hdmi| hdmi.pending.len()),
            muted: status.muted,
            volume: status.volume,
            host: self.counters,
            interval_silent: totals.0.saturating_sub(last.0),
            interval_dropped: totals.1.saturating_sub(last.1),
            backlog: self.level.take(),
            asynchronous: status.asynchronous,
            feedback: if virtio {
                FEEDBACK_NOMINAL
            } else {
                self.feedback.value()
            },
            measured: self.feedback.estimate(),
            packets: status.packets,
            feedback_reads: status.feedback_reads,
        };
        if self.reported != Some(report) {
            self.reported = Some(report);
            if virtio {
                output::arguments(format_args!("{}\n", VirtioReport(report)));
            } else {
                output::arguments(format_args!("{report}\n"));
            }
        }
    }

    /// What the stream lines add for the virtio source.
    fn source_tag(&self) -> &'static str {
        match self.source {
            Source::Usb => "",
            Source::Virtio(_) => " (virtio-snd)",
        }
    }
}

/// The guest's sound on the monitor: one HDMI playback of a claimed HDA
/// function, fed from the guest's frames each service turn.
struct Hdmi {
    host: Host,
    playback: HdmiPlayback,
    /// The loader's root, which maps the function's registers and DMA.
    root: u64,
    /// Where it plays, for the log.
    place: String,
    /// The guest's frames not yet written to a period.
    pending: VecDeque<Frame>,
    /// One period of 16-bit stereo, as written.
    period: Vec<u8>,
    refills: u64,
    /// Whether the route ever started.
    started: bool,
    restarts: Restarts,
    /// The last stop's reason.
    last_reason: Option<RestartReason>,
    /// Whether the next start gets a line: its stop had one.
    announce: bool,
    /// The monitor is away; the route waits for it, prepared when it was.
    paused: bool,
    /// When a route that could not start is tried again.
    retry_at: u64,
    /// The last error, reported once.
    error: Option<RetirementError>,
    reported: Option<(
        bool,
        bool,
        u64,
        Restarts,
        u8,
        hd_audio::hda::Stats,
        FifoCorrelation,
    )>,
    /// Route state dumps printed (`HDMI_STATE_DUMPS` at most), and whether
    /// one followed a resume yet.
    dumps: u8,
    resume_dumped: bool,
    /// With `--guest-audio-hdmi-tone`: the test tone, played instead of
    /// the guest's frames.
    tone: Option<Tone>,
    /// The largest absolute sample written since the last report.
    peak: u32,
    /// Whether the guest's first frames were reported.
    first_frames: bool,
    /// The controller needs a reset (`NeedsReset`, or a reset that failed):
    /// the next start resets it, as `Host::reset_hdmi` admits.
    reset_pending: bool,
    /// The error before the first `NeedsReset` of the pending reset.
    reset_cause: Option<RetirementError>,
    /// The pending reset's wait for the spacing was reported.
    reset_waiting: bool,
    /// Every reset was taken and the route still needs one: given up.
    gave_up: bool,
    /// The controller's counts as last logged (`Hdmi::absorbed`).
    stats: hd_audio::hda::Stats,
    /// FIFO errors near the GPU's frame uploads or apart from them.
    correlation: FifoCorrelation,
    /// [`guest_audio::UPLOAD_NEAR_MS`] in TSC ticks.
    near_ticks: u64,
    /// The stream ran at the end of the last turn that wrote.
    was_running: bool,
}

/// Route state dumps per boot: each preparation of the route, and the
/// first resume after the monitor slept, up to this many.
const HDMI_STATE_DUMPS: u8 = 4;

/// Why the playback stopped, from the error the refill returned; the
/// driver stops the same way (`hda::hdmi`): an underrun, a stalled DMA
/// position or a codec fault stops the stream only (a FIFO error alone no
/// longer stops it, `dma_fault` stays for one), a sink away pauses the route,
/// anything else closes it (a DMA descriptor error needs a controller
/// reset).
fn restart_reason(error: &RetirementError) -> RestartReason {
    use hd_audio::Error;
    match error {
        RetirementError::Controller(
            Error::PlaybackUnderrun | Error::PlaybackStalled | Error::InvalidPosition(_),
        ) => RestartReason::Underrun,
        RetirementError::Controller(Error::StreamDma { status })
            if guest_audio::StreamStatus(*status).fifo_error_only() =>
        {
            RestartReason::DmaFault
        }
        RetirementError::Controller(Error::SinkUnavailable) => RestartReason::SinkAway,
        RetirementError::Controller(
            Error::RouteRejected | Error::InvalidEld | Error::Timeout(hd_audio::Phase::Verb),
        ) => RestartReason::CodecFault,
        _ => RestartReason::RouteLost,
    }
}

impl Hdmi {
    /// The route started: the first time, how it plays; after a stop
    /// whose line was printed, or after a pause, how it started again.
    fn started(&mut self, start: HdmiStart) {
        let resumed = core::mem::take(&mut self.paused);
        let without_eld = self.playback.without_eld();
        if !self.started {
            self.started = true;
            match without_eld {
                Some(error) => output::arguments(format_args!(
                    "GUEST AUDIO HDMI playing on {} without an ELD ({}): converter {:#04x}, \
                     stream tag {}, basic audio 48 kHz 16-bit stereo on FL/FR, {HDMI_PERIODS} \
                     periods of {HDMI_PERIOD_MS} ms\n",
                    self.place,
                    eld_reason(&error),
                    self.playback.converter(),
                    self.playback.stream_tag(),
                )),
                None => output::arguments(format_args!(
                    "GUEST AUDIO HDMI playing on {}: converter {:#04x}, stream tag {}, 48 kHz \
                     16-bit stereo, {HDMI_PERIODS} periods of {HDMI_PERIOD_MS} ms\n",
                    self.place,
                    self.playback.converter(),
                    self.playback.stream_tag(),
                )),
            }
            return;
        }
        if !self.announce && !resumed {
            return;
        }
        let how = match start {
            HdmiStart::Stream => "stream only: its descriptor reset and re-armed",
            HdmiStart::Reprogrammed => {
                "route programmed again after a rejected readback (no codec walk: converter \
                 format and stream tag, DIG1, channel count and mapping, connection select, pin \
                 control, amp, infoframe), then the stream: its descriptor reset and re-armed"
            }
            HdmiStart::Resumed => {
                "resumed: the monitor is back with the same ELD; route programmed again (no \
                 codec walk: converter format and stream tag, DIG1, channel count and mapping, \
                 connection select, pin control, amp, infoframe), then the stream: its \
                 descriptor reset and re-armed"
            }
            HdmiStart::Prepared => "route prepared again: codec walked, ELD read",
        };
        output::arguments(format_args!(
            "GUEST AUDIO HDMI playing again on {} (restart {}): {how}; converter {:#04x}, \
             stream tag {}{}\n",
            self.place,
            self.restarts.total(),
            self.playback.converter(),
            self.playback.stream_tag(),
            if without_eld.is_some() {
                ", without an ELD"
            } else {
                ""
            }
        ));
    }

    /// The refill failed with `error`, which stopped the stream: count
    /// its reason, and print it unless it repeats the last one's past
    /// [`logged`].
    fn stopped(&mut self, error: RetirementError) {
        let reason = restart_reason(&error);
        let count = self.restarts.add(reason);
        self.announce = self.last_reason != Some(reason) || logged(count);
        self.last_reason = Some(reason);
        if self.announce {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI stopped on {} (restart {}, {} {count}): {error:?}: {}\n",
                self.place,
                self.restarts.total(),
                reason.name(),
                reason.action(),
            ));
        }
        self.detail(&error, self.announce);
        // The stop line already said the monitor is away.
        self.paused = reason == RestartReason::SinkAway;
        self.error = Some(error);
    }

    /// Diagnostics after `error`, printed when `print`: the codec answer
    /// behind a rejected route readback (taken in any case, so that a later
    /// line never shows a stale one), and a stream DMA status in hex with
    /// its bits named.
    fn detail(&mut self, error: &RetirementError, print: bool) {
        if let Some(rejection) = self.host.take_hdmi_rejection()
            && print
        {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI rejected on {}: {rejection}\n",
                self.place
            ));
        }
        if let RetirementError::Controller(hd_audio::Error::StreamDma { status }) = error
            && print
        {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI stream DMA fault on {}: SDnSTS {}\n",
                self.place,
                guest_audio::StreamStatus(*status)
            ));
        }
    }

    /// Prints the route as the codec answers now, `when`: after it was
    /// prepared, or after the first resume; [`HDMI_STATE_DUMPS`] at most.
    fn dump(&mut self, when: &str) {
        if self.dumps >= HDMI_STATE_DUMPS {
            return;
        }
        self.dumps += 1;
        match self.host.hdmi_route_state(&self.playback) {
            Ok(state) => output::arguments(format_args!(
                "GUEST AUDIO HDMI state on {} ({when}): {state}\n",
                self.place
            )),
            Err(error) => output::arguments(format_args!(
                "GUEST AUDIO HDMI state on {} ({when}): unreadable: {error:?}\n",
                self.place
            )),
        }
    }

    /// The turn: start the route when due, write every period the DMA
    /// completed, trim the queue to the cap. `None` when it could not look
    /// at the stream (the loader root was busy); else whether the stream
    /// ran through the turn and the last one, consuming without a break.
    fn play(
        &mut self,
        now: u64,
        streaming: bool,
        counters: &mut Counters,
        backlog: &mut Backlog,
        output_frames: &mut u64,
    ) -> Option<bool> {
        let mut turn = None;
        // SAFETY: the loader root `storage::open` captured, which maps the
        // claimed function's registers and DMA pages for the whole run; a
        // service turn runs with its processor's pinned record active.
        if let Some(_root) = unsafe { RestoreRoot::try_enter(self.root) } {
            let was_running = self.was_running && self.playback.running();
            if self.playback.running() || self.start(now) {
                *output_frames +=
                    self.refill(streaming, counters, backlog) * HDMI_PERIOD_FRAMES as u64;
            } else {
                self.correlation.interrupt();
            }
            self.was_running = self.playback.running();
            turn = Some(was_running && self.was_running);
        }
        // Running or not (paused while the monitor sleeps, waiting to
        // start again), at most the cap waits, oldest dropped first.
        let excess = self.pending.len().saturating_sub(CAP_FRAMES);
        if excess > 0 {
            self.pending.drain(..excess);
            counters.add(Step {
                dropped: excess,
                ..Step::default()
            });
        }
        turn
    }

    /// Starts the route when it is due, at counter `now`: whether it runs.
    fn start(&mut self, now: u64) -> bool {
        if self.gave_up || now < self.retry_at {
            return false;
        }
        if self.reset_pending {
            return self.reset(now);
        }
        // A route the stop left prepared restarts its stream alone; one
        // that was lost is prepared again (`Host::start_hdmi`).
        match self.host.start_hdmi(&mut self.playback) {
            Ok(start) => {
                self.on_start(start);
                true
            }
            Err(RetirementError::Controller(hd_audio::Error::NeedsReset)) => {
                // The controller needs its own reset: CRST, never a
                // function-level reset (`Host::reset_hdmi`).
                self.detail(
                    &RetirementError::Controller(hd_audio::Error::NeedsReset),
                    true,
                );
                self.reset_pending = true;
                self.reset_waiting = false;
                self.reset_cause = self.error;
                self.reset(now)
            }
            Err(RetirementError::Controller(hd_audio::Error::SinkUnavailable)) => {
                // The monitor sleeps or was unplugged: look again soon, one
                // pin sense each time.
                self.retry_at = now.wrapping_add(HDMI_PAUSED_TICKS);
                if !self.paused {
                    self.paused = true;
                    let prepared = self.host.hdmi_paused(&self.playback).unwrap_or(false);
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI paused on {}: the pin senses no monitor with a valid \
                         ELD (asleep or unplugged){}; playing again when it returns\n",
                        self.place,
                        if prepared {
                            "; the route stays prepared"
                        } else {
                            ""
                        }
                    ));
                }
                false
            }
            Err(error) => {
                self.retry_at = now.wrapping_add(HDMI_RETRY_TICKS);
                if self.error != Some(error) {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI cannot start on {}: {error:?}; trying again each \
                         second\n",
                        self.place
                    ));
                    self.error = Some(error);
                }
                // A start a readback refused (at most HDMI_TRANSIENT_FAULTS
                // in a row, a second apart) names the answer each time.
                self.detail(&error, true);
                false
            }
        }
    }

    /// The route started (`start`): its line, the readback that closed it
    /// before it was prepared again, and the state dump.
    fn on_start(&mut self, start: HdmiStart) {
        // A readback that closed the route before it was prepared again.
        if let Some(rejection) = self.host.take_hdmi_rejection() {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI rejected on {}: {rejection}\n",
                self.place
            ));
        }
        self.started(start);
        self.error = None;
        match start {
            HdmiStart::Prepared => self.dump("prepared"),
            HdmiStart::Resumed if !self.resume_dumped => {
                self.resume_dumped = true;
                self.dump("resumed");
            }
            _ => {}
        }
    }

    /// The controller needs a reset, at counter `now`: `Host::reset_hdmi`
    /// (its own CRST, the codecs walked again, the port attached again, a
    /// stream allocated), then the route prepared and started. At most
    /// `HDMI_RESETS` a boot, `HDMI_RESET_SPACING_US` apart; after the last,
    /// the route stays stopped. Whether it runs.
    fn reset(&mut self, now: u64) -> bool {
        let now_us = (u128::from(now) * 1_000_000 / u128::from(host::COUNTER_HZ)) as u64;
        let cause = match self.reset_cause {
            Some(error) => alloc::format!("{error:?}, then NeedsReset"),
            None => String::from("NeedsReset"),
        };
        let next = self.playback.resets() + 1;
        match self.host.reset_hdmi(&mut self.playback, now_us) {
            Ok(reset) => {
                self.reset_pending = false;
                self.reset_waiting = false;
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI controller reset {}/{HDMI_RESETS} on {}: after {cause}: the \
                     HD Audio controller's own reset (CRST; no function-level reset or D3), {} \
                     codecs found again, the port attached again, stream tag {} allocated on {}; \
                     preparing the route\n",
                    reset.reset,
                    self.place,
                    reset.codecs,
                    self.playback.stream_tag(),
                    if reset.same_buffers {
                        "the reset stream's ring and BDL"
                    } else {
                        "newly allocated DMA memory"
                    },
                ));
                self.reset_cause = None;
                self.error = None;
                match self.host.start_hdmi(&mut self.playback) {
                    Ok(start) => {
                        self.on_start(start);
                        true
                    }
                    Err(error) => {
                        self.retry_at = now.wrapping_add(HDMI_RETRY_TICKS);
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI cannot start on {} after controller reset {}/\
                             {HDMI_RESETS}: {error:?}; trying again each second\n",
                            self.place, reset.reset
                        ));
                        self.detail(&error, true);
                        self.reset_pending =
                            error == RetirementError::Controller(hd_audio::Error::NeedsReset);
                        self.reset_cause = Some(error);
                        self.error = Some(error);
                        false
                    }
                }
            }
            Err(HdmiResetError::Refused(HdmiResetRefusal::TooSoon { wait_us })) => {
                self.retry_at = now.wrapping_add(HDMI_RETRY_TICKS);
                if !core::mem::replace(&mut self.reset_waiting, true) {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI controller reset {next}/{HDMI_RESETS} on {} waits {} ms \
                         (resets at least {} s apart): after {cause}\n",
                        self.place,
                        wait_us.div_ceil(1000),
                        HDMI_RESET_SPACING_US / 1_000_000,
                    ));
                }
                false
            }
            Err(HdmiResetError::Refused(HdmiResetRefusal::Exhausted)) => {
                self.gave_up = true;
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI controller reset on {}: all {HDMI_RESETS} resets of this \
                     boot taken, and the route needs another after {cause}; giving up: the route \
                     stays stopped (running=false)\n",
                    self.place
                ));
                false
            }
            Err(HdmiResetError::Failed { reset, error }) => {
                self.retry_at = now.wrapping_add(HDMI_RETRY_TICKS);
                self.reset_waiting = false;
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI controller reset {reset}/{HDMI_RESETS} on {} failed: \
                     {error:?} (after {cause}); another reset when one is due\n",
                    self.place
                ));
                self.error = Some(error);
                false
            }
        }
    }

    /// Writes every period the DMA completed: the queue's oldest frames
    /// once the backlog plays, silence for any it lacks; the periods
    /// written.
    fn refill(&mut self, streaming: bool, counters: &mut Counters, backlog: &mut Backlog) -> u64 {
        // One whole route readback a turn, at its first status read; the
        // others sense the pin only. A failure shows at the refill.
        let _ = self.host.schedule_hdmi_readback(&self.playback);
        let mut written = 0;
        loop {
            // What the backlog would play; taken only once written. The
            // pre-roll's last period is silence first, so the stream's
            // frames run on unbroken into the next.
            let mut next = *backlog;
            let Period {
                lead,
                take,
                pre_roll,
            } = next.period(HDMI_PERIOD_FRAMES, self.pending.len(), streaming);
            // The test tone instead of the guest's frames, which are still
            // taken (and discarded) as they would be played.
            let tone = self.tone;
            match self.tone.as_mut() {
                Some(tone) => tone.fill(&mut self.period),
                None => {
                    let (silence, frames) = self.period.split_at_mut(lead * 4);
                    silence.fill(0);
                    s16_period(self.pending.iter().take(take).copied(), frames);
                }
            }
            match self.host.refill_hdmi(&mut self.playback, &self.period) {
                Ok(true) => {
                    *backlog = next;
                    written += 1;
                    self.pending.drain(..take);
                    self.refills += 1;
                    let peak = guest_audio::peak(&self.period);
                    self.peak = self.peak.max(peak);
                    if take > 0 && !self.first_frames {
                        self.first_frames = true;
                        if self.tone.is_some() {
                            output::arguments(format_args!(
                                "GUEST AUDIO HDMI first guest frames on the route on {} at period \
                                 {}: {take} frames taken and discarded; the test tone plays \
                                 instead\n",
                                self.place, self.refills
                            ));
                        } else {
                            output::arguments(format_args!(
                                "GUEST AUDIO HDMI first guest frames on the route on {} at period \
                                 {}: {take} frames written into the ring (peak {})\n",
                                self.place,
                                self.refills,
                                Dbfs(peak)
                            ));
                        }
                    }
                    // Silence counts only while the guest streams, the
                    // stream's pre-roll apart from padding.
                    let (silent, pre_roll) = if streaming {
                        (HDMI_PERIOD_FRAMES - take - pre_roll, pre_roll)
                    } else {
                        (0, 0)
                    };
                    counters.add(Step {
                        dropped: 0,
                        take,
                        silent: silent as u64,
                        pre_roll: pre_roll as u64,
                    });
                }
                // Not written: the tone's phase stays where the ring is.
                Ok(false) => {
                    self.tone = tone;
                    break;
                }
                Err(error) => {
                    self.tone = tone;
                    self.stopped(error);
                    break;
                }
            }
        }
        self.absorbed();
        written
    }

    /// What the controller absorbed since the last turn without stopping
    /// the stream, logged: FIFO errors acknowledged, route readbacks and
    /// pin senses a second read contradicted, RIRB entries waited for or
    /// lost, and responses that were never taken as an answer. Each kind's
    /// first four, then at each power of two; the report counts them all.
    /// FIFO errors are also counted near the GPU's frame uploads or apart
    /// from them (`FifoCorrelation`), as is the turn.
    fn absorbed(&mut self) {
        let Some(now) = self.host.hdmi_stats() else {
            return;
        };
        let last = core::mem::replace(&mut self.stats, now);
        // The misread is taken in any case, so that a later line never
        // shows a stale one.
        let misread = self.host.take_hdmi_misread();
        let fresh = now.fifo_errors.saturating_sub(last.fifo_errors);
        let uploads = uploads();
        let read = host::rdtsc();
        let near = if self.playback.running() {
            self.correlation.turn(read, uploads, fresh, self.near_ticks)
        } else {
            self.correlation.interrupt();
            None
        };
        if crossed(last.fifo_errors, now.fifo_errors) {
            let since = if uploads.count == 0 {
                String::from("no GPU frame upload yet")
            } else if uploads.busy {
                String::from("a GPU frame upload under way")
            } else {
                let per_ms = (self.near_ticks / guest_audio::UPLOAD_NEAR_MS).max(1);
                let tenths = read.saturating_sub(uploads.last_end).saturating_mul(10) / per_ms;
                alloc::format!(
                    "the last GPU frame upload ended {}.{} ms before this read",
                    tenths / 10,
                    tenths % 10
                )
            };
            output::arguments(format_args!(
                "GUEST AUDIO HDMI FIFO error on {} (fifo_errors {}): SDnSTS {}; acknowledged \
                 (FIFOE written 1 to clear), the stream plays on: no stop, no restart; {}: {since} \
                 ({} uploads so far; {})\n",
                self.place,
                now.fifo_errors,
                guest_audio::StreamStatus(now.fifo_status),
                match near {
                    Some(true) => "near an upload",
                    Some(false) => "apart from uploads",
                    None => "no window yet",
                },
                uploads.count,
                self.correlation,
            ));
        }
        if let Some(misread) = misread
            && (crossed(last.misreads, now.misreads)
                || crossed(last.sense_misreads, now.sense_misreads))
        {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI misread on {} (misreads {}, sense_misreads {}): {misread}, as \
                 set: a command-transport misread, not a route fault; the stream plays on\n",
                self.place, now.misreads, now.sense_misreads
            ));
        }
        if crossed(last.rirb_late, now.rirb_late)
            || crossed(last.rirb_lost, now.rirb_lost)
            || crossed(last.spurious, now.spurious)
            || crossed(last.unsolicited, now.unsolicited)
        {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI command responses on {}: rirb_late={} rirb_lost={} spurious={} \
                 unsolicited={} (late: the RIRB write pointer covered an entry before its write \
                 landed; waited for it, never taking the ring's previous lap; spurious and \
                 unsolicited: dropped, never an answer)\n",
                self.place, now.rirb_late, now.rirb_lost, now.spurious, now.unsolicited
            ));
        }
    }

    fn report(&mut self) {
        let state = (
            self.playback.running(),
            self.paused,
            self.refills,
            self.restarts,
            self.playback.resets(),
            self.stats,
            self.correlation,
        );
        if self.reported != Some(state) {
            self.reported = Some(state);
            let stats = state.5;
            // The peak: the largest sample written since the last report.
            output::arguments(format_args!(
                "GUEST AUDIO HDMI running={} paused={} periods={} {} resets={}/{HDMI_RESETS} \
                 fifo_errors={} misreads={} sense_misreads={} rirb_late={} rirb_lost={} \
                 spurious={} {} pending={} peak={}{}\n",
                state.0,
                state.1,
                state.2,
                state.3,
                state.4,
                stats.fifo_errors,
                stats.misreads,
                stats.sense_misreads,
                stats.rirb_late,
                stats.rirb_lost,
                stats.spurious,
                state.6,
                self.pending.len(),
                Dbfs(core::mem::take(&mut self.peak)),
                if self.tone.is_some() {
                    " source=tone"
                } else {
                    ""
                },
            ));
        }
    }
}

/// The display side of the monitor's audio as the HDA owner sees it.
/// nouveau's mode set already acquired an HDA-capable SOR, enabled HDMI
/// audio and wrote the ELD (`nv50_audio_enable`, the r535 SOR controls), so
/// this port acknowledges without programming the display; a sink change
/// shows in the codec's own pin sense, which every status read checks.
struct MonitorPort {
    identity: DisplayAudioIdentity,
}

// SAFETY: one port per claimed HDA function, codec, pin and device entry,
// made for the controller the boot keeps for its whole run; the GPU's
// display owner keeps the connector. `observe` cannot fail and reports
// one generation: the codec's pin sense, read at each status, is what
// stops playback on a sink change. Enabling and disabling change nothing
// the display owner programmed, and dropping the Arc revokes nothing.
unsafe impl DisplayAudioPort for MonitorPort {
    fn identity(&self) -> DisplayAudioIdentity {
        self.identity
    }
    fn observe(&self, _deadline_us: u64) -> Result<DisplayAudioState, hd_audio::Error> {
        Ok(DisplayAudioState {
            generation: 1,
            connected: true,
        })
    }
    fn enable_audio(&self, _format: PcmFormat, _deadline_us: u64) -> Result<(), hd_audio::Error> {
        Ok(())
    }
    fn disable_audio(&self, _deadline_us: u64) -> Result<(), hd_audio::Error> {
        Ok(())
    }
}

/// The HD Audio functions of the GPUs whose start reset their bus, claimed
/// once that start finished (`claim_display_audio`), kept for the boot.
/// Their registers and DMA are reached through the loader's root only.
pub(super) struct DisplayAudio {
    hosts: Vec<Host>,
    /// With `--guest-audio-hdmi`: the host whose playback was set up, by
    /// index, the playback and where it plays.
    playback: Option<(usize, HdmiPlayback, String)>,
}

/// Claims the HD Audio function (function 1) beside each GPU whose cold
/// start reset its bus in this boot (`gpu::reset_their_bus`), and reports
/// its codecs, HDMI/DP pins, their sense and ELDs: the GPU's display side
/// writes the ELD and presence of the monitor it drives with audio.
///
/// The function was left parked for that reset (`probe::display_audio`);
/// claimed only now, its original users are those the reset retired. Run
/// once the GPU's own start has finished, on the kernel work's foreground
/// as the GPU's claim is. With `hdmi`, the playback on the first entry
/// whose ELD takes the stream is set up here and starts at the first
/// service turn ([`GuestAudio::play`]).
pub(super) fn claim_display_audio(
    work: &'static super::kernel_work::KernelWork,
    mmio: &'static crate::native_mmio::Source,
    remappers: &'static super::remapping::Remappers,
    handoff: &crate::handoff::Handoff,
    clock: stela_drivers::Clock,
    hdmi: bool,
    trust_quiesced: bool,
) -> Option<DisplayAudio> {
    use super::gpu::GpuReset;
    use stela_drivers::probe::{NativeServices, native::hda::Vouch};
    // What vouches for each GPU's audio function: its GPU's bus reset, or,
    // by the owner's explicit choice, its observed quiesced state.
    let gpus: Vec<(Address, Vouch)> = super::gpu::claimed_resets()
        .into_iter()
        .filter_map(|(gpu, reset)| {
            let address = Address { function: 1, ..gpu };
            match reset {
                GpuReset::SecondaryBus => Some((gpu, Vouch::BusReset)),
                _ if trust_quiesced => {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI {address}: GPU {gpu} started with {}, so nothing \
                         reset this function; --guest-audio-hdmi-trust-quiesced claims it on \
                         its observed quiesced state\n",
                        reset.describe()
                    ));
                    Some((gpu, Vouch::Quiesced))
                }
                _ => {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI {address} not claimed: no proof its original users \
                         stopped: GPU {gpu} started with {}, not a Secondary Bus Reset of its \
                         bus; --guest-audio-hdmi-trust-quiesced would claim it on its observed \
                         quiesced state\n",
                        reset.describe()
                    ));
                    None
                }
            }
        })
        .collect();
    if gpus.is_empty() {
        output::arguments(format_args!(
            "GUEST AUDIO HDMI not claimed: no GPU audio function has a proof that its original \
             users stopped\n"
        ));
        return None;
    }
    let heap = handoff.heap.physical..handoff.heap.physical + handoff.heap.bytes;
    let tsc_hz = handoff.tsc_hz;
    let claimed = work.call_owned(CLAIM_US, move || {
        let services =
            super::services::Services::new(mmio, remappers, None, heap, tsc_hz, Vec::new());
        let mut hosts = Vec::new();
        let mut playback = None;
        for (gpu, vouch) in gpus {
            let address = Address { function: 1, ..gpu };
            let minimum = match services.minimum_delay() {
                Ok(minimum) => minimum,
                Err(error) => {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI {address} refused: no minimum-delay timer: {error}\n"
                    ));
                    continue;
                }
            };
            let mut observed = |state: &stela_drivers::probe::native::hda::Quiescence| {
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI {address} observed: command={:#06x} gcap={:#06x} \
                     gctl={:#010x} intctl={:#010x} intsts={:#010x} corbctl={:#04x} \
                     rirbctl={:#04x} running_streams={:#x} unanswered={} after_reset={:?} \
                     quiet={}\n",
                    state.command,
                    state.gcap,
                    state.gctl,
                    state.intctl,
                    state.intsts,
                    state.corbctl,
                    state.rirbctl,
                    state.running_streams,
                    state.unanswered,
                    state.reset,
                    state.quiet(),
                ));
            };
            // SAFETY: function 1 of a display controller whose cold start
            // completed in this boot; probe left it parked and nothing
            // claimed it since. `vouch` is its GPU's Secondary Bus Reset, or
            // the owner's explicit trust in its observed quiesced state
            // (`--guest-audio-hdmi-trust-quiesced`). No later reset of that
            // bus exists in the boot.
            match unsafe {
                stela_drivers::probe::native::hda::attach_display_audio(
                    &services,
                    address,
                    clock,
                    minimum,
                    vouch,
                    &mut observed,
                )
            } {
                Ok(mut host) => {
                    let target = report_display_audio(&mut host, gpu);
                    if hdmi
                        && playback.is_none()
                        && let Some(target) = target
                    {
                        if let Some(why) = &target.without_eld {
                            output::arguments(format_args!(
                                "GUEST AUDIO HDMI {address} codec {} pin {:#04x} device {} \
                                 plays without an ELD: {why}; basic audio only: 2-channel 48 kHz \
                                 16-bit LPCM on front left/right (FL/FR)\n",
                                target.codec, target.pin, target.device
                            ));
                        }
                        playback = hdmi_playback(
                            &mut host,
                            address,
                            (target.codec, target.pin, target.device),
                        )
                        .map(|(output, place)| (hosts.len(), output, place));
                    }
                    hosts.push(host);
                }
                Err((error, kept)) => {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI {address} refused ({}): {error}\n",
                        match vouch {
                            Vouch::BusReset => "vouched by its GPU's bus reset",
                            _ => "vouched by its observed quiesced state",
                        }
                    ));
                    // The original owners a failure keeps stay for the boot.
                    core::mem::forget(kept);
                }
            }
        }
        (hosts, playback)
    });
    match claimed {
        Ok((hosts, playback)) if !hosts.is_empty() => Some(DisplayAudio { hosts, playback }),
        Ok(_) => None,
        Err(error) => {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI claim did not finish: {error}\n"
            ));
            None
        }
    }
}

/// The longest the claim and the codecs' discovery may take, the survey's
/// ELD repolls (`hdmi::ELD_READS` reads `hdmi::ELD_REPOLL_US` apart, for a
/// sink whose ELD does not read or parse) included.
const CLAIM_US: u64 = 10_000_000;

/// Where the monitor's sound plays: a codec, pin and device entry, and,
/// for a sink whose ELD stayed unreadable or malformed, why it plays basic
/// audio without one.
struct Target {
    codec: u8,
    pin: u8,
    device: u8,
    without_eld: Option<String>,
}

/// An ELD read or parse failure as the check that failed names it.
fn eld_reason(error: &hd_audio::Error) -> String {
    match error {
        hd_audio::Error::EldRead(fault) => alloc::format!("{fault}"),
        hd_audio::Error::EldParse(fault) => alloc::format!("{fault}"),
        error => alloc::format!("{error:?}"),
    }
}

/// Bytes as the `ELD ... bytes:` lines print them; `-` for none.
fn hex_bytes(raw: &[u8]) -> String {
    if raw.is_empty() {
        return String::from("-");
    }
    let bytes: Vec<_> = raw.iter().map(|b| alloc::format!("{b:02x}")).collect();
    bytes.join(" ")
}

/// How an ELD read ended: read, malformed (all bytes read, not parsed) or
/// unreadable (the read stopped).
fn eld_kind(eld: &Result<hd_audio::hda::hdmi::Eld, hd_audio::Error>) -> &'static str {
    match eld {
        Ok(_) => "read",
        Err(hd_audio::Error::EldParse(_)) => "malformed",
        Err(_) => "unreadable",
    }
}

/// One failed ELD read of `entry`: the check that stopped it, and the
/// bytes read before it stopped (or all of them, malformed).
fn report_eld_failure(
    entry: &HdmiEntry,
    read: u8,
    error: &hd_audio::Error,
    raw: &[u8],
    more: &str,
) {
    let failed = Err(*error);
    let kind = eld_kind(&failed);
    output::arguments(format_args!(
        "GUEST AUDIO HDMI ELD pin {:#04x} device {}: {kind} on read {read} of up to {}: {}{more}\n",
        entry.pin,
        entry.device,
        hdmi_codec::ELD_READS,
        eld_reason(error),
    ));
    output::arguments(format_args!(
        "GUEST AUDIO HDMI ELD pin {:#04x} device {} bytes of read {read} ({} {}): {}\n",
        entry.pin,
        entry.device,
        raw.len(),
        if kind == "malformed" {
            "read, malformed"
        } else {
            "read before it stopped"
        },
        hex_bytes(raw)
    ));
}

/// The claimed controller's codecs, HDMI/DP pins and the ELDs on them;
/// the first entry whose ELD takes 48 kHz 16-bit stereo, else the first
/// present with ELD_VALID set whose ELD stayed unreadable or malformed,
/// for basic audio (`hdmi::choose_hdmi_target`).
fn report_display_audio(host: &mut Host, gpu: Address) -> Option<Target> {
    let location = host.location.clone();
    let proof = match host.vouch {
        stela_drivers::probe::native::hda::Vouch::BusReset => {
            alloc::format!("vouched by GPU {gpu}'s Secondary Bus Reset")
        }
        _ => String::from(
            "vouched by its observed quiesced state (--guest-audio-hdmi-trust-quiesced)",
        ),
    };
    match host.capabilities() {
        Ok((capabilities, codecs)) => output::arguments(format_args!(
            "GUEST AUDIO HDMI {location}: claimed, {proof}; codecs={codecs:#06x} \
             {capabilities:?}\n"
        )),
        Err(error) => output::arguments(format_args!(
            "GUEST AUDIO HDMI {location}: claimed, but its controller is unavailable: {error:?}\n"
        )),
    }
    for failure in &host.codec_failures {
        output::arguments(format_args!(
            "GUEST AUDIO HDMI {location} codec failure: {failure:?}\n"
        ));
    }
    if let Some(error) = host.discovery_error {
        output::arguments(format_args!(
            "GUEST AUDIO HDMI {location} discovery: {error:?}\n"
        ));
    }
    for codec in &host.codecs {
        let graph = &codec.graph;
        let setup = match &codec.setup {
            Ok(setup) => alloc::format!("{setup}"),
            Err(error) => alloc::format!("failed: {error:?}"),
        };
        output::arguments(format_args!(
            "GUEST AUDIO HDMI {location} codec {}: vendor={:08x} revision={:08x} groups={} \
             setup={setup}\n",
            graph.address,
            graph.vendor_id,
            graph.revision_id,
            graph.groups.len()
        ));
    }
    let surveys = match host.survey_hdmi() {
        Ok(surveys) => surveys,
        Err(error) => {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI {location} survey unavailable: {error:?}\n"
            ));
            return None;
        }
    };
    let mut surveyed: Vec<(u8, HdmiEntry)> = Vec::new();
    for (codec, survey) in surveys {
        let entries = match survey {
            Ok(entries) => entries,
            Err(error) => {
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI {location} codec {codec}: no survey: {error:?}\n"
                ));
                continue;
            }
        };
        for entry in entries {
            let eld = match &entry.eld {
                None => String::from("none"),
                Some(read) if read.reads > 1 => {
                    alloc::format!("{} after {} reads", eld_kind(&read.eld), read.reads)
                }
                Some(read) => String::from(eld_kind(&read.eld)),
            };
            output::arguments(format_args!(
                "GUEST AUDIO HDMI {location} codec {codec} pin {:#04x} device {}/{}: \
                 default={:#010x} caps={:#010x} sense={:#010x} present={} eld_valid={} eld={eld}\n",
                entry.pin,
                entry.device,
                entry.devices,
                entry.pin_default,
                entry.pin_capabilities,
                entry.sense,
                entry.present(),
                entry.eld_valid(),
            ));
            if let Some(read) = &entry.eld {
                report_eld(&entry, read);
            }
            surveyed.push((codec, entry));
        }
    }
    let (codec, entry, audio) =
        hdmi_codec::choose_hdmi_target(surveyed.iter().map(|(c, e)| (*c, e)), HDMI_FORMAT)?;
    let without_eld = match audio {
        SinkAudio::Eld => None,
        SinkAudio::Basic(error) => Some(alloc::format!(
            "present with ELD_VALID set, but its ELD stayed {} after {} reads: {}",
            eld_kind(&Err(error)),
            entry.eld.as_ref().map_or(0, |read| read.reads),
            eld_reason(&error)
        )),
    };
    Some(Target {
        codec,
        pin: entry.pin,
        device: entry.device,
        without_eld,
    })
}

/// An entry's ELD reads: the first failure when it was read again, then
/// the last read's ELD and bytes, or the check that stopped it and the
/// bytes read before.
fn report_eld(entry: &HdmiEntry, read: &EldRead) {
    use stela_boot_core::guest_audio::{audio_descriptor, speakers};
    if let Some((error, raw)) = &read.first_failure {
        let again = alloc::format!(
            "; reading it again every {} ms, up to {} reads",
            hdmi_codec::ELD_REPOLL_US / 1000,
            hdmi_codec::ELD_READS
        );
        report_eld_failure(entry, 1, error, raw, &again);
    }
    let eld = match &read.eld {
        Ok(eld) => eld,
        Err(error) => {
            report_eld_failure(entry, read.reads, error, &read.raw, "");
            return;
        }
    };
    if read.reads > 1 {
        output::arguments(format_args!(
            "GUEST AUDIO HDMI ELD pin {:#04x} device {}: valid on read {} of up to {}\n",
            entry.pin,
            entry.device,
            read.reads,
            hdmi_codec::ELD_READS
        ));
    }
    // The length used: the header's BLL, not the codec's buffer size.
    output::arguments(format_args!(
        "GUEST AUDIO HDMI ELD pin {:#04x} device {}: length {} bytes (4-byte header, baseline of \
         {} dwords) of the codec's {}-byte ELD buffer\n",
        entry.pin,
        entry.device,
        read.raw.len(),
        read.raw.get(2).copied().unwrap_or(0),
        read.buffer.unwrap_or(0),
    ));
    let sads: Vec<_> = eld
        .descriptors
        .iter()
        .map(|sad| audio_descriptor(sad.coding_type, sad.channels, sad.rates, sad.sample_bits))
        .collect();
    output::arguments(format_args!(
        "GUEST AUDIO HDMI ELD pin {:#04x} device {}: monitor=\"{}\" \
         connection={:?} version={} manufacturer={:#06x} product={:#06x} \
         port={:#x} speakers={:#04x}[{}] sync_delay_ms={} sads=[{}]\n",
        entry.pin,
        entry.device,
        alloc::string::String::from_utf8_lossy(&eld.monitor_name),
        eld.connection,
        eld.version,
        eld.manufacturer,
        eld.product,
        eld.port_id,
        eld.speakers,
        speakers(eld.speakers),
        eld.audio_sync_delay_ms,
        sads.join(", "),
    ));
    output::arguments(format_args!(
        "GUEST AUDIO HDMI ELD pin {:#04x} device {} bytes: {}\n",
        entry.pin,
        entry.device,
        hex_bytes(&read.raw)
    ));
}

/// Sets up the playback on `target` of the claimed function at `address`,
/// whose route starts at the first service turn; where it plays.
fn hdmi_playback(
    host: &mut Host,
    address: Address,
    (codec, pin, device): (u8, u8, u8),
) -> Option<(HdmiPlayback, String)> {
    let place = alloc::format!("{address} codec {codec} pin {pin:#04x} device {device}");
    let port = Arc::new(MonitorPort {
        identity: DisplayAudioIdentity {
            hda_function: address,
            gpu_function: Address {
                function: 0,
                ..address
            },
            codec,
            pin,
            device,
            port: u64::from(pin) << 8 | u64::from(device),
        },
    });
    // SAFETY: the port is this controller's only one on this codec, pin
    // and device entry, kept with the controller for the boot, and the
    // GPU's display owner keeps the connector it belongs to.
    match unsafe {
        host.hdmi_playback(
            (codec, pin, device),
            port,
            HDMI_FORMAT,
            HDMI_PERIOD_BYTES,
            HDMI_PERIODS,
        )
    } {
        Ok(playback) => Some((playback, place)),
        Err(error) => {
            output::arguments(format_args!(
                "GUEST AUDIO HDMI no playback on {place}: {error:?}\n"
            ));
            None
        }
    }
}
