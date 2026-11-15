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
//! 16-bit stereo in six 10 ms periods (`Hdmi`). Each turn moves the guest's
//! frames to a host queue under the devices' lock, then, in the loader's
//! root without waiting for the host drivers' lock, writes each period the
//! DMA completed: the queue's oldest frames, silence for any it lacks. More
//! than 20 ms left queued after that is dropped, oldest first. A route an
//! underrun or a sink change stopped starts again at a later turn, at most
//! once a second.
use super::{host, output, storage::RestoreRoot};
use alloc::{collections::VecDeque, string::String, sync::Arc, vec, vec::Vec};
use stela_boot_core::guest_audio::{
    self, BACKLOG_FRAMES, Counters, MAX_STEP_FRAMES, Pacer, Report, Step, s16_period,
};
use stela_drivers::{
    audio::{
        self as hd_audio, PcmFormat,
        hda::hdmi::{DisplayAudioIdentity, DisplayAudioPort, DisplayAudioState},
    },
    bus::pci::Address,
    probe::native::hda::{HdmiPlayback, Host, RetirementError},
};
use stela_soc::machine::{AppleDevices, USB_AUDIO_CHANNELS, UsbAudioStatus};

type Frame = [i32; USB_AUDIO_CHANNELS];

/// The HDMI output's stream: 48 kHz 16-bit stereo, which every HDMI sink
/// takes (`Eld::supports`'s baseline).
const HDMI_FORMAT: PcmFormat = PcmFormat {
    rate: 48_000,
    bits: 16,
    channels: 2,
};
/// One period: 10 ms, 1920 bytes, a multiple of the HDA's 128 bytes.
const HDMI_PERIOD_FRAMES: usize = 480;
const HDMI_PERIOD_BYTES: usize = HDMI_PERIOD_FRAMES * 4;
/// Six periods, 60 ms: a service gap of up to about 55 ms keeps the DMA
/// fed; a period plays about 50 ms after it is written.
const HDMI_PERIODS: usize = 6;
/// The wait before a route that could not start is tried again.
const HDMI_RETRY_TICKS: u64 = host::COUNTER_HZ;

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
    pub(super) fn new(display: Option<DisplayAudio>, root: u64) -> Self {
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
                (
                    Some(Hdmi {
                        host,
                        playback,
                        root,
                        place,
                        pending: VecDeque::with_capacity(MAX_STEP_FRAMES as usize),
                        period: vec![0; HDMI_PERIOD_BYTES],
                        refills: 0,
                        restarts: 0,
                        retry_at: 0,
                        error: None,
                        reported: None,
                    }),
                    Some(DisplayAudio {
                        hosts,
                        playback: None,
                    }),
                )
            }
            display => {
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
    restarts: u64,
    /// When a route that could not start is tried again.
    retry_at: u64,
    /// The last error, reported once.
    error: Option<RetirementError>,
    reported: Option<(bool, u64, u64)>,
}

/// Whether restart `count` gets lines of its own: the first four, then
/// each power of two; the report's count covers the rest.
fn logged(count: u64) -> bool {
    count <= 4 || count.is_power_of_two()
}

