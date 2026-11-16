//! Conventional HDA through one original-root/config/BAR/DMA lifetime.
use super::{Failure, NativeServices, ServiceError, map};
use crate::{
    Clock,
    audio::{
        self,
        hda::{self, CodecFailure, CodecGraph, CodecSetup},
    },
    bus::{
        mmio::{Mmio, owned::Mapping},
        pci::Address,
    },
    probe::Error,
};
use alloc::{boxed::Box, string::String, vec::Vec};
mod lifetime;
pub use lifetime::Error as RetirementError;
use lifetime::{Inputs, Native, OriginalUsers};

/// HD Audio BAR0 through its actual owned uncached mapping/native widths.
pub struct Bar0 {
    mmio: Mmio,
    bytes: usize,
    mapping: Mapping,
}
impl hda::Registers for Bar0 {
    fn region_bytes(&self) -> usize {
        self.bytes
    }
    fn read8(&mut self, o: usize) -> u8 {
        self.mmio.read8(o)
    }
    fn read16(&mut self, o: usize) -> u16 {
        self.mmio.read16(o)
    }
    fn read32(&mut self, o: usize) -> u32 {
        self.mmio.read32(o)
    }
    fn write8(&mut self, o: usize, v: u8) {
        self.mmio.write8(o, v);
    }
    fn write16(&mut self, o: usize, v: u16) {
        self.mmio.write16(o, v);
    }
    fn write32(&mut self, o: usize, v: u32) {
        self.mmio.write32(o, v);
    }
}
pub struct Codec {
    pub graph: CodecGraph,
    pub setup: Result<CodecSetup, audio::Error>,
}
/// The real register provider/core/DMA allocator never escape independently.
/// Discovery is metadata; a retained success alone is not guest audio readiness.
pub struct Host {
    pub location: String,
    owner: Native<Bar0, audio::Timing<Clock>>,
    pub codecs: Vec<Codec>,
    pub codec_failures: Vec<CodecFailure>,
    pub discovery_error: Option<audio::Error>,
    /// What vouched for the function's original users.
    pub vouch: Vouch,
}

/// What vouches for the original users of a GPU's HDMI/DP audio function
/// claimed after its GPU started ([`attach_display_audio`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vouch {
    /// The GPU's cold start reset their bus in this boot.
    BusReset,
    /// No bus reset did; the function was observed quiesced
    /// ([`Quiescence::quiet`]), after its own function-local reset when it
    /// was not. Only by the owner's explicit choice.
    Quiesced,
    /// Probe's conventional controllers: nothing vouches.
    Nothing,
}

/// A GPU audio function's state as the claim found it, before its
/// controller was stopped: the configuration's command register and the
/// HD Audio registers that show a running controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quiescence {
    pub command: u16,
    pub gcap: u16,
    pub gctl: u32,
    pub intctl: u32,
    pub intsts: u32,
    pub corbctl: u8,
    pub rirbctl: u8,
    /// The stream descriptors whose RUN bit was set, by index.
    pub running_streams: u32,
    /// BAR 0 answered all ones: its memory decode was off.
    pub unanswered: bool,
    /// The function's own reset, done because the first observation was
    /// not quiet; this observation is the one after it.
    pub reset: Option<crate::bus::pci::ResetMethod>,
}

impl Quiescence {
    const COMMAND_MEMORY: u16 = 1 << 1;
    const COMMAND_MASTER: u16 = 1 << 2;
    /// GCTL.CRST: the controller is out of reset.
    const RUNNING_LINK: u32 = 1;
    /// INTCTL.GIE and CIE.
    const INTERRUPTS: u32 = 3 << 30;
    /// CORBCTL.CORBRUN, RIRBCTL.RIRBDMAEN.
    const DMA_RUN: u8 = 1 << 1;
    const SD_RUN: u8 = 1 << 1;

