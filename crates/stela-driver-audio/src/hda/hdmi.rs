// SPDX-License-Identifier: GPL-2.0-only
//! Copyright(c) 2008-2010 Intel Corporation
//! Copyright (c) 2006 ATI Technologies Inc.
//! Copyright (c) 2008 NVIDIA Corp. All rights reserved.
//! Copyright (c) 2008 Wei Ni <wni@nvidia.com>
//! Copyright (c) 2013 Anssi Hannula <anssi.hannula@iki.fi>
//! Author: Wu Fengguang <wfg@linux.intel.com>
//!
//! Owned HDMI/DP codec routing, adapted from pinned Linux HDMI/nvhdmi/ELD
//! sources: the nvhdmi MODEL_LEGACY and MODEL_GENERIC profiles, not a vendor
//! wildcard. MODEL_GENERIC codecs are DP-MST capable: each pin device entry is
//! its own port. Display programming stays with the GPU port owner.
use super::{CodecGraph, Controller, ControllerToken, Registers, StreamId, Verb, WidgetKind};
use crate::Timer;
use crate::{Direction, Error, PcmFormat, Phase, bus::pci::Address};
use alloc::{sync::Arc, vec::Vec};
use core::mem::ManuallyDrop;
mod chmap;
mod eld;
mod playback;
pub use chmap::{ChannelMap, Speaker};
pub use eld::{AudioDescriptor, Connection, Eld, EldParseFault, EldReadFault};
pub use playback::RefilledPeriod;

