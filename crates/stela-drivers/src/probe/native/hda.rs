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

/// How [`Host::start_hdmi`] started a playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdmiStart {
    /// The route was prepared: the codec walked, the ELD read, the
    /// converter, pin and infoframe set, the stream started.
    Prepared,
    /// The stream alone, on the route as prepared (after an underrun or a
    /// transient codec fault).
    Stream,
    /// The route's verbs set again (no codec walk), then the stream, after
    /// a readback rejected the route.
    Reprogrammed,
    /// The route's verbs set again and the stream started, after a pause:
    /// the sink came back with the same ELD.
    Resumed,
}

/// Controller resets ([`Host::reset_hdmi`]) one playback may take in a
/// boot: each allocates nothing new when the reset streams' ring and BDL
/// fit the new stream, but a reset that keeps failing must end.
pub const HDMI_RESETS: u8 = 3;
/// The least time between the starts of two controller resets.
pub const HDMI_RESET_SPACING_US: u64 = 10_000_000;

/// Why [`Host::reset_hdmi`] did not reset the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdmiResetRefusal {
    /// All [`HDMI_RESETS`] were taken: the playback is given up.
    Exhausted,
    /// The last reset began less than [`HDMI_RESET_SPACING_US`] ago; this
    /// much of the spacing remains.
    TooSoon { wait_us: u64 },
}

/// A playback's controller resets: how many were taken, and when the last
/// began. At most [`HDMI_RESETS`], [`HDMI_RESET_SPACING_US`] apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HdmiResets {
    taken: u8,
    last_us: Option<u64>,
}

impl HdmiResets {
    /// The resets taken (admitted, whether they then succeeded or not).
    pub fn taken(&self) -> u8 {
        self.taken
    }

    /// Admits one more reset at `now_us` (a monotonic microsecond clock):
    /// its number, from 1, or why not. A refusal changes nothing.
    pub fn admit(&mut self, now_us: u64) -> Result<u8, HdmiResetRefusal> {
        if self.taken >= HDMI_RESETS {
            return Err(HdmiResetRefusal::Exhausted);
        }
        if let Some(last) = self.last_us {
            let since = now_us.saturating_sub(last);
            if since < HDMI_RESET_SPACING_US {
                return Err(HdmiResetRefusal::TooSoon {
                    wait_us: HDMI_RESET_SPACING_US - since,
                });
            }
        }
        self.taken += 1;
        self.last_us = Some(now_us);
        Ok(self.taken)
    }
}

/// How [`Host::reset_hdmi`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdmiResetError {
    /// No reset was made.
    Refused(HdmiResetRefusal),
    /// Reset `reset` (from 1) was made and failed at `error`; it counts.
    Failed { reset: u8, error: RetirementError },
}

/// What [`Host::reset_hdmi`] did: the reset's number, the codecs the new
/// link found, and whether the new stream took the retired one's ring and
/// BDL (no new DMA memory).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdmiReset {
    pub reset: u8,
    pub codecs: usize,
    pub same_buffers: bool,
}

/// One HDMI/DP output the owner plays PCM to: a playback stream of the
/// controller routed to one pin's device entry (`hda::hdmi`). The stream is
/// allocated once; its route is prepared once and kept through stream
/// restarts, and prepared again only after it was lost. A controller
/// reset ([`Host::reset_hdmi`]) attaches the port and allocates the stream
/// again.
pub struct HdmiPlayback {
    port: hda::hdmi::HdmiPortId,
    stream: hda::StreamId,
    converter: u8,
    period: usize,
    ring: usize,
    running: bool,
    without_eld: Option<audio::Error>,
    /// What a controller reset attaches and allocates again.
    target: (u8, u8, u8),
    owner: alloc::sync::Arc<dyn hda::hdmi::DisplayAudioPort>,
    format: audio::PcmFormat,
    periods: usize,
    /// The ring's device address, to tell whether a reset's new stream
    /// took the same memory.
    buffer_address: u64,
    resets: HdmiResets,
}