    /// Whether nothing of the function can run or interrupt: bus mastering
    /// off, and its controller in reset, not decoding, or running no DMA
    /// engine with its interrupts disabled.
    pub fn quiet(&self) -> bool {
        self.command & Self::COMMAND_MASTER == 0
            && (self.unanswered
                || self.command & Self::COMMAND_MEMORY == 0
                || self.gctl & Self::RUNNING_LINK == 0
                || (self.running_streams == 0
                    && self.corbctl & Self::DMA_RUN == 0
                    && self.rirbctl & Self::DMA_RUN == 0
                    && self.intctl & Self::INTERRUPTS == 0))
    }

    /// Reads the controller's state through `registers` and the command
    /// register `command`. Reads only.
    fn observe(registers: &mut impl hda::Registers, command: u16) -> Self {
        let gcap = registers.read16(0);
        if gcap == u16::MAX {
            return Self {
                command,
                gcap,
                unanswered: true,
                ..Self::default()
            };
        }
        let streams = ((gcap >> 8) & 15) + ((gcap >> 12) & 15) + ((gcap >> 3) & 31);
        let running_streams = (0..usize::from(streams).min(30))
            .filter(|&index| registers.read8(0x80 + 0x20 * index) & Self::SD_RUN != 0)
            .fold(0, |mask, index| mask | 1 << index);
        Self {
            command,
            gcap,
            gctl: registers.read32(0x08),
            intctl: registers.read32(0x20),
            intsts: registers.read32(0x24),
            corbctl: registers.read8(0x4c),
            rirbctl: registers.read8(0x5c),
            running_streams,
            unanswered: false,
            reset: None,
        }
    }
}
impl Host {
    /// Explicit one-shot hardware/root/lease/Attachment/mapping retirement.
    /// Failure and unwind keep the complete graph and permanently close entry.
    pub fn shutdown(&mut self) -> Result<(), RetirementError> {
        self.owner.close()
    }

    /// The controller's capabilities and the codecs its link found.
    pub fn capabilities(&mut self) -> Result<(hda::Capabilities, u16), RetirementError> {
        let (controller, _) = self.owner.controller()?;
        Ok((controller.capabilities(), controller.codec_mask()))
    }

    /// Each NVIDIA HDMI codec's pins and device entries, with their sense
    /// and ELD (`hda::Controller::survey_hdmi`), by codec address; other
    /// codecs are left out.
    pub fn survey_hdmi(&mut self) -> Result<Vec<HdmiSurvey>, RetirementError> {
        let (controller, _) = self.owner.controller()?;
        Ok(self
            .codecs
            .iter()
            .filter(|codec| codec.graph.vendor_id >> 16 == 0x10de)
            .map(|codec| (codec.graph.address, controller.survey_hdmi(&codec.graph)))
            .collect())
    }
}

/// One codec's HDMI/DP entries by its address, or why it has none.
pub type HdmiSurvey = (u8, Result<Vec<hda::hdmi::HdmiEntry>, audio::Error>);

/// One HDMI/DP output the owner plays PCM to: a playback stream of the
/// controller routed to one pin's device entry (`hda::hdmi`). The stream is
/// allocated once; its route is prepared again after each loss.
pub struct HdmiPlayback {
    port: hda::hdmi::HdmiPortId,
    stream: hda::StreamId,
    converter: u8,
    period: usize,
    ring: usize,
    running: bool,
    without_eld: Option<audio::Error>,
}

impl HdmiPlayback {
    /// With the route prepared on a sink whose ELD did not read or parse
    /// (`hda::hdmi::BASIC_AUDIO` only): why.
    pub fn without_eld(&self) -> Option<audio::Error> {
        self.without_eld
    }
    pub fn converter(&self) -> u8 {
        self.converter
    }
    pub fn stream_tag(&self) -> u8 {
        self.stream.tag()
    }
    pub fn period_bytes(&self) -> usize {
        self.period
    }
    /// Whether the route is prepared and its DMA runs.
    pub fn running(&self) -> bool {
        self.running
    }
}