const LEGACY_IDS: &[u32] = &[
    0x10de0008, 0x10de0009, 0x10de000a, 0x10de000b, 0x10de000c, 0x10de000d, 0x10de0010, 0x10de0011,
    0x10de0012, 0x10de0013, 0x10de0014, 0x10de0015, 0x10de0016, 0x10de0018, 0x10de0019, 0x10de001a,
    0x10de001b, 0x10de001c,
];
/// nvhdmi MODEL_GENERIC: probe_generic sets `dp_mst`.
const GENERIC_IDS: &[u32] = &[
    0x10de0040, 0x10de0041, 0x10de0042, 0x10de0043, 0x10de0044, 0x10de0045, 0x10de0050, 0x10de0051,
    0x10de0052, 0x10de0060, 0x10de0061, 0x10de0062, 0x10de0070, 0x10de0071, 0x10de0072, 0x10de0073,
    0x10de0074, 0x10de0076, 0x10de007b, 0x10de007c, 0x10de007d, 0x10de007e, 0x10de0080, 0x10de0081,
    0x10de0082, 0x10de0083, 0x10de0084, 0x10de0090, 0x10de0091, 0x10de0092, 0x10de0093, 0x10de0094,
    0x10de0095, 0x10de0097, 0x10de0098, 0x10de0099, 0x10de009a, 0x10de009b, 0x10de009c, 0x10de009d,
    0x10de009e, 0x10de009f, 0x10de00a0, 0x10de00a1, 0x10de00a3, 0x10de00a4, 0x10de00a5, 0x10de00a6,
    0x10de00a7, 0x10de00a8, 0x10de00a9, 0x10de00aa, 0x10de00ab, 0x10de00ad, 0x10de00ae, 0x10de00af,
    0x10de00b0, 0x10de00b1, 0x10de00c0, 0x10de00c1, 0x10de00c3, 0x10de00c4, 0x10de00c5,
];
const PIN_SENSE: u16 = 0xf09;
const SET_PIN_SENSE: u16 = 0x709;
const SET_DEVICE_SELECT: u16 = 0x735;
const PAR_DEVICE_LIST_LENGTH: u8 = 0x15;
const PIN_TRIGGER_REQUIRED: u32 = 1 << 1;
const PRESENT: u32 = 1 << 31;
const ELD_VALID: u32 = 1 << 30;
const DIGITAL: u32 = 1 << 9;
const PIN_HDMI: u32 = 1 << 7;
const PIN_DP: u32 = 1 << 24;
/// Linux hdmi_repoll_eld: the reads of an ELD that does not read or parse
/// on a pin showing a sink with a valid ELD, and their spacing.
pub const ELD_READS: u8 = 6;
pub const ELD_REPOLL_US: u64 = 300_000;
/// What a sink present with ELD_VALID set but without an ELD that reads
/// and parses takes: basic audio, 2-channel 48 kHz 16-bit LPCM on front
/// left/right, which every HDMI audio sink must accept. Linux likewise
/// opens a PCM on a present sink without a valid ELD, on the codec's caps.
pub const BASIC_AUDIO: PcmFormat = PcmFormat {
    rate: 48_000,
    bits: 16,
    channels: 2,
};
/// CEA-861 speaker allocation bit 0: FL/FR, basic audio's speakers.
const BASIC_SPEAKERS: u8 = 1;
/// A pin sense showing a sink present with a valid ELD.
fn eld_ready(sense: u32) -> bool {
    sense != u32::MAX && sense & (PRESENT | ELD_VALID) == PRESENT | ELD_VALID
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayAudioIdentity {
    pub hda_function: Address,
    pub gpu_function: Address,
    pub codec: u8,
    pub pin: u8,
    /// The pin's DP-MST device entry; 0 for a pin without one.
    pub device: u8,
    /// GPU connector identity, not a guessed pin-to-port arithmetic map.
    pub port: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayAudioState {
    /// Non-recycled port/sink generation, changed for disconnect, ELD rewrite,
    /// modeset or loss/recovery of the original power/clock/ownership epoch.
    pub generation: u64,
    pub connected: bool,
}
/// Coordinate an HDA function with its GPU display-audio route.
///
/// # Safety
/// Keep the matching HDA function, GPU connector/sink and device entry,
/// exclusive audio route, powered HDA registers/link and firmware exclusion
/// throughout attachment, errors, reset and controller Drop. Register access
/// and bounded stream/codec stop must remain valid after an observe failure.
/// A disconnected or unavailable sink immediately prevents new playback.
///
/// `observe` queries the live native service, returning an error when it is
/// unavailable, and reports every sink, ELD, modeset and ownership-epoch change.
/// `enable_audio` acknowledges rate, clock and link configuration;
/// `disable_audio` quiesces that configuration after hotplug as well as normal
/// playback. Calls use the controller Timer's absolute microsecond deadline
/// and return errors on access or acknowledgement uncertainty. Dropping the Arc
/// alone must not revoke the HDA access grant.
///
/// The platform must supply the requester, IOMMU, MMIO, DMA and power grants;
/// numeric BDF/port identities do not supply those resources. The constructor
/// also requires unique, coherent, identity-reachable RAM kept for the boot.
pub unsafe trait DisplayAudioPort {
    fn identity(&self) -> DisplayAudioIdentity;
    fn observe(&self, deadline_us: u64) -> Result<DisplayAudioState, Error>;
    fn enable_audio(&self, format: PcmFormat, deadline_us: u64) -> Result<(), Error>;
    fn disable_audio(&self, deadline_us: u64) -> Result<(), Error>;
}
/// One device entry of an HDMI/DP pin as the codec reports it before any
/// route: what Linux hdmi_present_sense reads, for the owner's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HdmiEntry {
    pub pin: u8,
    pub device: u8,
    /// The pin's device entries: one unless the codec is DP-MST capable.
    pub devices: u8,
    pub pin_default: u32,
    pub pin_capabilities: u32,
    /// The entry's pin sense: presence detect in bit 31, ELD valid in 30.
    pub sense: u32,
    /// With presence and a valid ELD: the ELD's reads, their bytes and
    /// parse, or the check that stopped them.
    pub eld: Option<EldRead>,
}
/// The survey's reads of one entry's ELD (Linux hdmi_present_sense with
/// hdmi_repoll_eld): the last read, and the first when it failed and the
/// ELD was read again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EldRead {
    /// The last read's bytes: the whole ELD (header and baseline, as
    /// long as its byte 2 says), or those read before the read stopped.
    pub raw: Vec<u8>,
    /// The ELD buffer size the codec answered on the last read, when it
    /// was in range; the ELD itself may be shorter.
    pub buffer: Option<u16>,
    /// Their parse, or what stopped the read (`Error::EldRead`, or a
    /// command transport error) or the parse (`Error::EldParse`).
    pub eld: Result<Eld, Error>,
    /// The reads made, 1..=[`ELD_READS`].
    pub reads: u8,
    /// With more than one read: what stopped the first, and its bytes.
    pub first_failure: Option<(Error, Vec<u8>)>,
}
impl EldRead {
    /// Whether the bytes were all read, but did not parse.
    pub fn malformed(&self) -> bool {
        matches!(self.eld, Err(Error::EldParse(_)))
    }
}
/// How a surveyed entry takes a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkAudio {
    /// Its parsed ELD supports the format (`Eld::supports`).
    Eld,
    /// Present with ELD_VALID set, but its ELD did not read or parse, for
    /// this reason: [`BASIC_AUDIO`] only.
    Basic(Error),
}
impl HdmiEntry {
    pub fn present(&self) -> bool {
        self.sense != u32::MAX && self.sense & PRESENT != 0
    }
    pub fn eld_valid(&self) -> bool {
        self.sense != u32::MAX && self.sense & ELD_VALID != 0
    }
    /// Whether, and how, this entry takes `format`: a sink present with a
    /// valid ELD that supports it, or, whose ELD stayed unreadable or
    /// malformed, basic audio. `prepare_hdmi` applies the same rule.
    pub fn takes(&self, format: PcmFormat) -> Option<SinkAudio> {
        if !self.present() || !self.eld_valid() {
            return None;
        }
        match &self.eld.as_ref()?.eld {
            Ok(eld) => eld.supports(format).then_some(SinkAudio::Eld),
            Err(error @ (Error::EldRead(_) | Error::EldParse(_))) => {
                (format == BASIC_AUDIO).then_some(SinkAudio::Basic(*error))
            }
            Err(_) => None,
        }
    }
}
/// The playback target among surveyed `entries` (by codec address) for
/// `format`: the first whose ELD supports it, else the first present with
/// ELD_VALID set whose ELD stayed unreadable or malformed, as basic audio.
pub fn choose_hdmi_target<'a>(
    entries: impl IntoIterator<Item = (u8, &'a HdmiEntry)>,
    format: PcmFormat,
) -> Option<(u8, &'a HdmiEntry, SinkAudio)> {
    let mut basic = None;
    for (codec, entry) in entries {
        match entry.takes(format) {
            Some(SinkAudio::Eld) => return Some((codec, entry, SinkAudio::Eld)),
            Some(audio) => {
                basic.get_or_insert((codec, entry, audio));
            }
            None => {}
        }
    }
    basic
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdmiPortId {
    token: ControllerToken,
    index: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HdmiSink {
    pub state: DisplayAudioState,
    pub eld: Option<Eld>,
    /// A sink present with ELD_VALID set whose ELD did not read or parse:
    /// why. It takes [`BASIC_AUDIO`] only.
    pub eld_error: Option<Error>,
}
struct Route {
    stream: StreamId,
    converter: u8,
    mux: u8,
    format: PcmFormat,
    map: ChannelMap,
    digital: u8,
    /// The infoframe's connection type, for [`Controller::hdmi_program`].
    connection: Connection,
    generation: u64,
    prepared: bool,
    progress: Option<playback::Progress>,
    /// The next status read reads the whole route back
    /// ([`Controller::schedule_hdmi_readback`]); a start always does, and
    /// other status reads read only the pin sense.
    verify: bool,
    /// A readback rejected the route: the next restart sets the route's
    /// verbs again ([`Controller::hdmi_program`]) before its readback.
    reprogram: bool,
    /// The route's verbs were just set again: a rejected readback now
    /// counts toward [`HDMI_TRANSIENT_FAULTS`].
    reprogrammed: bool,
    /// The stream stopped because the pin sensed no sink with a valid ELD
    /// (a monitor asleep or unplugged); the route stays prepared, and
    /// [`Controller::restart_hdmi`] resumes it once the sink is back with
    /// the ELD it was prepared for.
    paused: bool,
    /// Transient codec faults in a row, each of which stopped only the
    /// stream; a refilled period clears them. Past
    /// [`HDMI_TRANSIENT_FAULTS`] the route is given up (`NeedsReset`). A
    /// rejected readback counts only once the route's verbs were set again
    /// and it still reads back wrong.
    faults: u8,
}
/// How a route's runtime check failed, and so what it stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeFault {
    /// The pin answered its sense without presence and ELD valid: the
    /// stream stops and the route pauses, prepared.
    Absent,
    /// A codec answer the route did not expect (a readback that does not
    /// match, an unanswered pin sense, a verb timeout): the stream stops,
    /// the route stays prepared, and the fault counts.
    Transient,
    /// The display owner reports another sink, or anything else: the
    /// route closes, as before.
    Lost,
}
impl RuntimeFault {
    fn of(error: Error) -> Self {
        match error {
            Error::RouteRejected | Error::InvalidEld | Error::Timeout(Phase::Verb) => {
                Self::Transient
            }
            _ => Self::Lost,
        }
    }
}
/// What a codec answer that rejected a route was checked against
/// ([`VerbRejection`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    /// The answer, exactly.
    Value(u32),
    /// The answer's bits under `mask`.
    Masked { mask: u32, value: u32 },
    /// An answer within `low..=high`.
    Within { low: u32, high: u32 },
    /// Any answer but all ones.
    Answered,
}
impl Expected {
    /// Whether `read` is what was expected.
    pub fn accepts(self, read: u32) -> bool {
        match self {
            Self::Value(value) => read == value,
            Self::Masked { mask, value } => read != u32::MAX && read & mask == value,
            Self::Within { low, high } => (low..=high).contains(&read),
            Self::Answered => read != u32::MAX,
        }
    }
}
/// An HDMI runtime readback that answered wrong, then, read again at
/// once, as set ([`Controller::take_misread`]): a misread of the command
/// transport, which neither stopped the stream nor rejected the route.
/// For a pin sense (0xf09), `first` lacked presence or ELD valid and the
/// second sense had both: the route did not pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Misread {
    /// The first answer, against what the route expected.
    pub first: VerbRejection,
    /// The second answer, which the route accepted.
    pub again: u32,
}
impl core::fmt::Display for Misread {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}; read again: {:#010x}", self.first, self.again)
    }
}
impl core::fmt::Display for Expected {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Value(value) => write!(f, "{value:#010x}"),
            Self::Masked { mask, value } => write!(f, "{value:#x} under mask {mask:#x}"),
            Self::Within { low, high } => write!(f, "{low}..={high}"),
            Self::Answered => f.write_str("an answer (not all ones)"),
        }
    }
}
/// The codec answer behind the last `Error::RouteRejected`, for the
/// owner's log ([`Controller::take_rejection`]): which readback, what the
/// route set or expected, and what the codec answered. Diagnostics only:
/// recording it changes nothing the controller does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerbRejection {
    pub codec: u8,
    pub node: u8,
    /// The verb that answered: a 12-bit get verb (0xfxx), or a long verb
    /// (0xa the converter format, 0xb an amplifier).
    pub verb: u16,
    /// Its payload; for a DIP data read (0xf31), the byte's index in the
    /// packet.
    pub payload: u16,
    pub expected: Expected,
    pub read: u32,
    /// Whether a verb timeout had already moved the command transport to
    /// the immediate command interface.
    pub single_command: bool,
}
impl core::fmt::Display for VerbRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "codec {} node {:#04x} verb {:#05x} payload {:#06x}: answered {:#010x}, expected {} \
             (commands over {})",
            self.codec,
            self.node,
            self.verb,
            self.payload,
            self.read,
            self.expected,
            if self.single_command {
                "the immediate command interface"
            } else {
                "CORB/RIRB"
            }
        )
    }
}
/// A route's converter and pin as the codec answered them, with the
/// stream descriptor's registers ([`Controller::hdmi_route_state`]), for
/// the owner's log. `set_*` are what the route programmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HdmiRouteState {
    pub codec: u8,
    pub group: u8,
    pub converter: u8,
    pub pin: u8,
    pub device: u8,
    /// GET_POWER (0xf05) of the function group, converter and pin.
    pub group_power: u32,
    pub converter_power: u32,
    pub pin_power: u32,
    /// GET_DIGI_CONVERT (0xf0d): DIG1 in bits 7:0, the category code (DIG2)
    /// in 14:8, DIG3 in 23:16.
    pub digital: u32,
    pub set_digital: u8,
    /// GET_CVT_CHAN_COUNT (0xf2d): channels - 1.
    pub channels: u32,
    pub set_channels: u8,
    /// GET_STREAM_FORMAT (long 0xa).
    pub format: u32,
    pub set_format: u16,
    /// GET_CONV (0xf06): stream tag in 7:4, channel in 3:0.
    pub stream: u32,
    pub set_stream: u8,
    /// Output amplifier left and right (long 0xb, 0x8000 and 0xa000), for
    /// a widget that has one.
    pub converter_amp: Option<[u32; 2]>,
    pub pin_amp: Option<[u32; 2]>,
    /// GET_CONNECT_SEL (0xf01) and the mux the route set.
    pub connection: u32,
    pub set_connection: u8,
    /// GET_PIN_WIDGET_CONTROL (0xf07): 0x40 is out enable.
    pub pin_control: u32,
    /// The device entry's pin sense (0xf09): PD bit 31, ELDV bit 30.
    pub sense: u32,
    /// GET_DEVICE_SELECT (0xf35), for a pin with several device entries.
    pub device_select: Option<u32>,
    /// GET_HDMI_CHAN_SLOT (0xf34) of ASP slots 0..8: channel in 7:4.
    pub slots: [u32; 8],
    /// GET_HDMI_DIP_SIZE (0xf2e) of packet 0, GET_HDMI_DIP_XMIT (0xf32),
    /// and packet 0's bytes (0xf31), up to 32.
    pub dip_size: u32,
    pub dip_transmit: u32,
    pub dip: Vec<u8>,
    /// The stream descriptor: SDnCTL (24 bits), SDnSTS, SDnLPIB, SDnFIFOS,
    /// SDnFMT.
    pub sd_control: u32,
    pub sd_status: u8,
    pub sd_position: u32,
    pub sd_fifo: u16,
    pub sd_format: u16,
    /// Whether a verb timeout had moved the command transport to the
    /// immediate command interface.
    pub single_command: bool,
}
impl HdmiRouteState {
    /// The DIP packet's byte sum: 0 for an infoframe whose checksum holds.
    pub fn dip_sum(&self) -> u8 {
        self.dip.iter().fold(0u8, |s, b| s.wrapping_add(*b))
    }
}
impl core::fmt::Display for HdmiRouteState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let amp = |f: &mut core::fmt::Formatter<'_>, amp: Option<[u32; 2]>| match amp {
            Some([left, right]) => write!(f, "amp L {left:#04x} R {right:#04x}"),
            None => f.write_str("no amp"),
        };
        let d = self.digital;
        write!(
            f,
            "group {:#04x} power {:#x}; converter {:#04x} power {:#x} digital {d:#08x} \
             (DigEn={} V={} VCFG={} PRE={} Copy={} NonAudio={} Pro={} L={} CC={:#04x} DIG3={:#04x}; \
             set {:#04x}) channels-1 {:#x} (set {:#x}) format {:#06x} (set {:#06x}) stream \
             {:#04x} (set {:#04x}) ",
            self.group,
            self.group_power,
            self.converter,
            self.converter_power,
            d & 1,
            (d >> 1) & 1,
            (d >> 2) & 1,
            (d >> 3) & 1,
            (d >> 4) & 1,
            (d >> 5) & 1,
            (d >> 6) & 1,
            (d >> 7) & 1,
            (d >> 8) & 0x7f,
            (d >> 16) & 0xff,
            self.set_digital,
            self.channels,
            self.set_channels,
            self.format,
            self.set_format,
            self.stream,
            self.set_stream,
        )?;
        amp(f, self.converter_amp)?;
        write!(
            f,
            "; pin {:#04x} device {} power {:#x} connection {:#x} (set {:#x}) control {:#04x} \
             sense {:#010x} select ",
            self.pin,
            self.device,
            self.pin_power,
            self.connection,
            self.set_connection,
            self.pin_control,
            self.sense,
        )?;
        match self.device_select {
            Some(select) => write!(f, "{select:#x} ")?,
            None => f.write_str("- ")?,
        }
        amp(f, self.pin_amp)?;
        f.write_str(" slots")?;
        for slot in self.slots {
            write!(f, " {slot:02x}")?;
        }
        write!(
            f,
            "; dip size {} transmit {:#04x} bytes",
            self.dip_size, self.dip_transmit
        )?;
        for byte in &self.dip {
            write!(f, " {byte:02x}")?;
        }
        write!(
            f,
            " (sum {:#04x}); sd ctl {:#08x} sts {:#04x} lpib {} fifos {} fmt {:#06x}; commands \
             over {}",
            self.dip_sum(),
            self.sd_control,
            self.sd_status,
            self.sd_position,
            self.sd_fifo,
            self.sd_format,
            if self.single_command {
                "the immediate command interface"
            } else {
                "CORB/RIRB"
            }
        )
    }
}
/// Transient codec faults in a row (see `RuntimeFault::Transient`), each
/// stopping only the stream for a restart, before the route is given up
/// and the controller needs a reset. A refilled period clears the count.
pub const HDMI_TRANSIENT_FAULTS: u8 = 8;
/// How [`Controller::restart_hdmi`] started a prepared route's stream
/// again: on the same converter, stream tag and format, after the stream
/// descriptor's reset; neither the codec graph nor a connection list is
/// read again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdmiRestart {
    /// The stream alone, after a stream-level stop (an underrun, a stalled
    /// DMA position, a transient codec fault). A DMA FIFO error alone no
    /// longer stops the stream.
    Stream,
    /// The route's verbs set again ([`Controller::hdmi_program`], no graph
    /// walk), then the stream, after a readback rejected the route.
    Reprogrammed,
    /// The stream, after a pause: the sink came back with the ELD the
    /// route was prepared for, and the route's verbs were set again, as
    /// Linux sets the infoframe again when ELD_VALID returns.
    Resumed,
}
pub(super) struct Port {
    attachment: HdmiPortId,
    // Intentional quarantine: no ordinary Drop can release the GPU/HDA owner.
    owner: ManuallyDrop<Arc<dyn DisplayAudioPort>>,
    graph: CodecGraph,
    pin: u8,
    device: u8,
    /// The pin's device entries, read once at attach: a status read selects
    /// the entry without reading the parameter again.
    devices: u8,
    /// The device entry's connection list, as Linux hdmi_read_pin_conn reads it.
    connections: Vec<u8>,
    sink: Option<HdmiSink>,
    route: Option<Route>,
    first_error: Option<Error>,
}
impl Port {
    fn owner(&self) -> &dyn DisplayAudioPort {
        self.owner.as_ref()
    }
}
impl<R: Registers, T: Timer> Controller<R, T> {
    fn hdmi_index(&self, id: HdmiPortId) -> Result<usize, Error> {
        self.minimum_guard()?;
        if id.token != self.token
            || self
                .hdmi_ports
                .get(id.index)
                .and_then(Option::as_ref)
                .is_none()
        {
            return Err(Error::InvalidHdmiPort);
        }
        Ok(id.index)
    }
    fn hdmi_deadline(&self) -> u64 {
        self.timer
            .now_us()
            .wrapping_add(u64::from(self.config.transition_timeout_us))
    }
    fn hdmi_late(&self, start: u64) -> Result<(), Error> {
        self.minimum_guard()?;
        if self.timer.now_us().wrapping_sub(start) >= u64::from(self.config.transition_timeout_us) {
            Err(Error::Timeout(Phase::DisplayAudio))
        } else {
            Ok(())
        }
    }
    fn hdmi_observe(&self, index: usize) -> Result<DisplayAudioState, Error> {
        self.minimum_guard()?;
        let start = self.timer.now_us();
        let state = self.hdmi_ports[index]
            .as_ref()
            .unwrap()
            .owner()
            .observe(self.hdmi_deadline())?;
        self.hdmi_late(start)?;
        Ok(state)
    }
    fn hdmi_effect(&mut self, index: usize, format: Option<PcmFormat>) -> Result<(), Error> {
        self.minimum_guard()?;
        let start = self.timer.now_us();
        let owner = self.hdmi_ports[index].as_ref().unwrap().owner();
        let result = if let Some(format) = format {
            owner.enable_audio(format, self.hdmi_deadline())
        } else {
            owner.disable_audio(self.hdmi_deadline())
        };
        result.and_then(|()| self.hdmi_late(start))
    }
    fn hdmi_topology(&mut self, graph: &CodecGraph, pin: u8) -> Result<(), Error> {
        self.minimum_guard()?;
        if graph.token != self.token {
            return Err(Error::InvalidCodecGraph);
        }
        if !LEGACY_IDS.contains(&graph.vendor_id) && !GENERIC_IDS.contains(&graph.vendor_id) {
            return Err(Error::UnsupportedTopology);
        }
        let group = graph
            .groups
            .iter()
            .find(|g| g.function_type & 255 == 1 && g.widgets.iter().any(|w| w.node == pin))
            .ok_or(Error::InvalidCodecGraph)?;
        let widget = graph.widget(pin).ok_or(Error::InvalidCodecGraph)?;
        if group
            .power_states
            .is_none_or(|p| p == u32::MAX || p & 1 == 0)
            || widget
                .power_states
                .is_some_and(|p| p == u32::MAX || p & 1 == 0)
            || widget.kind != WidgetKind::Pin
            || widget.capabilities & DIGITAL == 0
            || widget
                .pin_capabilities
                .is_none_or(|p| p == u32::MAX || p & (PIN_HDMI | PIN_DP) == 0 || p & 0x10 == 0)
            || widget
                .pin_default
                .is_none_or(|p| p == u32::MAX || p >> 30 == 1)
            || widget.connections.is_empty()
            || widget.connections.len() > 32
        {
            return Err(Error::UnsupportedTopology);
        }
        // Fresh enumeration includes inherited PCM, connection expansion,
        // pin capabilities and defaults. A mutable public graph is no permission.
        let actual = self.enumerate_codec(graph.address)?;
        if actual.vendor_id != graph.vendor_id || actual.revision_id != graph.revision_id {
            return Err(Error::InvalidCodecGraph);
        }
        let live = actual
            .groups
            .iter()
            .find(|g| g.node == group.node)
            .ok_or(Error::InvalidCodecGraph)?;
        let pin_live = actual.widget(pin).ok_or(Error::InvalidCodecGraph)?;
        if live.function_type != group.function_type
            || live.power_states != group.power_states
            || pin_live.power_states != widget.power_states
            || pin_live.output_amplifier != widget.output_amplifier
            || pin_live.kind != widget.kind
            || pin_live.capabilities != widget.capabilities
            || pin_live.pin_capabilities != widget.pin_capabilities
            || pin_live.pin_default != widget.pin_default
            || pin_live.connections != widget.connections
        {
            return Err(Error::InvalidCodecGraph);
        }
        Ok(())
    }
    /// Linux snd_hda_get_num_devices + 1: the pin's device entries, one
    /// unless the codec is DP-MST capable. An unanswered parameter is 0.
    fn hdmi_devices(&mut self, codec: u8, vendor: u32, pin: u8) -> Result<u8, Error> {
        self.minimum_guard()?;
        if !GENERIC_IDS.contains(&vendor) {
            return Ok(1);
        }
        let length = match self.parameter(codec, pin, PAR_DEVICE_LIST_LENGTH)? {
            u32::MAX => 0,
            length => length & 0x3f,
        };
        Ok(length as u8 + 1)
    }
    /// Linux snd_hda_set_dev_select: address `device` on a multi-device pin.
    fn hdmi_select(&mut self, codec: u8, vendor: u32, pin: u8, device: u8) -> Result<(), Error> {
        self.minimum_guard()?;
        let devices = self.hdmi_devices(codec, vendor, pin)?;
        if devices == 1 {
            return Ok(());
        }
        if device >= devices {
            return Err(Error::InvalidHdmiPort);
        }
        self.verb(Verb::short(codec, pin, SET_DEVICE_SELECT, device)?)?;
        Ok(())
    }
    /// Select the port's device entry, on the device count read at attach.
    fn hdmi_select_port(&mut self, index: usize) -> Result<(), Error> {
        self.minimum_guard()?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let (codec, pin, device, devices) =
            (port.graph.address, port.pin, port.device, port.devices);
        if devices == 1 {
            return Ok(());
        }
        self.verb(Verb::short(codec, pin, SET_DEVICE_SELECT, device)?)?;
        Ok(())
    }
    /// Linux read_pin_sense: trigger when the pin requires it, then read the
    /// port's device entry.
    fn hdmi_pin_sense(&mut self, index: usize) -> Result<u32, Error> {
        self.minimum_guard()?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let (codec, pin, device) = (port.graph.address, port.pin, port.device);
        let capabilities = port
            .graph
            .widget(pin)
            .and_then(|w| w.pin_capabilities)
            .unwrap_or(0);
        self.pin_sense(codec, pin, device, capabilities)
    }
    fn pin_sense(
        &mut self,
        codec: u8,
        pin: u8,
        device: u8,
        capabilities: u32,
    ) -> Result<u32, Error> {
        self.minimum_guard()?;
        if capabilities & PIN_TRIGGER_REQUIRED != 0 {
            self.verb(Verb::short(codec, pin, SET_PIN_SENSE, 0)?)?;
        }
        self.verb(Verb::short(codec, pin, PIN_SENSE, device)?)
    }
    /// The selected device entry's ELD bytes through the codec's command
    /// transport, into `raw`, with the checks of Linux snd_hdmi_get_eld:
    /// on a failed check, the bytes read before it. `buffer` is set to the
    /// ELD buffer size the codec answered, once it is in range.
    ///
    /// The 4-byte header is read first, then only the ELD its BLL (byte 2)
    /// sizes, [`eld::eld_length`]: the codec's buffer may be larger, and a
    /// GPU codec need not mark the bytes past the ELD valid (NVIDIA's did
    /// not: byte 37 of a 36-byte ELD in a larger buffer).
    fn read_eld_bytes(
        &mut self,
        codec: u8,
        pin: u8,
        raw: &mut Vec<u8>,
        buffer: &mut Option<u16>,
    ) -> Result<(), Error> {
        self.minimum_guard()?;
        raw.clear();
        *buffer = None;
        let response = self.verb(Verb::short(codec, pin, 0xf2e, 8)?)?;
        // Same donor compatibility fallback: a zero length means 128,
        // never a missing service or invented monitor/ELD-valid bit.
        let size = if response == 0 {
            128
        } else {
            usize::try_from(response).unwrap_or(usize::MAX)
        };
        if !(20..=256).contains(&size) {
            return Err(Error::EldRead(EldReadFault::Size { response }));
        }
        *buffer = Some(size as u16);
        raw.reserve(size);
        let mut length = 4;
        let mut byte = 0;
        while byte < length {
            let index = byte as u8;
            let value = self.verb(Verb::short(codec, pin, 0xf2f, index)?)?;
            let fault = if value == u32::MAX {
                Some(EldReadFault::ByteUnanswered { index })
            } else if value & (1 << 31) == 0 {
                Some(EldReadFault::ByteNotValid {
                    index,
                    response: value,
                })
            } else if byte == 0 && value & 255 == 0 {
                Some(EldReadFault::FirstByteZero { response: value })
            } else {
                None
            };
            if let Some(fault) = fault {
                return Err(Error::EldRead(fault));
            }
            raw.push(value as u8);
            byte += 1;
            if byte == 4 {
                let baseline_dwords = raw[2];
                length = usize::from(eld::eld_length(baseline_dwords));
                if length > size {
                    return Err(Error::EldParse(EldParseFault::BaselineBeyondBuffer {
                        baseline_dwords,
                        buffer: size as u16,
                    }));
                }
            }
        }
        Ok(())
    }
    /// One ELD read and its parse: the bytes read, the codec's ELD buffer
    /// size when it answered one in range, and the ELD or what stopped the
    /// read or parse.
    fn read_eld(&mut self, codec: u8, pin: u8) -> (Vec<u8>, Option<u16>, Result<Eld, Error>) {
        let mut raw = Vec::new();
        let mut buffer = None;
        let eld = self
            .read_eld_bytes(codec, pin, &mut raw, &mut buffer)
            .and_then(|()| Eld::parse(&raw));
        (raw, buffer, eld)
    }
    /// Linux hdmi_present_sense with hdmi_repoll_eld, for the survey: read
    /// the selected entry's ELD and, while the read or its parse fails and
    /// the entry still senses a sink with a valid ELD, read it again
    /// [`ELD_REPOLL_US`] later, up to [`ELD_READS`] reads. `sense` is the
    /// entry's sense, left at its last value.
    fn survey_eld(
        &mut self,
        codec: u8,
        pin: u8,
        device: u8,
        capabilities: u32,
        sense: &mut u32,
    ) -> Result<EldRead, Error> {
        let mut first_failure = None;
        let mut reads = 0;
        loop {
            reads += 1;
            let (raw, buffer, eld) = self.read_eld(codec, pin);
            let done = |raw, eld, first_failure| EldRead {
                raw,
                buffer,
                eld,
                reads,
                first_failure,
            };
            let error = match eld {
                Err(error @ (Error::EldRead(_) | Error::EldParse(_))) if reads < ELD_READS => error,
                eld => return Ok(done(raw, eld, first_failure)),
            };
            // A minimum delay that cannot be kept ends the repoll, not the survey.
            if self
                .timer
                .check()
                .and_then(|()| self.timer.wait_us(ELD_REPOLL_US))
                .is_err()
            {
                return Ok(done(raw, Err(error), first_failure));
            }
            *sense = self.pin_sense(codec, pin, device, capabilities)?;
            if !eld_ready(*sense) {
                return Ok(done(raw, Err(error), first_failure));
            }
            first_failure.get_or_insert((error, raw));
        }
    }
    /// Every HDMI/DP pin of NVIDIA HDMI codec `graph` and each of its device
    /// entries: its pin sense and, with a sink present and a valid ELD, the
    /// ELD. Nothing but device selection changes, which it leaves on entry
    /// 0; refused while a stream runs, whose pin state that selection moves.
    /// No port is attached and no route is granted.
    pub fn survey_hdmi(&mut self, graph: &CodecGraph) -> Result<Vec<HdmiEntry>, Error> {
        self.minimum_guard()?;
        if graph.token != self.token {
            return Err(Error::InvalidCodecGraph);
        }
        if !LEGACY_IDS.contains(&graph.vendor_id) && !GENERIC_IDS.contains(&graph.vendor_id) {
            return Err(Error::UnsupportedTopology);
        }
        self.require_routing_stopped()?;
        let pins: Vec<(u8, u32, u32)> = graph
            .groups
            .iter()
            .filter(|g| g.function_type & 255 == 1)
            .flat_map(|g| &g.widgets)
            .filter(|w| w.kind == WidgetKind::Pin && w.capabilities & DIGITAL != 0)
            .filter_map(|w| {
                let capabilities = w
                    .pin_capabilities
                    .filter(|&p| p != u32::MAX && p & (PIN_HDMI | PIN_DP) != 0)?;
                Some((w.node, capabilities, w.pin_default.unwrap_or(0)))
            })
            .collect();
        let (codec, vendor) = (graph.address, graph.vendor_id);
        let mut entries = Vec::new();
        for (pin, pin_capabilities, pin_default) in pins {
            let devices = self.hdmi_devices(codec, vendor, pin)?;
            for device in 0..devices {
                self.hdmi_select(codec, vendor, pin, device)?;
                let mut sense = self.pin_sense(codec, pin, device, pin_capabilities)?;
                let eld = if eld_ready(sense) {
                    Some(self.survey_eld(codec, pin, device, pin_capabilities, &mut sense)?)
                } else {
                    None
                };
                entries.push(HdmiEntry {
                    pin,
                    device,
                    devices,
                    pin_default,
                    pin_capabilities,
                    sense,
                    eld,
                });
            }
            if devices > 1 {
                self.hdmi_select(codec, vendor, pin, 0)?;
            }
        }
        Ok(entries)
    }
    /// Attach a platform-owned GPU/HDA display-audio route. ELD is read through
    /// the codec command transport. Intel/AMD vendor profiles are refused.
    ///
    /// # Safety
    /// `owner` must keep and coordinate this controller/function, graph codec,
    /// physical pin and device entry. Its unsafe trait contract applies through
    /// failures, reset and controller Drop.
    pub unsafe fn attach_hdmi_port(
        &mut self,
        graph: &CodecGraph,
        pin: u8,
        device: u8,
        owner: Arc<dyn DisplayAudioPort>,
    ) -> Result<HdmiPortId, Error> {
        self.minimum_guard()?;
        if self.routing_failed {
            return Err(Error::NeedsReset);
        }
        self.require_routing_stopped()?;
        let identity = owner.identity();
        if identity.codec != graph.address
            || identity.pin != pin
            || identity.device != device
            || self
                .hdmi_ports
                .iter()
                .flatten()
                .any(|p| p.graph.address == graph.address && p.pin == pin && p.device == device)
        {
            return Err(Error::InvalidHdmiPort);
        }
        self.hdmi_topology(graph, pin)?;
        let devices = self.hdmi_devices(graph.address, graph.vendor_id, pin)?;
        if device >= devices {
            return Err(Error::InvalidHdmiPort);
        }
        self.hdmi_select(graph.address, graph.vendor_id, pin, device)?;
        let connections = self.connections(graph.address, pin)?;
        if connections.is_empty() || connections.len() > 32 {
            return Err(Error::UnsupportedTopology);
        }
        let start = self.timer.now_us();
        owner.observe(self.hdmi_deadline())?;
        self.hdmi_late(start)?;
        let id = HdmiPortId {
            token: self.token,
            index: self.hdmi_ports.len(),
        };
        self.hdmi_ports.push(Some(Port {
            attachment: id,
            owner: ManuallyDrop::new(owner),
            graph: graph.clone(),
            pin,
            device,
            devices,
            connections,
            sink: None,
            route: None,
            first_error: None,
        }));
        Ok(id)
    }
    /// Sticky original cause remains inspectable if a later full reset fails.
    /// Old handles still grant no route/control operations in the new epoch.
    pub fn hdmi_first_error(&self, id: HdmiPortId) -> Result<Option<Error>, Error> {
        self.minimum_guard()?;
        self.hdmi_ports
            .get(id.index)
            .and_then(Option::as_ref)
            .filter(|p| p.attachment == id && id.token.owner == self.token.owner)
            .map(|p| p.first_error)
            .ok_or(Error::InvalidHdmiPort)
    }
    pub(super) fn hdmi_note_descriptor_fault(&mut self, descriptor: u8, error: Error) {
        for port in self.hdmi_ports.iter_mut().flatten() {
            if port
                .route
                .as_ref()
                .is_some_and(|r| r.stream.descriptor_index() == descriptor)
            {
                port.first_error.get_or_insert(error);
            }
        }
    }
    fn hdmi_record(&mut self, index: usize, error: Error) -> Error {
        self.hdmi_ports[index]
            .as_mut()
            .unwrap()
            .first_error
            .get_or_insert(error);
        error
    }
    fn hdmi_read_eld(&mut self, index: usize, state: DisplayAudioState) -> Result<HdmiSink, Error> {
        self.minimum_guard()?;
        self.hdmi_select_port(index)?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let (codec, pin) = (port.graph.address, port.pin);
        let sense = self.hdmi_pin_sense(index)?;
        if sense == u32::MAX {
            return Err(Error::InvalidEld);
        }
        if !state.connected || sense & PRESENT == 0 || sense & ELD_VALID == 0 {
            return Ok(HdmiSink {
                state,
                eld: None,
                eld_error: None,
            });
        }
        // One read: the survey repolls; a sink whose ELD does not read or
        // parse keeps the reason and takes basic audio only. A command
        // transport failure is no such reason.
        let (_, _, eld) = self.read_eld(codec, pin);
        if let Err(error) = eld
            && !matches!(error, Error::EldRead(_) | Error::EldParse(_))
        {
            return Err(error);
        }
        let after = self.hdmi_pin_sense(index)?;
        if after != sense || self.hdmi_observe(index)? != state {
            return Err(Error::SinkChanged);
        }
        Ok(match eld {
            Ok(eld) => HdmiSink {
                state,
                eld: Some(eld),
                eld_error: None,
            },
            Err(error) => HdmiSink {
                state,
                eld: None,
                eld_error: Some(error),
            },
        })
    }
    /// The port's sink as the last [`Self::poll_hdmi_sink`] (or
    /// [`Self::prepare_hdmi`]) read it.
    pub fn hdmi_sink(&self, id: HdmiPortId) -> Result<Option<HdmiSink>, Error> {
        let index = self.hdmi_index(id)?;
        Ok(self.hdmi_ports[index].as_ref().unwrap().sink.clone())
    }
    /// Explicit polled hotplug/ELD service. A failed/changed snapshot first
    /// stops any PCM route; a new valid snapshot requires fresh prepare.
    pub fn poll_hdmi_sink(&mut self, id: HdmiPortId) -> Result<HdmiSink, Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        if self.routing_failed {
            return Err(Error::NeedsReset);
        }
        let observed = self
            .hdmi_observe(index)
            .and_then(|s| self.hdmi_read_eld(index, s));
        let changed = match &observed {
            Ok(s) => self.hdmi_ports[index].as_ref().unwrap().sink.as_ref() != Some(s),
            Err(_) => true,
        };
        if changed && self.hdmi_ports[index].as_ref().unwrap().route.is_some() {
            let cause = observed
                .as_ref()
                .err()
                .copied()
                .unwrap_or(Error::SinkChanged);
            self.hdmi_record(index, cause);
            if let Err(error) = self.hdmi_close_index(index) {
                self.routing_failed = true;
                self.hdmi_record(index, error);
                return Err(cause);
            }
        }
        match observed {
            Ok(sink) => {
                self.hdmi_ports[index].as_mut().unwrap().sink = Some(sink.clone());
                Ok(sink)
            }
            Err(error) => Err(self.hdmi_record(index, error)),
        }
    }
    fn hdmi_long_control(
        &mut self,
        codec: u8,
        node: u8,
        set: u8,
        get: u8,
        value: u16,
    ) -> Result<(), Error> {
        self.minimum_guard()?;
        self.verb(Verb::long(codec, node, set, value)?)?;
        let read = self.verb(Verb::long(codec, node, get, 0)?)?;
        if read != u32::from(value) {
            let expected = Expected::Value(u32::from(value));
            return Err(self.rejected(codec, node, u16::from(get), 0, expected, read));
        }
        Ok(())
    }
    fn hdmi_digital_control(&mut self, codec: u8, node: u8, value: u8) -> Result<(), Error> {
        self.minimum_guard()?;
        self.verb(Verb::short(codec, node, 0x70d, value)?)?;
        let observed = self.verb(Verb::short(codec, node, 0xf0d, 0)?)?;
        if observed == u32::MAX || observed & 255 != u32::from(value) {
            let expected = Expected::Masked {
                mask: 0xff,
                value: u32::from(value),
            };
            return Err(self.rejected(codec, node, 0xf0d, 0, expected, observed));
        }
        Ok(())
    }
    /// Records the answer behind an `Error::RouteRejected` for
    /// [`Self::take_rejection`], and returns that error. Diagnostics only.
    pub(super) fn rejected(
        &mut self,
        codec: u8,
        node: u8,
        verb: u16,
        payload: u16,
        expected: Expected,
        read: u32,
    ) -> Error {
        self.rejection = Some(VerbRejection {
            codec,
            node,
            verb,
            payload,
            expected,
            read,
            single_command: self.single_command,
        });
        Error::RouteRejected
    }
    /// The answer behind the last `Error::RouteRejected` of a route
    /// control or HDMI route check, once: diagnostics for the owner's log.
    pub fn take_rejection(&mut self) -> Option<VerbRejection> {
        self.rejection.take()
    }
    /// The last HDMI runtime readback a second read contradicted
    /// ([`Misread`]), once: diagnostics for the owner's log.
    pub fn take_misread(&mut self) -> Option<Misread> {
        self.misread.take()
    }
    /// A runtime readback of `verb` (a get verb or the long format get on
    /// `node`): its answer, read again at once when `expected` refuses it.
    /// A second answer as expected was a transport misread: counted
    /// ([`super::Stats::misreads`]) and kept ([`Self::take_misread`]), and
    /// the route stands. A second answer still wrong rejects the route with
    /// it ([`Self::rejected`]): the codec holds what the route did not set.
    fn hdmi_confirm(
        &mut self,
        verb: Verb,
        (get, payload): (u16, u16),
        expected: Expected,
    ) -> Result<(), Error> {
        let first = self.verb(verb)?;
        if expected.accepts(first) {
            return Ok(());
        }
        let again = self.verb(verb)?;
        let (codec, node) = (verb.codec(), verb.node());
        if !expected.accepts(again) {
            return Err(self.rejected(codec, node, get, payload, expected, again));
        }
        self.stats.misreads += 1;
        self.misread = Some(Misread {
            first: VerbRejection {
                codec,
                node,
                verb: get,
                payload,
                expected,
                read: first,
                single_command: self.single_command,
            },
            again,
        });
        Ok(())
    }
    fn hdmi_get(&mut self, codec: u8, node: u8, verb: u16, payload: u8) -> Result<u32, Error> {
        self.verb(Verb::short(codec, node, verb, payload)?)
    }
    fn hdmi_amp(&mut self, codec: u8, node: u8) -> Result<[u32; 2], Error> {
        Ok([
            self.verb(Verb::long(codec, node, 0xb, 0x8000)?)?,
            self.verb(Verb::long(codec, node, 0xb, 0xa000)?)?,
        ])
    }
    /// The prepared route's converter and pin as the codec answers now, and
    /// its stream descriptor's registers, for the owner's log. Get verbs
    /// only, except the DIP index: set to packet 0's start before its bytes
    /// are read, and back to it after, as `prepare_hdmi` leaves it (Linux
    /// hdmi_infoframe_uptodate reads the DIP the same way). Nothing is
    /// checked or programmed; `SinkUnavailable` without a route.
    pub fn hdmi_route_state(&mut self, id: HdmiPortId) -> Result<HdmiRouteState, Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let route = port.route.as_ref().ok_or(Error::SinkUnavailable)?;
        let (codec, pin, device, devices) =
            (port.graph.address, port.pin, port.device, port.devices);
        let (stream, converter, mux, digital, format) = (
            route.stream,
            route.converter,
            route.mux,
            route.digital,
            route.format,
        );
        let group = port
            .graph
            .groups
            .iter()
            .find(|g| g.widgets.iter().any(|w| w.node == pin))
            .ok_or(Error::InvalidCodecGraph)?
            .node;
        let amplified = |node| {
            port.graph
                .widget(node)
                .is_some_and(|w| w.output_amplifier.is_some())
        };
        let (converter_amplified, pin_amplified) = (amplified(converter), amplified(pin));
        let set_format = format.encode()?;
        self.hdmi_select_port(index)?;
        let group_power = self.hdmi_get(codec, group, 0xf05, 0)?;
        let converter_power = self.hdmi_get(codec, converter, 0xf05, 0)?;
        let digital_read = self.hdmi_get(codec, converter, 0xf0d, 0)?;
        let channels = self.hdmi_get(codec, converter, 0xf2d, 0)?;
        let format_read = self.verb(Verb::long(codec, converter, 0xa, 0)?)?;
        let stream_read = self.hdmi_get(codec, converter, 0xf06, 0)?;
        let converter_amp = if converter_amplified {
            Some(self.hdmi_amp(codec, converter)?)
        } else {
            None
        };
        let pin_power = self.hdmi_get(codec, pin, 0xf05, 0)?;
        let connection = self.hdmi_get(codec, pin, 0xf01, 0)?;
        let pin_control = self.hdmi_get(codec, pin, 0xf07, 0)?;
        let sense = self.hdmi_pin_sense(index)?;
        let device_select = if devices > 1 {
            Some(self.hdmi_get(codec, pin, 0xf35, 0)?)
        } else {
            None
        };
        let pin_amp = if pin_amplified {
            Some(self.hdmi_amp(codec, pin)?)
        } else {
            None
        };
        let mut slots = [0; 8];
        for (slot, value) in slots.iter_mut().enumerate() {
            *value = self.hdmi_get(codec, pin, 0xf34, slot as u8)?;
        }
        let dip_size = self.hdmi_get(codec, pin, 0xf2e, 0)?;
        let dip_transmit = self.hdmi_get(codec, pin, 0xf32, 0)?;
        let mut dip = Vec::new();
        let bytes = if dip_size == u32::MAX {
            0
        } else {
            dip_size.min(32)
        };
        if bytes > 0 {
            self.verb(Verb::short(codec, pin, 0x730, 0)?)?;
            for _ in 0..bytes {
                dip.push(self.hdmi_get(codec, pin, 0xf31, 0)? as u8);
            }
            self.verb(Verb::short(codec, pin, 0x730, 0)?)?;
        }
        let base = super::registers::stream_base(stream.descriptor_index());
        let sd_control = u32::from(self.registers.read8(base))
            | u32::from(self.registers.read8(base + 1)) << 8
            | u32::from(self.registers.read8(base + 2)) << 16;
        Ok(HdmiRouteState {
            codec,
            group,
            converter,
            pin,
            device,
            group_power,
            converter_power,
            pin_power,
            digital: digital_read,
            set_digital: digital,
            channels,
            set_channels: format.channels - 1,
            format: format_read,
            set_format,
            stream: stream_read,
            set_stream: stream.tag() << 4,
            converter_amp,
            pin_amp,
            connection,
            set_connection: mux,
            pin_control,
            sense,
            device_select,
            slots,
            dip_size,
            dip_transmit,
            dip,
            sd_control,
            sd_status: self.registers.read8(base + 3),
            sd_position: self.registers.read32(base + 4),
            sd_fifo: self.registers.read16(base + 0x10),
            sd_format: self.registers.read16(base + 0x12),
            single_command: self.single_command,
        })
    }
    fn hdmi_packet(
        &mut self,
        codec: u8,
        pin: u8,
        connection: Connection,
        map: &ChannelMap,
    ) -> Result<(), Error> {
        self.minimum_guard()?;
        let mut packet = [0u8; 9];
        packet[0] = 0x84;
        match connection {
            Connection::Hdmi => {
                packet[1] = 1;
                packet[2] = 10;
            }
            Connection::DisplayPort => {
                packet[1] = 0x1b;
                packet[2] = 0x44;
            }
        }
        packet[4] = map.order.len() as u8 - 1;
        packet[7] = map.allocation;
        packet[3] = 0u8.wrapping_sub(packet.iter().fold(0u8, |s, b| s.wrapping_add(*b)));
        // Stop -> write entire union -> verify DIP RAM -> transmit.
        self.route_control(codec, pin, 0x730, 0xf30, 0)?;
        self.route_control(codec, pin, 0x732, 0xf32, 0)?;
        for byte in packet {
            self.verb(Verb::short(codec, pin, 0x731, byte)?)?;
        }
        self.route_control(codec, pin, 0x730, 0xf30, 0)?;
        for (index, byte) in packet.into_iter().enumerate() {
            let read = self.verb(Verb::short(codec, pin, 0xf31, 0)?)?;
            if read != u32::from(byte) {
                let expected = Expected::Value(u32::from(byte));
                return Err(self.rejected(codec, pin, 0xf31, index as u16, expected, read));
            }
        }
        self.route_control(codec, pin, 0x730, 0xf30, 0)?;
        self.route_control(codec, pin, 0x732, 0xf32, 0xc0)
    }
    /// Prepare digital converter, mux, count, slots, packet and pin for
    /// 2..8ch LPCM, constrained by this sink's ELD; a sink present with
    /// ELD_VALID set whose ELD does not read or parse takes [`BASIC_AUDIO`]
    /// on FL/FR only, else its ELD error. No HBR/non-PCM alias.
    pub fn prepare_hdmi(
        &mut self,
        id: HdmiPortId,
        stream: StreamId,
        converter: u8,
        custom_map: Option<&[Speaker]>,
    ) -> Result<ChannelMap, Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        if self.routing_failed {
            return Err(Error::NeedsReset);
        }
        self.require_routing_stopped()?;
        if self.hdmi_ports[index].as_ref().unwrap().route.is_some() {
            return Err(Error::RouteInUse);
        }
        let format = self.stream_format(stream)?;
        let period = self.stream(stream)?.period_bytes;
        let ring = self.stream_buffer_bytes(stream)?;
        if ring / period < 3 {
            return Err(Error::InvalidBuffer);
        }
        if self.stream_direction(stream)? != Direction::Playback {
            return Err(Error::InvalidFormat);
        }
        let graph = self.hdmi_ports[index].as_ref().unwrap().graph.clone();
        let pin = self.hdmi_ports[index].as_ref().unwrap().pin;
        self.hdmi_topology(&graph, pin)?;
        let snapshot = self.poll_hdmi_sink(id)?;
        let (speakers, connection) = match (&snapshot.eld, snapshot.eld_error) {
            (Some(eld), _) => {
                if !eld.supports(format) {
                    return Err(Error::InvalidFormat);
                }
                (eld.speakers, eld.connection)
            }
            // Present with ELD_VALID set, but its ELD did not read or
            // parse: basic audio on FL/FR only, over HDMI unless the pin
            // cannot carry it (Linux's default connection type).
            (None, Some(error)) => {
                if format != BASIC_AUDIO {
                    return Err(error);
                }
                let caps = graph
                    .widget(pin)
                    .and_then(|w| w.pin_capabilities)
                    .unwrap_or(0);
                let connection = if caps & PIN_HDMI == 0 && caps & PIN_DP != 0 {
                    Connection::DisplayPort
                } else {
                    Connection::Hdmi
                };
                (BASIC_SPEAKERS, connection)
            }
            (None, None) => return Err(Error::SinkUnavailable),
        };
        let map = ChannelMap::choose(format.channels, speakers, custom_map)?;
        let widget = graph.widget(converter).ok_or(Error::InvalidCodecGraph)?;
        let actual = self.enumerate_codec(graph.address)?;
        let live = actual.widget(converter).ok_or(Error::InvalidCodecGraph)?;
        if widget.kind != WidgetKind::Output
            || widget.capabilities & DIGITAL == 0
            || widget.channels() < format.channels
            || widget
                .power_states
                .is_some_and(|p| p == u32::MAX || p & 1 == 0)
            || live.power_states != widget.power_states
            || !widget.pcm.supports(format)
            || live.capabilities != widget.capabilities
            || live.pcm.rates_and_bits != widget.pcm.rates_and_bits
            || live.pcm.stream_formats != widget.pcm.stream_formats
        {
            return Err(Error::InvalidFormat);
        }
        let mux = self.hdmi_ports[index]
            .as_ref()
            .unwrap()
            .connections
            .iter()
            .position(|n| *n == converter)
            .ok_or(Error::UnsupportedTopology)? as u8;
        if self.converter_bindings.iter().any(|binding| {
            binding.stream == stream
                || (binding.codec == graph.address
                    && (binding.node == pin || binding.node == converter))
        }) {
            return Err(Error::RouteInUse);
        }
        if self.hdmi_ports.iter().flatten().any(|p| {
            p.route.as_ref().is_some_and(|r| {
                r.stream == stream || (p.graph.address == graph.address && r.converter == converter)
            })
        }) || self.routes.iter().any(|(owner, r)| {
            *owner == stream
                || (r.codec_address() == graph.address
                    && (r.nodes().contains(&pin) || r.nodes().contains(&converter)))
        }) || self
            .codec_controls
            .iter()
            .any(|(_, c, n)| *c == graph.address && (*n == pin || *n == converter))
        {
            return Err(Error::RouteInUse);
        }
        self.hdmi_select_port(index)?;
        let packet_size = self.verb(Verb::short(graph.address, pin, 0xf2e, 0)?)?;
        if !(9..=32).contains(&packet_size) {
            let expected = Expected::Within { low: 9, high: 32 };
            return Err(self.rejected(graph.address, pin, 0xf2e, 0, expected, packet_size));
        }
        let digital = self.verb(Verb::short(graph.address, converter, 0xf0d, 0)?)?;
        if digital == u32::MAX {
            let expected = Expected::Answered;
            return Err(self.rejected(graph.address, converter, 0xf0d, 0, expected, digital));
        }
        let digital = (digital as u8 & !0x21) | 1; // LPCM + enabled; keep unrelated channel-status controls.
        // Reserve all owners BEFORE GPU or codec effects, including failed ACK.
        self.streams[usize::from(stream.descriptor_index())]
            .as_mut()
            .unwrap()
            .hdmi_required = true;
        self.hdmi_ports[index].as_mut().unwrap().route = Some(Route {
            stream,
            converter,
            mux,
            format,
            map: map.clone(),
            digital,
            connection,
            generation: snapshot.state.generation,
            prepared: false,
            progress: None,
            verify: false,
            reprogram: false,
            reprogrammed: false,
            paused: false,
            faults: 0,
        });
        let result = (|| {
            self.hdmi_effect(index, Some(format))?;
            self.hdmi_program(index)?;
            if self.hdmi_observe(index)? != snapshot.state {
                return Err(Error::SinkChanged);
            }
            self.hdmi_ports[index]
                .as_mut()
                .unwrap()
                .route
                .as_mut()
                .unwrap()
                .prepared = true;
            Ok(map)
        })();
        result.map_err(|error| {
            self.routing_failed = true;
            self.hdmi_record(index, error)
        })
    }
    /// Set the route's codec state at `index` as [`Self::prepare_hdmi`]
    /// sets it, each set verb with its readback: the device entry, the
    /// function group, converter and pin in D0, the connection select, pin
    /// control off, the digital converter (DIG1), the channel count and
    /// mapping, the pin's amplifier unmuted, the audio infoframe (stop,
    /// write, verify, transmit), pin control on, then the converter's
    /// format and stream tag. Nothing else: no graph walk, connection list,
    /// ELD or DIP size read, and no call to the display owner. Linux
    /// update_eld sets the infoframe again in the same way when a sink's
    /// ELD becomes valid again. Called only with the stream stopped.
    fn hdmi_program(&mut self, index: usize) -> Result<(), Error> {
        self.minimum_guard()?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let route = port.route.as_ref().ok_or(Error::SinkUnavailable)?;
        let (codec, pin) = (port.graph.address, port.pin);
        let group = port
            .graph
            .groups
            .iter()
            .find(|g| g.widgets.iter().any(|w| w.node == pin))
            .ok_or(Error::InvalidCodecGraph)?
            .node;
        let powered = |node| {
            port.graph
                .widget(node)
                .is_some_and(|w| w.power_states.is_some())
        };
        let (converter, mux, digital, format, map, connection, tag) = (
            route.converter,
            route.mux,
            route.digital,
            route.format,
            route.map.clone(),
            route.connection,
            route.stream.tag(),
        );
        let (converter_powered, pin_powered) = (powered(converter), powered(pin));
        let pin_amplified = port
            .graph
            .widget(pin)
            .is_some_and(|w| w.output_amplifier.is_some());
        self.hdmi_select_port(index)?;
        self.power_d0(codec, group)?;
        if converter_powered {
            self.power_d0(codec, converter)?;
        }
        if pin_powered {
            self.power_d0(codec, pin)?;
        }
        self.route_control(codec, pin, 0x701, 0xf01, mux)?;
        self.route_control(codec, pin, 0x707, 0xf07, 0)?;
        self.hdmi_digital_control(codec, converter, digital)?;
        self.route_control(codec, converter, 0x72d, 0xf2d, format.channels - 1)?;
        for slot in map.slots {
            self.verb(Verb::short(codec, pin, 0x734, slot)?)?;
        }
        // NVIDIA stereo ignores ASP remapping; the fixed FL/FR map is
        // validated at prepare. Wider maps must read back every slot.
        if format.channels > 2 {
            for slot in map.slots {
                let value = self.verb(Verb::short(codec, pin, 0xf34, slot & 15)?)?;
                if value & 0xf0 != u32::from(slot & 0xf0) || value == u32::MAX {
                    let expected = Expected::Masked {
                        mask: 0xf0,
                        value: u32::from(slot & 0xf0),
                    };
                    let payload = u16::from(slot & 15);
                    return Err(self.rejected(codec, pin, 0xf34, payload, expected, value));
                }
            }
        }
        if pin_amplified {
            self.verb(Verb::long(codec, pin, 3, 0xb000)?)?;
            for channel in [0x8000, 0xa000] {
                let read = self.verb(Verb::long(codec, pin, 0xb, channel)?)?;
                if read & 0xff != 0 {
                    let expected = Expected::Masked {
                        mask: 0xff,
                        value: 0,
                    };
                    return Err(self.rejected(codec, pin, 0xb, channel, expected, read));
                }
            }
        }
        self.hdmi_packet(codec, pin, connection, &map)?;
        self.route_control(codec, pin, 0x707, 0xf07, 0x40)?;
        self.hdmi_long_control(codec, converter, 2, 0xa, format.encode()?)?;
        self.route_control(codec, converter, 0x706, 0xf06, tag << 4)
    }
    /// The next status read of the port's stream reads its whole route
    /// back (mux, pin control, stream tag, channel count, infoframe
    /// transmit, digital converter, slots, format), once; other status
    /// reads check only the pin sense, and a start always reads the whole
    /// route. The owner calls this once per service turn: a route rejected
    /// between turns is caught at the next one, at a few verbs a turn
    /// rather than eight at every status read. Nothing without a route.
    pub fn schedule_hdmi_readback(&mut self, id: HdmiPortId) -> Result<(), Error> {
        let index = self.hdmi_index(id)?;
        if let Some(route) = self.hdmi_ports[index].as_mut().unwrap().route.as_mut() {
            route.verify = true;
        }
        Ok(())
    }
    /// Standard IEC958 digital-output enable, with live sink/route
    /// validation. This does not change coding type, PCM format, rate or GPU
    /// association; the DMA clock may continue while the output is disabled.
    pub fn set_hdmi_enabled(&mut self, id: HdmiPortId, enabled: bool) -> Result<(), Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        if self.routing_failed {
            return Err(Error::NeedsReset);
        }
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let route = port.route.as_ref().ok_or(Error::SinkUnavailable)?;
        let (stream, codec, cvt, value) = (
            route.stream,
            port.graph.address,
            route.converter,
            (route.digital & !1) | u8::from(enabled),
        );
        self.stream(stream)?;
        self.hdmi_runtime(stream, true)?;
        self.hdmi_ports[index]
            .as_mut()
            .unwrap()
            .route
            .as_mut()
            .unwrap()
            .digital = value;
        self.hdmi_digital_control(codec, cvt, value).map_err(|e| {
            self.routing_failed = true;
            self.hdmi_record(index, e)
        })
    }
    fn hdmi_close_index(&mut self, index: usize) -> Result<(), Error> {
        self.minimum_guard()?;
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let (codec, pin) = (port.graph.address, port.pin);
        let Some(route) = port.route.as_ref() else {
            return Ok(());
        };
        let (stream, converter, digital) = (route.stream, route.converter, route.digital);
        // DMA-stop ACK is mandatory before changing route/clock owners.
        self.stop_descriptor(stream.descriptor_index())?;
        if let Some(s) = self.streams[usize::from(stream.descriptor_index())].as_mut() {
            s.running = false;
        }
        self.hdmi_select_port(index)?;
        self.route_control(codec, pin, 0x730, 0xf30, 0)?;
        self.route_control(codec, pin, 0x732, 0xf32, 0)?;
        self.route_control(codec, pin, 0x707, 0xf07, 0)?;
        self.route_control(codec, converter, 0x706, 0xf06, 0)?;
        self.hdmi_digital_control(codec, converter, digital & !1)?;
        self.hdmi_effect(index, None)?;
        self.hdmi_ports[index].as_mut().unwrap().route = None;
        Ok(())
    }
    pub fn close_hdmi(&mut self, id: HdmiPortId) -> Result<(), Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        self.hdmi_close_index(index).map_err(|e| {
            self.routing_failed = true;
            self.hdmi_record(index, e)
        })
    }
    pub fn detach_hdmi_port(&mut self, id: HdmiPortId) -> Result<(), Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        self.close_hdmi(id)?;
        let mut port = self.hdmi_ports[index].take().unwrap();
        // SAFETY: close_hdmi acknowledged stream, converter and GPU stop.
        // Taking the port removes its stored Arc; ManuallyDrop prevents an
        // automatic second release after this explicit drop.
        unsafe {
            ManuallyDrop::drop(&mut port.owner);
        }
        Ok(())
    }
    pub(super) fn shutdown_hdmi(&mut self) -> Result<(), Error> {
        self.minimum_guard()?;
        let mut first = None;
        for index in 0..self.hdmi_ports.len() {
            if self.hdmi_ports[index].is_some()
                && let Err(error) = self.hdmi_close_index(index)
            {
                first.get_or_insert(self.hdmi_record(index, error));
            }
        }
        first.map_or(Ok(()), Err)
    }
    /// Release only already-quiet GPU/HDA D0 grants before native PCI suspend.
    /// A mere request to enter D3, PMCSR capability or cleared software RUN is
    /// not proof. Successful shutdown must have acknowledged every DMA/command
    /// stop and every codec/GPU route effect in the original D0 epoch.
    #[allow(
        dead_code,
        reason = "PCI suspend is a preserved native lifecycle boundary; HDMI retirement is exercised before the production PCI suspend consumer is wired"
    )] // Consumer is the separately owned native PCI lifecycle.
    pub(super) fn retire_hdmi_for_pci_suspend(&mut self) -> Result<(), Error> {
        self.minimum_guard()?;
        if !self.stopped
            || self.lifecycle_failed
            || !self.command_poisoned
            || self.hdmi_ports.iter().flatten().any(|p| p.route.is_some())
        {
            return Err(Error::NeedsReset);
        }
        for entry in &mut self.hdmi_ports {
            if let Some(mut port) = entry.take() {
                // SAFETY: the stopped, command_quiet and lifecycle checks above
                // establish completed shutdown. Taking the entry removes its
                // stored Arc, which is explicitly released exactly once.
                unsafe {
                    ManuallyDrop::drop(&mut port.owner);
                }
            }
        }
        Ok(())
    }
    pub(super) fn retire_hdmi_after_reset(&mut self) -> Result<(), Error> {
        self.minimum_guard()?;
        // A lost command ACK may prevent pre-reset codec cleanup. The
        // full link/command reset restores unambiguous verbs; re-enumerate the
        // original codec/pin before proving quiet and releasing any old owner.
        for index in 0..self.hdmi_ports.len() {
            if let Some(port) = self.hdmi_ports[index].as_ref() {
                if port.route.is_none() {
                    continue;
                }
                let (codec, pin, vendor) = (port.graph.address, port.pin, port.graph.vendor_id);
                let graph = self.enumerate_codec(codec)?;
                if graph.vendor_id != vendor {
                    return Err(Error::InvalidCodecGraph);
                }
                self.hdmi_topology(&graph, pin)?;
                self.hdmi_ports[index].as_mut().unwrap().graph = graph;
                self.hdmi_close_index(index)?;
            }
        }
        // Do not discard a failed route on reset. Whole stream/codec/GPU quiet
        // must have succeeded; otherwise the original port remains quarantined.
        if self.hdmi_ports.iter().flatten().any(|p| p.route.is_some()) {
            return Err(Error::NeedsReset);
        }
        for entry in &mut self.hdmi_ports {
            if let Some(mut p) = entry.take() {
                // SAFETY: all original routes are quiet after acknowledged
                // stream/codec/GPU stop above. Taking this owned Port removes
                // its sole stored Arc; release it once and never read it again.
                // Port's ManuallyDrop field prevents a second automatic drop.
                unsafe {
                    ManuallyDrop::drop(&mut p.owner);
                }
            }
        }
        self.hdmi_ports.clear();
        Ok(())
    }
    pub(super) fn hdmi_owns_node(&self, codec: u8, node: u8) -> bool {
        self.hdmi_ports.iter().flatten().any(|p| {
            p.graph.address == codec
                && (p.pin == node || p.route.as_ref().is_some_and(|r| r.converter == node))
        })
    }
    pub(super) fn hdmi_start_failed(&mut self, index: usize, id: StreamId, error: Error) {
        self.hdmi_record(index, error);
        if let Some(stream) = self.streams[usize::from(id.descriptor_index())].as_mut() {
            stream.poisoned = true;
        }
        self.routing_failed = true;
    }
    pub(super) fn hdmi_stream_port(&self, id: StreamId) -> Option<usize> {
        self.hdmi_ports.iter().position(|p| {
            p.as_ref()
                .is_some_and(|p| p.route.as_ref().is_some_and(|r| r.stream == id))
        })
    }
    /// Stop only the route's stream: its DMA stops (acknowledged) and its
    /// progress ends, while the converter, stream tag, format, pin and the
    /// display owner's audio stay as prepared. A stop that is not
    /// acknowledged poisons the stream and fails routing, as a failed
    /// close does.
    fn hdmi_stop_stream(&mut self, index: usize) -> Result<(), Error> {
        self.minimum_guard()?;
        let route = self.hdmi_ports[index]
            .as_mut()
            .unwrap()
            .route
            .as_mut()
            .ok_or(Error::SinkUnavailable)?;
        route.progress = None;
        let stream = route.stream;
        let result = self.stop_descriptor(stream.descriptor_index());
        if let Some(s) = self.streams[usize::from(stream.descriptor_index())].as_mut() {
            s.running = false;
            if result.is_err() {
                s.poisoned = true;
            }
        }
        if let Err(cleanup) = result {
            self.routing_failed = true;
            self.hdmi_record(index, cleanup);
        }
        result
    }
    /// A stream-level fault of the route's DMA (an underrun, a position
    /// past the ring): `error` is recorded and only the stream stops; the
    /// route stays prepared for [`Self::restart_hdmi`].
    pub(super) fn hdmi_stream_fault(&mut self, index: usize, error: Error) -> Error {
        self.hdmi_record(index, error);
        let _ = self.hdmi_stop_stream(index);
        error
    }
    /// A failed runtime check of the route at `index`, by `kind`.
    fn hdmi_runtime_fault(&mut self, index: usize, error: Error, kind: RuntimeFault) -> Error {
        self.hdmi_record(index, error);
        match kind {
            RuntimeFault::Absent => {
                if self.hdmi_stop_stream(index).is_ok() {
                    let route = self.hdmi_ports[index].as_mut().unwrap().route.as_mut();
                    if let Some(route) = route {
                        route.paused = true;
                    }
                }
                error
            }
            RuntimeFault::Transient => {
                if self.hdmi_stop_stream(index).is_err() {
                    return error;
                }
                let route = self.hdmi_ports[index].as_mut().unwrap().route.as_mut();
                let faults = route.map_or(0, |route| {
                    if error == Error::RouteRejected {
                        // The restart sets the route's verbs again first;
                        // only a readback still wrong after that counts.
                        route.reprogram = true;
                        if !route.reprogrammed {
                            return route.faults;
                        }
                    }
                    route.faults = route.faults.saturating_add(1);
                    route.faults
                });
                if faults <= HDMI_TRANSIENT_FAULTS {
                    return error;
                }
                // Bounded: the codec keeps answering what the route did not
                // set. Give the route up as a reset condition.
                if let Err(cleanup) = self.hdmi_close_index(index) {
                    self.hdmi_record(index, cleanup);
                }
                self.routing_failed = true;
                Error::NeedsReset
            }
            RuntimeFault::Lost => {
                // A normal hotplug is recoverable after cleanup. A failed
                // stop/command/access keeps the route, first error and owners.
                if let Err(cleanup) = self.hdmi_close_index(index) {
                    self.routing_failed = true;
                    self.hdmi_record(index, cleanup);
                }
                error
            }
        }
    }
    /// Whether the port has a prepared route, running or stopped.
    pub fn hdmi_prepared(&self, id: HdmiPortId) -> Result<bool, Error> {
        let index = self.hdmi_index(id)?;
        Ok(self.hdmi_ports[index]
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .is_some_and(|route| route.prepared))
    }
    /// Whether the port's prepared route is paused: its stream stopped
    /// because the pin sensed no sink with a valid ELD.
    pub fn hdmi_paused(&self, id: HdmiPortId) -> Result<bool, Error> {
        let index = self.hdmi_index(id)?;
        Ok(self.hdmi_ports[index]
            .as_ref()
            .unwrap()
            .route
            .as_ref()
            .is_some_and(|route| route.paused))
    }
    /// Start the stream of a route that is still prepared after a
    /// stream-level stop: an underrun, a transient codec fault, or a pause
    /// while the sink was away. The ring is silenced and the stream starts
    /// as [`Self::start`] does: the route's readback (pin sense, mux, pin
    /// control, stream tag, channel count, packet transmit, digital
    /// control, format), then the stream descriptor's reset and re-arm on
    /// the same converter, stream tag and format. Neither the codec graph
    /// nor a connection list is read; the ELD is read only when resuming
    /// from a pause. On a resume, and after a readback rejected the route,
    /// the route's verbs are set again first ([`Self::hdmi_program`]): a
    /// rejected readback counts toward [`HDMI_TRANSIENT_FAULTS`] only when
    /// it is still wrong right after that.
    ///
    /// `SinkUnavailable`: no prepared route, or the sink is still away (the
    /// route stays paused). `SinkChanged`: the sink came back with another
    /// ELD, or its route no longer read back as prepared; the route was
    /// closed and needs [`Self::prepare_hdmi`]. A transient codec fault
    /// counts toward [`HDMI_TRANSIENT_FAULTS`].
    pub fn restart_hdmi(&mut self, id: HdmiPortId) -> Result<HdmiRestart, Error> {
        self.minimum_guard()?;
        let index = self.hdmi_index(id)?;
        if self.routing_failed {
            return Err(Error::NeedsReset);
        }
        let port = self.hdmi_ports[index].as_ref().unwrap();
        let (codec, pin) = (port.graph.address, port.pin);
        let (stream, paused, reprogram) = match port.route.as_ref() {
            Some(route) if route.prepared => (route.stream, route.paused, route.reprogram),
            _ => return Err(Error::SinkUnavailable),
        };
        if self.stream(stream)?.running {
            return Err(Error::StreamRunning);
        }
        if paused {
            let sense = self
                .hdmi_select_port(index)
                .and_then(|()| self.hdmi_pin_sense(index));
            let sense = match sense {
                Ok(u32::MAX) => Err(Error::InvalidEld),
                sense => sense,
            };
            let sense = match sense {
                Ok(sense) => sense,
                Err(error) => {
                    let kind = RuntimeFault::of(error);
                    return Err(self.hdmi_runtime_fault(index, error, kind));
                }
            };
            if !eld_ready(sense) {
                return Err(Error::SinkUnavailable);
            }
            // The sink is back: the route plays to it again only with the
            // ELD it was prepared for (or, for basic audio, still none that
            // reads or parses).
            let (_, _, eld) = self.read_eld(codec, pin);
            if let Err(error) = eld
                && !matches!(error, Error::EldRead(_) | Error::EldParse(_))
            {
                let kind = RuntimeFault::of(error);
                return Err(self.hdmi_runtime_fault(index, error, kind));
            }
            let sink = self.hdmi_ports[index].as_ref().unwrap().sink.as_ref();
            let same = match (&eld, sink) {
                (Ok(eld), Some(sink)) => sink.eld.as_ref() == Some(eld),
                (Err(_), Some(sink)) => sink.eld.is_none() && sink.eld_error.is_some(),
                (_, None) => false,
            };
            if !same {
                return Err(self.hdmi_runtime_fault(index, Error::SinkChanged, RuntimeFault::Lost));
            }
            let route = self.hdmi_ports[index].as_mut().unwrap().route.as_mut();
            if let Some(route) = route {
                route.paused = false;
            }
        }
        // Back from a pause (the display side may have reset what it
        // shares with the codec), or after a rejected readback: the route's
        // verbs are set again, with no graph walk, before the start's
        // readback; from here a rejected readback counts.
        if paused || reprogram {
            let route = self.hdmi_ports[index].as_mut().unwrap().route.as_mut();
            if let Some(route) = route {
                route.reprogrammed = true;
            }
            if let Err(error) = self.hdmi_program(index) {
                let (error, kind) = match error {
                    // As a resume's readback below: prepare it again.
                    Error::RouteRejected if paused => (Error::SinkChanged, RuntimeFault::Lost),
                    error => (error, RuntimeFault::of(error)),
                };
                return Err(self.hdmi_runtime_fault(index, error, kind));
            }
        }
        // The ring restarts from its start: silence until refilled.
        self.streams[usize::from(stream.descriptor_index())]
            .as_mut()
            .ok_or(Error::InvalidStream)?
            .buffer
            .bytes_mut()
            .fill(0);
        let started = self.start(stream);
        if started.is_ok()
            && let Some(route) = self.hdmi_ports[index].as_mut().unwrap().route.as_mut()
        {
            route.reprogram = false;
            route.reprogrammed = false;
        }
        match started {
            Ok(()) if paused => Ok(HdmiRestart::Resumed),
            Ok(()) if reprogram => Ok(HdmiRestart::Reprogrammed),
            Ok(()) => Ok(HdmiRestart::Stream),
            // Back from a pause, the codec no longer holds the route as
            // prepared (the display side may have reset it): prepare it
            // again rather than counting toward a reset.
            Err(Error::RouteRejected) if paused && self.hdmi_stream_port(stream) == Some(index) => {
                Err(self.hdmi_runtime_fault(index, Error::SinkChanged, RuntimeFault::Lost))
            }
            Err(error) => Err(error),
        }
    }
    /// The route's runtime check before a start and at each status read:
    /// the display owner's state and the pin sense always (a sink away
    /// pauses the route); with `full`, or when the owner scheduled it
    /// ([`Self::schedule_hdmi_readback`]), the whole route read back.
    pub(super) fn hdmi_runtime(&mut self, id: StreamId, full: bool) -> Result<(), Error> {
        self.minimum_guard()?;
        let Some(index) = self.hdmi_stream_port(id) else {
            return Ok(());
        };
        let mut absent = false;
        let result = (|| {
            let port = self.hdmi_ports[index].as_mut().unwrap();
            let route = port.route.as_mut().unwrap();
            let full = full || core::mem::take(&mut route.verify);
            let (codec, pin, cvt, mux, digital, format, generation, prepared) = (
                port.graph.address,
                port.pin,
                route.converter,
                route.mux,
                route.digital,
                route.format,
                route.generation,
                route.prepared,
            );
            if !prepared {
                return Err(Error::SinkChanged);
            }
            let state = self.hdmi_observe(index)?;
            if state.generation != generation || !state.connected {
                return Err(Error::SinkChanged);
            }
            self.hdmi_select_port(index)?;
            let mut sense = self.hdmi_pin_sense(index)?;
            if sense != u32::MAX && !eld_ready(sense) {
                // Sensed again at once before pausing: audio8a's misread
                // senses (another verb's answer, without presence) paused
                // a playing route for one poll. A sink that went away
                // senses so twice; one that is there, once at most.
                let again = self.hdmi_pin_sense(index)?;
                if eld_ready(again) {
                    let first = sense;
                    self.stats.sense_misreads += 1;
                    self.misread = Some(Misread {
                        first: VerbRejection {
                            codec,
                            node: pin,
                            verb: PIN_SENSE,
                            payload: 0,
                            expected: Expected::Masked {
                                mask: PRESENT | ELD_VALID,
                                value: PRESENT | ELD_VALID,
                            },
                            read: first,
                            single_command: self.single_command,
                        },
                        again,
                    });
                }
                sense = again;
            }
            if sense == u32::MAX {
                // Unanswered: a transient codec fault, not a sink change.
                return Err(Error::InvalidEld);
            }
            if sense & (PRESENT | ELD_VALID) != (PRESENT | ELD_VALID) {
                // The monitor sleeps or was unplugged: pause.
                absent = true;
                return Err(Error::SinkUnavailable);
            }
            if !full {
                return Ok(());
            }
            // Each readback is read again once before it rejects the route
            // (`hdmi_confirm`): only a codec that holds another value twice
            // in a row stops the stream.
            for (node, get, expected) in [
                (pin, 0xf01, u32::from(mux)),
                (pin, 0xf07, 0x40),
                (cvt, 0xf06, u32::from(id.tag() << 4)),
                (cvt, 0xf2d, u32::from(format.channels - 1)),
                (pin, 0xf32, 0xc0),
            ] {
                let verb = Verb::short(codec, node, get, 0)?;
                self.hdmi_confirm(verb, (get, 0), Expected::Value(expected))?;
            }
            let expected = Expected::Masked {
                mask: 0xff,
                value: u32::from(digital),
            };
            self.hdmi_confirm(Verb::short(codec, cvt, 0xf0d, 0)?, (0xf0d, 0), expected)?;
            let map = self.hdmi_ports[index]
                .as_ref()
                .unwrap()
                .route
                .as_ref()
                .unwrap()
                .map
                .clone();
            if format.channels > 2 {
                for slot in map.slots {
                    let expected = Expected::Masked {
                        mask: 0xf0,
                        value: u32::from(slot & 0xf0),
                    };
                    let verb = Verb::short(codec, pin, 0xf34, slot & 15)?;
                    self.hdmi_confirm(verb, (0xf34, u16::from(slot & 15)), expected)?;
                }
            }
            let encoded = u32::from(format.encode()?);
            let verb = Verb::long(codec, cvt, 0xa, 0)?;
            self.hdmi_confirm(verb, (0xa, 0), Expected::Value(encoded))
        })();
        if let Err(error) = result {
            let kind = if absent {
                RuntimeFault::Absent
            } else {
                RuntimeFault::of(error)
            };
            return Err(self.hdmi_runtime_fault(index, error, kind));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
