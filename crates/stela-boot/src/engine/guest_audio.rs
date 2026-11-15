//! The guest's USB speaker on the host (`--guest-audio`,
//! `stela_soc::usb::audio`): each service turn takes the frames macOS sent
//! since the last one, under the devices' lock.
//!
//! The host's output is a sink that consumes 48 kHz of the host's counter
//! (`stela_boot_core::guest_audio::Pacer`): what the guest sent is counted
//! and discarded, a turn that owes more than waits counts silence, and more
//! than 20 ms left waiting is dropped, oldest first. The `GUEST AUDIO` line
//! reports the counts every report period when they changed.
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
}

impl GuestAudio {
    pub(super) fn new() -> Self {
        output::arguments(format_args!(
            "GUEST AUDIO speaker on the native hub: 48 kHz stereo, played to a sink at the \
             host's counter\n"
        ));
        Self {
            pacer: Pacer::new(host::COUNTER_HZ),
            counters: Counters::default(),
            // A turn takes at most the guest ring's 200 ms.
            frames: vec![[0; USB_AUDIO_CHANNELS]; MAX_STEP_FRAMES as usize],
            status: None,
            streaming: 0,
            reported: None,
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