impl Host {
    /// Attach `owner`'s display-audio port on `target`, a codec's pin and
    /// device entry, and allocate one playback stream of `periods`
    /// periods of `period_bytes` in `format` for it, on the first digital
    /// converter the pin connects to. Nothing starts: [`Self::start_hdmi`].
    ///
    /// # Safety
    /// As [`hda::Controller::attach_hdmi_port`]: `owner` keeps and
    /// coordinates this controller's function, the codec, pin and device
    /// entry, and the GPU connector they belong to, through failures and
    /// for the boot.
    pub unsafe fn hdmi_playback(
        &mut self,
        (codec, pin, device): (u8, u8, u8),
        owner: alloc::sync::Arc<dyn hda::hdmi::DisplayAudioPort>,
        format: audio::PcmFormat,
        period_bytes: usize,
        periods: usize,
    ) -> Result<HdmiPlayback, RetirementError> {
        let graph = self
            .codecs
            .iter()
            .find(|c| c.graph.address == codec)
            .map(|c| c.graph.clone())
            .ok_or(RetirementError::Controller(audio::Error::InvalidCodecGraph))?;
        let connections = graph
            .widget(pin)
            .map(|w| w.connections.clone())
            .unwrap_or_default();
        let converter = connections
            .iter()
            .copied()
            .find(|&node| {
                graph.widget(node).is_some_and(|w| {
                    w.kind == hda::WidgetKind::Output
                        && w.capabilities & DIGITAL_WIDGET != 0
                        && w.pcm.supports(format)
                })
            })
            .ok_or(RetirementError::Controller(
                audio::Error::UnsupportedTopology,
            ))?;
        let (controller, dma) = self.owner.controller()?;
        // SAFETY: forwarded owner contract.
        let port = unsafe { controller.attach_hdmi_port(&graph, pin, device, owner) }?;
        let stream = controller.allocate_stream(
            audio::Direction::Playback,
            format,
            period_bytes,
            periods,
            dma,
        )?;
        let ring = controller.stream_buffer_bytes(stream)?;
        Ok(HdmiPlayback {
            port,
            stream,
            converter,
            period: period_bytes,
            ring,
            running: false,
            without_eld: None,
        })
    }

    /// Prepare the route (`hda::Controller::prepare_hdmi`, which reads the
    /// sink's ELD again), fill the whole ring with silence and start its
    /// DMA. A sink without ELD_VALID is `SinkUnavailable`; one whose ELD
    /// does not read or parse takes basic audio only
    /// ([`HdmiPlayback::without_eld`]).
    pub fn start_hdmi(&mut self, playback: &mut HdmiPlayback) -> Result<(), RetirementError> {
        let (controller, _) = self.owner.controller()?;
        playback.running = false;
        controller.prepare_hdmi(playback.port, playback.stream, playback.converter, None)?;
        playback.without_eld = controller
            .hdmi_sink(playback.port)?
            .and_then(|sink| sink.eld_error);
        let silence = alloc::vec![0u8; playback.ring];
        controller.write_pcm(playback.stream, 0, &silence)?;
        controller.start(playback.stream)?;
        playback.running = true;
        Ok(())
    }

    /// Refill the next completed period with `pcm`, exactly one period:
    /// whether one was due. An error stopped the stream and closed the
    /// route (an underrun, a sink change); [`Self::start_hdmi`] starts it
    /// again.
    pub fn refill_hdmi(
        &mut self,
        playback: &mut HdmiPlayback,
        pcm: &[u8],
    ) -> Result<bool, RetirementError> {
        let (controller, _) = self.owner.controller()?;
        match controller.refill_hdmi_period(playback.stream, pcm) {
            Ok(done) => Ok(done.is_some()),
            Err(error) => {
                playback.running = false;
                Err(RetirementError::Controller(error))
            }
        }
    }