impl HdmiPlayback {
    /// The controller resets taken.
    pub fn resets(&self) -> u8 {
        self.resets.taken()
    }
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
        let port = unsafe { controller.attach_hdmi_port(&graph, pin, device, owner.clone()) }?;
        let stream = controller.allocate_stream(
            audio::Direction::Playback,
            format,
            period_bytes,
            periods,
            dma,
        )?;
        let ring = controller.stream_buffer_bytes(stream)?;
        let buffer_address = controller.stream_buffer_address(stream)?;
        Ok(HdmiPlayback {
            port,
            stream,
            converter,
            period: period_bytes,
            ring,
            running: false,
            without_eld: None,
            target: (codec, pin, device),
            owner,
            format,
            periods,
            buffer_address,
            resets: HdmiResets::default(),
        })
    }

    /// Recover a playback whose controller needs a reset (`NeedsReset`:
    /// a DMA descriptor error, a stream stop that was not acknowledged, a
    /// route given up after [`hda::hdmi::HDMI_TRANSIENT_FAULTS`] faults, a
    /// failed preparation): the controller's own reset
    /// (`hda::Controller::reinitialize`, GCTL.CRST: every stream and the
    /// command rings stopped, the link reset, the old route and port
    /// retired), the codecs discovered again, the port attached again on
    /// the new graph with the same owner, pin and device entry, and a new
    /// stream allocated, on the retired stream's ring and BDL. The route is
    /// not prepared: [`Self::start_hdmi`] prepares it.
    ///
    /// This is never a function-level reset (FLR) or a D3 transition of
    /// the PCI function: those could clear what the GPU's firmware wrote
    /// into the codec (the ELD, presence, ELD valid). CRST does not; the
    /// claim already took the link through it before the ELD was read.
    ///
    /// At most [`HDMI_RESETS`] per playback and boot, at least
    /// [`HDMI_RESET_SPACING_US`] apart by `now_us`; a refused reset changes
    /// nothing, and a failed one counts.
    pub fn reset_hdmi(
        &mut self,
        playback: &mut HdmiPlayback,
        now_us: u64,
    ) -> Result<HdmiReset, HdmiResetError> {
        let reset = playback
            .resets
            .admit(now_us)
            .map_err(HdmiResetError::Refused)?;
        playback.running = false;
        let failed = |error| HdmiResetError::Failed { reset, error };
        let codecs = {
            let (controller, _) = self.owner.controller().map_err(failed)?;
            controller
                .reinitialize()
                .map_err(|error| failed(error.into()))?;
            let discovery = controller
                .discover_codecs()
                .map_err(|failure| failed(failure.error.into()))?;
            let mut codecs = Vec::new();
            for graph in discovery.codecs {
                let setup = controller.initialize_codec(&graph);
                codecs.push(Codec { graph, setup });
            }
            self.codec_failures = discovery.failures;
            codecs
        };
        self.codecs = codecs;
        self.discovery_error = None;
        let previous = playback.buffer_address;
        // SAFETY: the owner was given for this controller's function, codec,
        // pin and device entry for the boot (`Self::hdmi_playback`); the
        // reset retired its old port after the route closed.
        let fresh = unsafe {
            self.hdmi_playback(
                playback.target,
                playback.owner.clone(),
                playback.format,
                playback.period,
                playback.periods,
            )
        }
        .map_err(failed)?;
        let resets = playback.resets;
        *playback = fresh;
        playback.resets = resets;
        Ok(HdmiReset {
            reset,
            codecs: self.codecs.len(),
            same_buffers: playback.buffer_address == previous,
        })
    }

    /// The next status read of the playback's stream reads its whole route
    /// back (`hda::Controller::schedule_hdmi_readback`): once per service
    /// turn, before the turn's refills; the others sense the pin only.
    pub fn schedule_hdmi_readback(
        &mut self,
        playback: &HdmiPlayback,
    ) -> Result<(), RetirementError> {
        let (controller, _) = self.owner.controller()?;
        Ok(controller.schedule_hdmi_readback(playback.port)?)
    }

    /// Start the playback. A route still prepared from an earlier start
    /// (its stream stopped by an underrun, a transient codec fault or the
    /// sink going away) restarts its stream only
    /// (`hda::Controller::restart_hdmi`): the same converter, stream tag
    /// and format, with no codec walk. Otherwise, or when the sink came
    /// back with another ELD, the route is prepared
    /// (`hda::Controller::prepare_hdmi`, which walks the codec and reads
    /// the sink's ELD again), the whole ring filled with silence and its
    /// DMA started.
    ///
    /// A sink away (no presence or no ELD_VALID) is `SinkUnavailable`: a
    /// prepared route stays paused for the next start. A sink whose ELD
    /// does not read or parse takes basic audio only
    /// ([`HdmiPlayback::without_eld`]). `NeedsReset` asks for
    /// [`Self::reset_hdmi`].
    pub fn start_hdmi(
        &mut self,
        playback: &mut HdmiPlayback,
    ) -> Result<HdmiStart, RetirementError> {
        let (controller, _) = self.owner.controller()?;
        playback.running = false;
        if controller.hdmi_prepared(playback.port)? {
            match controller.restart_hdmi(playback.port) {
                Ok(restart) => {
                    playback.running = true;
                    return Ok(match restart {
                        hda::hdmi::HdmiRestart::Stream => HdmiStart::Stream,
                        hda::hdmi::HdmiRestart::Reprogrammed => HdmiStart::Reprogrammed,
                        hda::hdmi::HdmiRestart::Resumed => HdmiStart::Resumed,
                    });
                }
                // Another ELD, or a route the codec no longer holds after
                // the sink came back: closed, so prepared again below.
                Err(audio::Error::SinkChanged) if !controller.hdmi_prepared(playback.port)? => {}
                Err(error) => return Err(error.into()),
            }
        }
        controller.prepare_hdmi(playback.port, playback.stream, playback.converter, None)?;
        playback.without_eld = controller
            .hdmi_sink(playback.port)?
            .and_then(|sink| sink.eld_error);
        let silence = alloc::vec![0u8; playback.ring];
        controller.write_pcm(playback.stream, 0, &silence)?;
        controller.start(playback.stream)?;
        playback.running = true;
        Ok(HdmiStart::Prepared)
    }

    /// Whether the playback's route is paused: prepared, its stream
    /// stopped because the pin sensed no sink with a valid ELD.
    pub fn hdmi_paused(&mut self, playback: &HdmiPlayback) -> Result<bool, RetirementError> {
        let (controller, _) = self.owner.controller()?;
        Ok(controller.hdmi_paused(playback.port)?)
    }

    /// Refill the next completed period with `pcm`, exactly one period:
    /// whether one was due. An error stopped the stream: an underrun or a
    /// transient codec fault stops only the stream, a sink away pauses the
    /// route, anything else closes it; [`Self::start_hdmi`] starts it
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

    /// The playback's route as the codec answers now, with its stream
    /// descriptor's registers (`hda::Controller::hdmi_route_state`): for
    /// the log only; nothing is checked or programmed.
    pub fn hdmi_route_state(
        &mut self,
        playback: &HdmiPlayback,
    ) -> Result<hda::hdmi::HdmiRouteState, RetirementError> {
        let (controller, _) = self.owner.controller()?;
        Ok(controller.hdmi_route_state(playback.port)?)
    }

    /// The codec answer behind the last `RouteRejected`, once
    /// (`hda::Controller::take_rejection`): for the log only.
    pub fn take_hdmi_rejection(&mut self) -> Option<hda::hdmi::VerbRejection> {
        let (controller, _) = self.owner.controller().ok()?;
        controller.take_rejection()
    }

    /// The last HDMI readback or pin sense a second read contradicted,
    /// once (`hda::Controller::take_misread`): for the log only.
    pub fn take_hdmi_misread(&mut self) -> Option<hda::hdmi::Misread> {
        let (controller, _) = self.owner.controller().ok()?;
        controller.take_misread()
    }

    /// What the controller absorbed without failing a verb or the route
    /// (`hda::Controller::stats`): RIRB entries waited for, spurious and
    /// unsolicited responses, misreads, FIFO errors acknowledged.
    pub fn hdmi_stats(&mut self) -> Option<hda::Stats> {
        let (controller, _) = self.owner.controller().ok()?;
        Some(controller.stats())
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
mod reset_tests {
    use super::*;

    const SECOND: u64 = 1_000_000;

    #[test]
    fn three_controller_resets_a_boot_at_least_ten_seconds_apart() {
        let mut resets = HdmiResets::default();
        // The first reset may come at once.
        assert_eq!(resets.admit(5 * SECOND), Ok(1));
        // The next only 10 s after it; a refusal changes nothing.
        assert_eq!(
            resets.admit(6 * SECOND),
            Err(HdmiResetRefusal::TooSoon {
                wait_us: 9 * SECOND
            })
        );
        assert_eq!(
            resets.admit(15 * SECOND - 1),
            Err(HdmiResetRefusal::TooSoon { wait_us: 1 })
        );
        assert_eq!(resets.taken(), 1);
        assert_eq!(resets.admit(15 * SECOND), Ok(2));
        // A clock that went back waits the whole spacing.
        assert_eq!(
            resets.admit(SECOND),
            Err(HdmiResetRefusal::TooSoon {
                wait_us: HDMI_RESET_SPACING_US
            })
        );
        assert_eq!(resets.admit(40 * SECOND), Ok(3));
        // After the third, never again, however long after.
        for later in [41, 50, 1_000_000] {
            assert_eq!(
                resets.admit(later * SECOND),
                Err(HdmiResetRefusal::Exhausted)
            );
        }
        assert_eq!(resets.taken(), HDMI_RESETS);
        assert_eq!((HDMI_RESETS, HDMI_RESET_SPACING_US), (3, 10 * SECOND));
    }
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