impl Hdmi {
    fn play(&mut self, now: u64, streaming: bool, counters: &mut Counters) {
        // SAFETY: the loader root `storage::open` captured, which maps the
        // claimed function's registers and DMA pages for the whole run; a
        // service turn runs with its processor's pinned record active.
        let Some(_root) = (unsafe { RestoreRoot::try_enter(self.root) }) else {
            return;
        };
        if !self.playback.running() {
            if now < self.retry_at {
                return;
            }
            // A route an error left prepared closes first; closing none
            // changes nothing.
            let _ = self.host.stop_hdmi(&mut self.playback);
            match self.host.start_hdmi(&mut self.playback) {
                Ok(()) => {
                    if self.refills == 0 && self.restarts == 0 {
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI playing on {}: converter {:#04x}, stream tag {}, \
                             48 kHz 16-bit stereo, {HDMI_PERIODS} periods of 10 ms\n",
                            self.place,
                            self.playback.converter(),
                            self.playback.stream_tag(),
                        ));
                    } else if logged(self.restarts) {
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI playing again on {} (restart {})\n",
                            self.place, self.restarts
                        ));
                    }
                    self.error = None;
                }
                Err(error) => {
                    self.retry_at = now.wrapping_add(HDMI_RETRY_TICKS);
                    if self.error != Some(error) {
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI cannot start on {}: {error:?}; trying again \
                             each second\n",
                            self.place
                        ));
                        self.error = Some(error);
                    }
                    return;
                }
            }
        }
        loop {
            let take = self.pending.len().min(HDMI_PERIOD_FRAMES);
            s16_period(self.pending.iter().take(take).copied(), &mut self.period);
            match self.host.refill_hdmi(&mut self.playback, &self.period) {
                Ok(true) => {
                    self.pending.drain(..take);
                    self.refills += 1;
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
                Ok(false) => break,
                Err(error) => {
                    self.restarts += 1;
                    if logged(self.restarts) {
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI stopped on {} (restart {}): {error:?}; starting \
                             again\n",
                            self.place, self.restarts
                        ));
                    }
                    self.error = Some(error);
                    break;
                }
            }
        }
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

    fn report(&mut self) {
        let state = (self.playback.running(), self.refills, self.restarts);
        if self.reported != Some(state) {
            self.reported = Some(state);
            output::arguments(format_args!(
                "GUEST AUDIO HDMI running={} periods={} restarts={} pending={}\n",
                state.0,
                state.1,
                state.2,
                self.pending.len()
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
) -> Option<DisplayAudio> {
    use stela_drivers::probe::NativeServices;
    let gpus = super::gpu::reset_their_bus();
    if gpus.is_empty() {
        output::arguments(format_args!(
            "GUEST AUDIO HDMI not claimed: no GPU started with a Secondary Bus Reset of its bus, \
             which alone vouches for its audio function\n"
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
        for gpu in gpus {
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
            // SAFETY: function 1 of a display controller whose cold start
            // completed a Secondary Bus Reset of its own bus in this boot;
            // probe left it parked through that reset and nothing claimed
            // it since. No later reset of that bus exists in the boot.
            match unsafe {
                stela_drivers::probe::native::hda::attach_display_audio(
                    &services, address, clock, minimum,
                )
            } {
                Ok(mut host) => {
                    let target = report_display_audio(&mut host, gpu);
                    if hdmi
                        && playback.is_none()
                        && let Some(target) = target
                    {
                        playback = hdmi_playback(&mut host, address, target)
                            .map(|(output, place)| (hosts.len(), output, place));
                    }
                    hosts.push(host);
                }
                Err((error, kept)) => {
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI {address} refused: {error}\n"
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

/// The longest the claim and the codecs' discovery may take.
const CLAIM_US: u64 = 10_000_000;

/// The claimed controller's codecs, HDMI/DP pins and the ELDs on them;
/// the first entry whose ELD takes 48 kHz 16-bit stereo, as its codec,
/// pin and device entry.
fn report_display_audio(host: &mut Host, gpu: Address) -> Option<(u8, u8, u8)> {
    use stela_boot_core::guest_audio::{audio_descriptor, speakers};
    let location = host.location.clone();
    match host.capabilities() {
        Ok((capabilities, codecs)) => output::arguments(format_args!(
            "GUEST AUDIO HDMI {location}: claimed after GPU {gpu}'s bus reset; codecs={codecs:#06x} \
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
    let mut target = None;
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
                None => "none",
                Some(Ok((_, Ok(_)))) => "read",
                Some(Ok((_, Err(_)))) => "malformed",
                Some(Err(_)) => "unreadable",
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
            match &entry.eld {
                Some(Ok((raw, parsed))) => {
                    if let Ok(eld) = parsed {
                        if target.is_none() && eld.supports(HDMI_FORMAT) {
                            target = Some((codec, entry.pin, entry.device));
                        }
                        let sads: Vec<_> = eld
                            .descriptors
                            .iter()
                            .map(|sad| {
                                audio_descriptor(
                                    sad.coding_type,
                                    sad.channels,
                                    sad.rates,
                                    sad.sample_bits,
                                )
                            })
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
                    } else if let Err(error) = parsed {
                        output::arguments(format_args!(
                            "GUEST AUDIO HDMI ELD pin {:#04x} device {}: malformed: {error:?}\n",
                            entry.pin, entry.device
                        ));
                    }
                    let bytes: Vec<_> = raw.iter().map(|b| alloc::format!("{b:02x}")).collect();
                    output::arguments(format_args!(
                        "GUEST AUDIO HDMI ELD pin {:#04x} device {} bytes: {}\n",
                        entry.pin,
                        entry.device,
                        bytes.join(" ")
                    ));
                }
                Some(Err(error)) => output::arguments(format_args!(
                    "GUEST AUDIO HDMI ELD pin {:#04x} device {}: unreadable: {error:?}\n",
                    entry.pin, entry.device
                )),
                None => {}
            }
        }
    }
    target
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