    /// Stop the stream and close its route, if it runs.
    pub fn stop_hdmi(&mut self, playback: &mut HdmiPlayback) -> Result<(), RetirementError> {
        let (controller, _) = self.owner.controller()?;
        playback.running = false;
        controller.close_hdmi(playback.port)?;
        Ok(())
    }
}

/// A converter widget with digital output (HDA 7.3.4.6).
const DIGITAL_WIDGET: u32 = 1 << 9;

/// What vouches for the original users of the function an attach takes.
enum Users<'a> {
    /// Nothing: the conventional controller is retained and refused.
    Unavailable,
    /// The function's GPU reset its bus in this boot ([`OriginalUsers::after_bus_reset`]).
    AfterBusReset,
    /// The function observed quiesced ([`OriginalUsers::after_quiescence`]),
    /// each observation handed to `report`.
    Quiesced {
        report: &'a mut dyn FnMut(&Quiescence),
    },
}
/// Observes the function's controller ([`Quiescence`]); when it is not
/// quiet, resets the function alone if it can and observes it again. Each
/// observation goes to `report`. `Err` when it stays unquiet.
#[expect(
    clippy::result_large_err,
    reason = "A refused claim keeps probe's typed cause, as every HDA attach does"
)]
fn quiesce(
    cold: &mut ColdOwner,
    registers: &mut Bar0,
    report: &mut dyn FnMut(&Quiescence),
) -> Result<(), Error> {
    use crate::bus::pci::ConfigSpace;
    let observe = |cold: &mut ColdOwner, registers: &mut Bar0| -> Result<Quiescence, Error> {
        let function = cold.requester.as_mut().unwrap().function_mut();
        let command = function
            .config()
            .read16(0x04)
            .map_err(|error| Error::Native(ServiceError::Config(error)))?;
        Ok(Quiescence::observe(registers, command))
    };
    let first = observe(cold, registers)?;
    report(&first);
    if first.quiet() {
        return Ok(());
    }
    let timer = cold.minimum.clone().expect("cold minimum timer");
    let function = cold.requester.as_mut().unwrap().function_mut();
    let method = function
        .reset(&*timer)
        .map_err(|_| {
            Error::Native(ServiceError::Refused(
                "its controller was running and its function-local reset failed",
            ))
        })?
        .ok_or(Error::Native(ServiceError::Refused(
            "its controller was running and it has no function-local reset",
        )))?;
    crate::bus::pci::requester::validate_stopped(function)
        .map_err(|error| Error::Native(ServiceError::Claim(error)))?;
    let mut second = observe(cold, registers)?;
    second.reset = Some(method);
    report(&second);
    if second.quiet() {
        Ok(())
    } else {
        Err(Error::Native(ServiceError::Refused(
            "its controller still runs after its own reset",
        )))
    }
}

struct ColdOwner {
    requester: Option<crate::bus::pci::requester::preparation::PreparingRequester>,
    mapping: Option<Mapping>,
    minimum: Option<alloc::rc::Rc<dyn crate::bus::pci::power::Timer>>,
}
impl Drop for ColdOwner {
    fn drop(&mut self) {
        core::mem::forget(self.requester.take());
        core::mem::forget(self.mapping.take());
        core::mem::forget(self.minimum.take());
    }
}
/// # Safety
/// NativeServices ownership, calibrated clock and original D0/decode/power
/// hold. Conventional identity alone cannot exclude firmware/AML/SMM/DSP users.
/// Their original root/backing and competing-owner exclusions remain required.
/// The separate original IRQ/work retirement producer is currently unavailable:
/// this factory performs source containment, then retains/refuses before bind.
#[expect(
    clippy::result_large_err,
    reason = "A failed HDA attach keeps its typed cause and all original resource owners"
)]
pub(in crate::probe) unsafe fn attach(
    services: &dyn NativeServices,
    address: Address,
    clock: Clock,
    minimum: alloc::rc::Rc<dyn crate::bus::pci::power::Timer>,
) -> Result<Host, Failure> {
    // SAFETY: forwarded.
    unsafe { attach_with(services, address, clock, minimum, Users::Unavailable) }
}

