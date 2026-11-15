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

/// What vouches for the original users of the function an attach takes.
enum Users {
    /// Nothing: the conventional controller is retained and refused.
    Unavailable,
    /// The function's GPU reset its bus in this boot ([`OriginalUsers::after_bus_reset`]).
    AfterBusReset,
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

/// The HD Audio function of a GPU whose cold start reset their bus: the
/// HDMI/DP audio controller probe left parked for that reset
/// (`probe::display_audio`), claimed once the GPU started. Its original
/// users are those the reset retired, so the controller binds a private
/// domain and its codecs are discovered, all polled.
///
/// A function that is not an HD Audio controller is refused, and every
/// failure keeps the original owners in the returned graph, which the
/// caller keeps for the boot.
///
/// # Safety
/// As for probe's HD Audio attach, and: `address` is a function of the
/// device whose function 0 is a display controller that, in this boot,
/// completed its cold start with a Secondary Bus Reset of its own bus
/// (`gpu::cold::Reset::SecondaryBus`). Until that reset the function stayed
/// parked in the contained fabric, and nothing claimed or enabled it since.
/// No later reset of that bus runs while the returned host is live.
#[expect(
    clippy::result_large_err,
    reason = "A failed HDA attach keeps its typed cause and all original resource owners"
)]
pub unsafe fn attach_display_audio(
    services: &dyn NativeServices,
    address: Address,
    clock: Clock,
    minimum: alloc::rc::Rc<dyn crate::bus::pci::power::Timer>,
) -> Result<Host, (Error, Option<Box<dyn core::any::Any>>)> {
    // SAFETY: forwarded, with the bus reset that retired the original users.
    unsafe { attach_with(services, address, clock, minimum, Users::AfterBusReset) }
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
    users: Users,
) -> Result<Host, Failure> {
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
    let original_users = match users {
        Users::Unavailable => None,
        Users::AfterBusReset => {
            let function = cold.requester.as_mut().unwrap().function_mut();
            if function.class[..2] != [0x04, 0x03] {
                return Err((
                    Error::Native(ServiceError::Refused("not an HD Audio controller")),
                    Some(Box::new(cold)),
                ));
            }
            match function.config().retain_epoch() {
                // SAFETY: this function's own epoch; the caller vouches for
                // the bus reset that retired its original users.
                Ok(retained) => Some(unsafe { OriginalUsers::after_bus_reset(retained) }),
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
    let registers = Bar0 {
        // SAFETY: exactly this original owned UC BAR0; Native outer owner
        // keeps actual mapping across constructor error and callback unwind.
        mmio: unsafe { Mmio::new(window.as_ptr() as u64, bytes) },
        bytes,
        mapping: cold.mapping.take().unwrap(),
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
        location: alloc::format!("{address}"),
        owner,
        codecs: discovery.codecs,
        codec_failures: discovery.codec_failures,
        discovery_error: discovery.discovery_error,
    })
}
