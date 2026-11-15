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
//! the monitor's ELD reported (`claim_display_audio`); nothing plays on it
//! yet.
use super::{host, output};
use alloc::{vec, vec::Vec};
use stela_boot_core::guest_audio::{self, Counters, MAX_STEP_FRAMES, Pacer, Report};
use stela_soc::machine::{AppleDevices, USB_AUDIO_CHANNELS, UsbAudioStatus};

type Frame = [i32; USB_AUDIO_CHANNELS];

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
    /// The GPU's audio functions, claimed and kept for the boot.
    _display: Option<DisplayAudio>,
}

impl GuestAudio {
    pub(super) fn new(display: Option<DisplayAudio>) -> Self {
        output::arguments(format_args!(
            "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played to a sink at the \
             host's counter; HDMI audio functions claimed: {}\n",
            display.as_ref().map_or(0, |display| display.hosts.len())
        ));
        Self {
            pacer: Pacer::new(host::COUNTER_HZ),
            counters: Counters::default(),
            // A turn takes at most the guest ring's 200 ms.
            frames: vec![[0; USB_AUDIO_CHANNELS]; MAX_STEP_FRAMES as usize],
            status: None,
            streaming: 0,
            reported: None,
            _display: display,
        }
    }

    /// One turn, under the devices' lock: the frames 48 kHz of the host's
    /// counter owes since the last turn, at `now`. While the guest does not
    /// stream, what it left waiting is discarded and the clock stops.
    pub(super) fn serve(&mut self, devices: &mut AppleDevices, now: u64) {
        let Some((status, queued)) = devices.guest_audio_status() else {
            return;
        };
        self.status = Some((status, queued));
        if status.bits == 0 {
            self.pacer.stop();
            let leftover = queued.min(self.frames.len());
            devices.guest_audio_take(&mut self.frames[..leftover]);
            return;
        }
        let step = guest_audio::step(self.pacer.due(now), queued);
        devices.guest_audio_take(&mut self.frames[..step.dropped.min(MAX_STEP_FRAMES as usize)]);
        devices.guest_audio_take(&mut self.frames[..step.take]);
        self.counters.add(step);
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

    /// The `GUEST AUDIO` counts, when they changed since the last report.
    pub(super) fn report(&mut self) {
        let Some((status, queued)) = self.status else {
            return;
        };
        let report = Report {
            bits: status.bits,
            received: status.counters.received,
            overflowed: status.counters.overflowed,
            split: status.counters.split,
            queued,
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

/// The HD Audio functions of the GPUs whose start reset their bus, claimed
/// once that start finished (`claim_display_audio`), kept for the boot.
/// Their registers and DMA are reached through the loader's root only.
pub(super) struct DisplayAudio {
    hosts: Vec<stela_drivers::probe::native::hda::Host>,
}

/// Claims the HD Audio function (function 1) beside each GPU whose cold
/// start reset its bus in this boot (`gpu::reset_their_bus`), and reports
/// its codecs, HDMI/DP pins, their sense and ELDs: the GPU's display side
/// writes the ELD and presence of the monitor it drives with audio.
///
/// The function was left parked for that reset (`probe::display_audio`);
/// claimed only now, its original users are those the reset retired. Run
/// once the GPU's own start has finished, on the kernel work's foreground
/// as the GPU's claim is. Nothing plays.
pub(super) fn claim_display_audio(
    work: &'static super::kernel_work::KernelWork,
    mmio: &'static crate::native_mmio::Source,
    remappers: &'static super::remapping::Remappers,
    handoff: &crate::handoff::Handoff,
    clock: stela_drivers::Clock,
) -> Option<DisplayAudio> {
    use stela_drivers::{bus::pci::Address, probe::NativeServices};
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
                    report_display_audio(&mut host, gpu);
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
        hosts
    });
    match claimed {
        Ok(hosts) if !hosts.is_empty() => Some(DisplayAudio { hosts }),
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

/// The claimed controller's codecs, HDMI/DP pins and the ELDs on them.
fn report_display_audio(
    host: &mut stela_drivers::probe::native::hda::Host,
    gpu: stela_drivers::bus::pci::Address,
) {
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
            return;
        }
    };
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
}