/// The HD Audio function of a GPU, the HDMI/DP audio controller probe
/// left parked for the GPU's start (`probe::display_audio`), claimed once
/// the GPU started. `vouch` names what retired its original users:
///
/// - [`Vouch::BusReset`]: the GPU's cold start reset their bus.
/// - [`Vouch::Quiesced`]: the claim observes the function first
///   ([`Quiescence`], each observation handed to `report`). Its
///   configuration must show it stopped (bus mastering, MSI, MSI-X and INTx
///   off), and its controller must be quiet ([`Quiescence::quiet`]). If the
///   controller is not quiet and the function has a function-local reset
///   (FLR, else D3hot), the claim resets the function alone and observes it
///   again. A function that stays unquiet is refused.
///
/// The controller then binds a private domain and its codecs are
/// discovered, all polled. A function that is not an HD Audio controller is
/// refused, and every failure keeps the original owners in the returned
/// graph, which the caller keeps for the boot.
///
/// # Safety
/// As for probe's HD Audio attach, and: `address` is a function of the
/// device whose function 0 is a display controller that completed its cold
/// start in this boot. The function stayed parked in the contained fabric
/// since boot, and nothing claimed or enabled it since. For
/// [`Vouch::BusReset`], that cold start completed a Secondary Bus Reset of
/// its own bus (`gpu::cold::Reset::SecondaryBus`). For [`Vouch::Quiesced`],
/// the owner chose explicitly to trust what [`OriginalUsers::after_quiescence`]
/// requires. No later reset of that bus runs while the returned host is live.
#[expect(
    clippy::result_large_err,
    reason = "A failed HDA attach keeps its typed cause and all original resource owners"
)]
pub unsafe fn attach_display_audio(
    services: &dyn NativeServices,
    address: Address,
    clock: Clock,
    minimum: alloc::rc::Rc<dyn crate::bus::pci::power::Timer>,
    vouch: Vouch,
    report: &mut dyn FnMut(&Quiescence),
) -> Result<Host, (Error, Option<Box<dyn core::any::Any>>)> {
    let users = match vouch {
        Vouch::BusReset => Users::AfterBusReset,
        Vouch::Quiesced => Users::Quiesced { report },
        Vouch::Nothing => Users::Unavailable,
    };
    // SAFETY: forwarded, with what retired the original users.
    unsafe { attach_with(services, address, clock, minimum, users) }
}

