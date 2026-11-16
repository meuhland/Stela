//! The guest's USB speaker on the host (`--guest-audio`,
//! `stela_soc::usb::audio`): each service turn takes the frames macOS sent
//! since the last one, under the devices' lock.
//!
//! The host's output is a sink that consumes 48 kHz of the host's counter
//! (`stela_boot_core::guest_audio::Pacer`): what the guest sent is counted
//! and discarded, a turn that owes more than waits counts silence, and more
//! than 20 ms left waiting is dropped, oldest first. The `GUEST AUDIO` line
//! reports the counts every report period when they changed.
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
//! any it lacks. More than 20 ms left queued after that is dropped, oldest
//! first. A stream an underrun, a DMA FIFO error or a transient codec fault
//! stopped restarts alone at the next turn, on its route as prepared (after
//! a rejected readback, with the route's verbs set again); a monitor that
//! went away pauses the route until it returns, and its verbs are set again
//! when it does; a route that was lost is prepared again, at most once a
//! second; a controller that needs a reset gets its own CRST, at most
//! `HDMI_RESETS` times a boot, 10 s apart. Each turn reads the route back
//! once. With `--guest-audio-hdmi-tone` the route plays a 1 kHz test tone
//! instead of the guest's frames.
use super::{host, output, storage::RestoreRoot};
use alloc::{collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use stela_boot_core::guest_audio::{
    self, BACKLOG_FRAMES, Counters, Dbfs, MAX_STEP_FRAMES, Pacer, Report, RestartReason, Restarts,
    Step, Tone, s16_period,
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
use stela_soc::machine::{AppleDevices, USB_AUDIO_CHANNELS, UsbAudioStatus};

type Frame = [i32; USB_AUDIO_CHANNELS];

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

pub(super) struct GuestAudio {
    pacer: Pacer,
    counters: Counters,
    /// The frames a turn takes, before they leave the devices' lock.
    frames: Vec<Frame>,
    /// The function's last status and queue, from the last turn.
    status: Option<(UsbAudioStatus, usize)>,
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
    /// guest's sound (`--guest-audio-hdmi-tone`).
    pub(super) fn new(display: Option<DisplayAudio>, root: u64, tone: bool) -> Self {
        let (hdmi, display) = match display {
            Some(DisplayAudio {
                hosts,
                playback: Some((index, playback, place)),
            }) => {
                let mut hosts = hosts;
                let host = hosts.remove(index);
                output::arguments(format_args!(
                    "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played on the \
                     monitor: {place}\n"
                ));
                let ring_ms = HDMI_PERIODS * HDMI_PERIOD_MS;
                output::arguments(format_args!(
                    "GUEST AUDIO HDMI ring: {HDMI_PERIODS} periods of {HDMI_PERIOD_MS} ms \
                     ({ring_ms} ms, {} bytes), refilled each service turn: a turn up to about {} \
                     ms late keeps the DMA fed (the guest's own ring holds {} ms); about {} ms \
                     from the guest to the monitor at most\n",
                    HDMI_PERIODS * HDMI_PERIOD_BYTES,
                    ring_ms - HDMI_PERIOD_MS,
                    MAX_STEP_FRAMES * 1000 / guest_audio::RATE,
                    ring_ms + BACKLOG_FRAMES * 1000 / guest_audio::RATE as usize,
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
                output::arguments(format_args!(
                    "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played to a sink at \
                     the host's counter; HDMI audio functions claimed: {}\n",
                    display.as_ref().map_or(0, |display| display.hosts.len())
                ));
                (None, display)
            }
        };
        Self {
            pacer: Pacer::new(host::COUNTER_HZ),
            counters: Counters::default(),
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
        let Some((status, queued)) = devices.guest_audio_status() else {
            return;
        };
        self.status = Some((status, queued));
        let taken = queued.min(self.frames.len());
        if status.bits == 0 {
            self.pacer.stop();
            devices.guest_audio_take(&mut self.frames[..taken]);
            return;
        }
        if let Some(hdmi) = self.hdmi.as_mut() {
            let moved = devices.guest_audio_take(&mut self.frames[..taken]);
            hdmi.pending.extend(&self.frames[..moved]);
            return;
        }
        let step = guest_audio::step(self.pacer.due(now), queued);
        devices.guest_audio_take(&mut self.frames[..step.dropped.min(MAX_STEP_FRAMES as usize)]);
        devices.guest_audio_take(&mut self.frames[..step.take]);
        self.counters.add(step);
    }

    /// The monitor's turn, outside the devices' lock, at counter `now`:
    /// starts its route when it is not running and is due, writes every
    /// period the DMA completed, and trims the queue to the backlog.
    pub(super) fn play(&mut self, now: u64) {
        let streaming = self.status.is_some_and(|(status, _)| status.bits != 0);
        let Some(hdmi) = self.hdmi.as_mut() else {
            return;
        };
        hdmi.play(now, streaming, &mut self.counters);
    }

    /// Reports the guest starting or stopping its stream, outside the lock.
    pub(super) fn transitions(&mut self) {
        let bits = self.status.map_or(0, |(status, _)| status.bits);
        if bits != self.streaming {
            self.streaming = bits;
            match bits {
                0 => output::arguments(format_args!("GUEST AUDIO stream stopped\n")),
                bits => output::arguments(format_args!(
                    "GUEST AUDIO stream started: {bits}-bit stereo at 48 kHz\n"
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
        let report = Report {
            bits: status.bits,
            received: status.counters.received,
            overflowed: status.counters.overflowed,
            split: status.counters.split,
            queued: queued + self.hdmi.as_ref().map_or(0, |hdmi| hdmi.pending.len()),
            muted: status.muted,
            volume: status.volume,
            host: self.counters,
        };
        if self.reported != Some(report) {
            self.reported = Some(report);
            output::arguments(format_args!("{report}\n"));
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
    reported: Option<(bool, bool, u64, Restarts, u8)>,
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
}

/// Route state dumps per boot: each preparation of the route, and the
/// first resume after the monitor slept, up to this many.
const HDMI_STATE_DUMPS: u8 = 4;

/// Whether restart `count` (of one reason) gets lines of its own: the
/// first four, then each power of two; the report's counts cover the rest.
fn logged(count: u64) -> bool {
    count <= 4 || count.is_power_of_two()
}

/// Why the playback stopped, from the error the refill returned; the
/// driver stops the same way (`hda::hdmi`): an underrun, a DMA FIFO error
/// or a codec fault stops the stream only, a sink away pauses the route,
/// anything else closes it (a DMA descriptor error needs a controller
/// reset).
fn restart_reason(error: &RetirementError) -> RestartReason {
    use hd_audio::Error;
    match error {
        RetirementError::Controller(Error::PlaybackUnderrun | Error::InvalidPosition(_)) => {
            RestartReason::Underrun
        }
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

    fn play(&mut self, now: u64, streaming: bool, counters: &mut Counters) {
        // SAFETY: the loader root `storage::open` captured, which maps the
        // claimed function's registers and DMA pages for the whole run; a
        // service turn runs with its processor's pinned record active.
        if let Some(_root) = unsafe { RestoreRoot::try_enter(self.root) }
            && (self.playback.running() || self.start(now))
        {
            self.refill(streaming, counters);
        }
        // Running or not (paused while the monitor sleeps, waiting to
        // start again), at most the backlog waits, oldest dropped first.
        let excess = self.pending.len().saturating_sub(BACKLOG_FRAMES);
        if excess > 0 {
            self.pending.drain(..excess);
            counters.add(Step {
                dropped: excess,
                take: 0,
                silent: 0,
            });
        }
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

    /// Writes every period the DMA completed: the queue's oldest frames,
    /// silence for any it lacks.
    fn refill(&mut self, streaming: bool, counters: &mut Counters) {
        // One whole route readback a turn, at its first status read; the
        // others sense the pin only. A failure shows at the refill.
        let _ = self.host.schedule_hdmi_readback(&self.playback);
        loop {
            let take = self.pending.len().min(HDMI_PERIOD_FRAMES);
            // The test tone instead of the guest's frames, which are still
            // taken (and discarded) as they would be played.
            let tone = self.tone;
            match self.tone.as_mut() {
                Some(tone) => tone.fill(&mut self.period),
                None => {
                    s16_period(self.pending.iter().take(take).copied(), &mut self.period);
                }
            }
            match self.host.refill_hdmi(&mut self.playback, &self.period) {
                Ok(true) => {
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
                    // Silence counts only while the guest streams.
                    let silent = if streaming {
                        (HDMI_PERIOD_FRAMES - take) as u64
                    } else {
                        0
                    };
                    counters.add(Step {
                        dropped: 0,
                        take,
                        silent,
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
    }

    fn report(&mut self) {
        let state = (
            self.playback.running(),
            self.paused,
            self.refills,
            self.restarts,
            self.playback.resets(),
        );
        if self.reported != Some(state) {
            self.reported = Some(state);
            // The peak: the largest sample written since the last report.
            output::arguments(format_args!(
                "GUEST AUDIO HDMI running={} paused={} periods={} {} resets={}/{HDMI_RESETS} \
                 pending={} peak={}{}\n",
                state.0,
                state.1,
                state.2,
                state.3,
                state.4,
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