/// # Safety
/// As for [`attach`]; [`Users::AfterBusReset`] as for
/// [`attach_display_audio`].
#[expect(
    clippy::result_large_err,
    reason = "A failed HDA attach keeps its typed cause and all original resource owners"
)]
unsafe fn attach_with(
    services: &dyn NativeServices,
    address: Address,
    clock: Clock,
    minimum: alloc::rc::Rc<dyn crate::bus::pci::power::Timer>,
    mut users: Users<'_>,
) -> Result<Host, Failure> {
    let vouch = match users {
        Users::Unavailable => Vouch::Nothing,
        Users::AfterBusReset => Vouch::BusReset,
        Users::Quiesced { .. } => Vouch::Quiesced,
    };
    minimum.check().map_err(|error| {
        (
            Error::HdaNative(RetirementError::Controller(audio::Error::MinimumDelay(
                error,
            ))),
            None,
        )
    })?;
    // Check the largest selected conventional-core wait before any config or
    // vendor effect. Generic clock metadata alone does not qualify a timer.
    clock
        .check_duration(1_000_000)
        .map_err(|error| (Error::HdaNative(RetirementError::Clock(error)), None))?;
    let requester = services
        .prepare_requester(address)
        .map_err(|e| (Error::Native(e), None))?;
    let mut cold = ColdOwner {
        requester: Some(requester),
        mapping: None,
        minimum: Some(minimum),
    };
    let bars = match cold.requester.as_mut().unwrap().function_mut().bars() {
        Ok(bars) => bars,
        Err(error) => return Err((error.into(), Some(Box::new(cold)))),
    };
    let mapping = match map(services, address, &bars, 0) {
        Ok(m) => m,
        Err(e) => return Err((Error::Native(e), Some(Box::new(cold)))),
    };
    cold.mapping = Some(mapping);
    let retained = match users {
        Users::Unavailable => None,
        Users::AfterBusReset | Users::Quiesced { .. } => {
            let function = cold.requester.as_mut().unwrap().function_mut();
            if function.class[..2] != [0x04, 0x03] {
                return Err((
                    Error::Native(ServiceError::Refused("not an HD Audio controller")),
                    Some(Box::new(cold)),
                ));
            }
            if let Users::Quiesced { .. } = users
                && let Err(error) = crate::bus::pci::requester::validate_stopped(function)
            {
                return Err((
                    Error::Native(ServiceError::Claim(error)),
                    Some(Box::new(cold)),
                ));
            }
            match function.config().retain_epoch() {
                Ok(retained) => Some(retained),
                Err(error) => {
                    return Err((
                        Error::Native(ServiceError::Mapping(error)),
                        Some(Box::new(cold)),
                    ));
                }
            }
        }
    };
    let bar = bars[0].expect("map checked BAR0");
    let window = (|| {
        let mapping = cold.mapping.as_ref().unwrap();
        let bytes = mapping.bytes()?;
        Ok((mapping.address(0, bytes)?, bytes))
    })();
    let (window, bytes) = match window {
        Ok(v) => v,
        Err(e) => {
            return Err((
                Error::Native(ServiceError::Mapping(e)),
                Some(Box::new(cold)),
            ));
        }
    };
    let mut registers = Bar0 {
        // SAFETY: exactly this original owned UC BAR0; Native outer owner
        // keeps actual mapping across constructor error and callback unwind.
        mmio: unsafe { Mmio::new(window.as_ptr() as u64, bytes) },
        bytes,
        mapping: cold.mapping.take().unwrap(),
    };
    let original_users = match (&mut users, retained) {
        (_, None) => None,
        (Users::AfterBusReset, Some(retained)) => {
            // SAFETY: this function's own epoch; the caller vouches for the
            // bus reset that retired its original users.
            Some(unsafe { OriginalUsers::after_bus_reset(retained) })
        }
        (Users::Quiesced { report }, Some(retained)) => {
            match quiesce(&mut cold, &mut registers, &mut **report) {
                // SAFETY: this function's own epoch, observed quiet; the
                // owner chose to trust that observation.
                Ok(()) => Some(unsafe { OriginalUsers::after_quiescence(retained) }),
                Err(error) => return Err((error, Some(Box::new((cold, registers))))),
            }
        }
        (Users::Unavailable, Some(_)) => None,
    };
    let inputs = Inputs {
        requester: cold.requester.take().unwrap(),
        registers,
        clock: audio::Timing::new(clock, cold.minimum.take().unwrap()),
        pages: services.pages(),
        bar,
        // Only a bus reset in this boot produces the original-owner
        // IRQ/CPU/EOI/service/work retirement. Post-EBS serialization, source
        // masks or polled operation do not construct one; without it Native
        // retains/refuses before root drain/bind.
        original_users,
        // Only a function vouched for by its quiesced state, by the owner's
        // explicit choice, takes NVIDIA's coherence bytes as Linux does.
        gpu_audio: vouch == Vouch::Quiesced,
    };
    let mut owner = match Native::new(inputs) {
        Ok(owner) => owner,
        Err(failure) => {
            let error = failure
                .construction_error()
                .cloned()
                .map_or(Error::HdaNative(failure.error), Error::Device);
            return Err((error, Some(Box::new(failure))));
        }
    };
    let discovery = match owner.discover_and_setup() {
        Ok(d) => d,
        Err(error) => return Err((Error::HdaNative(error), Some(Box::new(owner)))),
    };
    Ok(Host {
        vouch,
        location: alloc::format!("{address}"),
        owner,
        codecs: discovery.codecs,
        codec_failures: discovery.codec_failures,
        discovery_error: discovery.discovery_error,
    })
}

#[cfg(test)]
mod quiescence_tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// A controller's registers as values by offset; the rest read zero.
    struct Values(BTreeMap<usize, u32>, bool);
    impl hda::Registers for Values {
        fn region_bytes(&self) -> usize {
            0x4000
        }
        fn read8(&mut self, o: usize) -> u8 {
            self.read32(o) as u8
        }
        fn read16(&mut self, o: usize) -> u16 {
            self.read32(o) as u16
        }
        fn read32(&mut self, o: usize) -> u32 {
            if self.1 {
                return u32::MAX;
            }
            self.0.get(&o).copied().unwrap_or(0)
        }
        fn write8(&mut self, _: usize, _: u8) {
            panic!("an observation writes nothing");
        }
        fn write16(&mut self, _: usize, _: u16) {
            panic!("an observation writes nothing");
        }
        fn write32(&mut self, _: usize, _: u32) {
            panic!("an observation writes nothing");
        }
    }

    /// GCAP of NVIDIA's HDMI controllers: 4 input, 4 output, no
    /// bidirectional streams, 64-bit.
    const GCAP: u32 = 0x4401;
    const MEMORY: u16 = 1 << 1;
    const MASTER: u16 = 1 << 2;

    fn observe(values: &[(usize, u32)], command: u16) -> Quiescence {
        let mut registers = Values(values.iter().copied().collect(), false);
        Quiescence::observe(&mut registers, command)
    }

    #[test]
    fn a_controller_in_reset_or_idle_with_interrupts_off_is_quiet() {
        // In reset: whatever else reads.
        let reset = observe(&[(0, GCAP), (0x20, 0xc000_0000)], MEMORY);
        assert_eq!(reset.gcap, 0x4401);
        assert!(reset.quiet());
        // Out of reset, nothing running, interrupts off.
        let idle = observe(&[(0, GCAP), (0x08, 1), (0x24, 0x8000_0000)], MEMORY);
        assert_eq!(
            (idle.gctl, idle.intsts, idle.running_streams),
            (1, 0x8000_0000, 0)
        );
        assert!(idle.quiet());
        // Not decoding: nothing the CPU set up can run through it.
        let off = Quiescence::observe(&mut Values(BTreeMap::new(), true), 0);
        assert!(off.unanswered && off.quiet());
        assert!(observe(&[(0, GCAP), (0x08, 1), (0x4c, 2)], 0).quiet());
    }

    #[test]
    fn running_dma_armed_interrupts_or_bus_mastering_are_not_quiet() {
        // Output stream 0 is descriptor 4, after the four inputs.
        let stream = observe(&[(0, GCAP), (0x08, 1), (0x80 + 0x20 * 4, 2)], MEMORY);
        assert_eq!(stream.running_streams, 1 << 4);
        assert!(!stream.quiet());
        for (offset, value) in [(0x4c, 2), (0x5c, 2), (0x20, 0x8000_0000)] {
            let state = observe(&[(0, GCAP), (0x08, 1), (offset, value)], MEMORY);
            assert!(!state.quiet(), "{offset:#x}");
        }
        // Bus mastering on is never quiet, even in reset.
        assert!(!observe(&[(0, GCAP)], MEMORY | MASTER).quiet());
    }
}
