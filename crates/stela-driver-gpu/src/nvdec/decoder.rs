//! A guest's decode session on the host: the samples a VideoToolbox client
//! hands StelaVD, decoded on NVDEC and shown, in display order, into the
//! client's pixel buffers. The gate's `DecoderOpen`/`DecoderDecode`
//! (`metal::service::decoders`) drive it a budgeted piece at a time; nothing
//! here waits past one look at an engine.
//!
//! - **Open** ([`Decoder::begin`], then [`Decoder::bring_up_step`] until it
//!   says done): the `avcC`/`hvcC` record opens a [`Stream`] whose sequence
//!   sizes the session; one buffer of the [`Layout`] is made per step, its
//!   surfaces unmapped for the CPU (pictures leave by the copy engine
//!   alone), and the copy engine's [`Copier`] last, its ring past the
//!   layout ([`copy::ring_after`]) and its staging buffer one displayed
//!   picture.
//! - **A sample** ([`Decoder::submit`], then [`Decoder::poll`]): parsed into
//!   its picture, which is submitted to NVDEC; once the engine releases it,
//!   the pictures it shows ([`Picture::outputs`]) join the queue of pictures
//!   to show, each with the caller's token of the sample that decoded it.
//! - **Showing** ([`Decoder::stage`], [`Decoder::staged`],
//!   [`Decoder::pop`]): the queue's head is blitted by the copy engine from
//!   its block-linear surface into the staging buffer, pitch-linear NV12 or
//!   P010, where the caller copies it out.
//!
//! **The stream's caller contract** ([`super::stream`]): every picture a
//! decode shows is copied out after that decode and before the next
//! submit, since the next picture may be decoded into a surface just given
//! back. [`Decoder::submit`] refuses while the queue holds a picture, so the
//! caller shows them all first.
use super::copy::{self, Copier};
use super::dpb::Output;
use super::layout::Layout;
use super::parse;
use super::session::{self, Decoded, Session, Status};
use super::stream::{Sequence, Stream};
use super::surface::Crop;
use super::{Codec, Picture};
use crate::nvenc::Node;
use crate::nvenc::layout::Geometry;
use crate::nvenc::session::Closed;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// The largest side a session decodes: NVDEC's H.264 and HEVC limit on
/// GA10x, and the copy engine's staging (`copy::MOST_STAGING`).
pub const MAX_SIDE: u32 = 4096;
/// The smallest: HEVC's 64-sample CTBs need at least one, H.264 a
/// macroblock pair; NVDEC's own minimum is 48 for HEVC.
pub const MIN_SIDE: u32 = 48;
/// The video memory one session may take: 4K Main 10 at its usual DPB
/// takes about 0.75 GiB (52 MiB a surface on NVIDIA's map).
pub const VRAM_BUDGET: u64 = 1 << 30;
/// A copy's bound.
pub const COPY_BOUND_NS: u64 = 500_000_000;
/// Tokens kept for decoded pictures not yet shown, at most: a picture the
/// stream never shows (HEVC `pic_output_flag` 0) is forgotten past it.
const MOST_TOKENS: usize = 64;

/// The order a session shows its pictures in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    /// Display order, as the stream's DPB bumps them ([`Picture::outputs`],
    /// then [`Stream::flush`]'s): VideoToolbox's temporal processing.
    #[default]
    Display,
    /// Each picture right after its decode, from its own surface: a client
    /// that reorders by itself, as VideoToolbox without temporal processing.
    Decode,
}

/// The pixel format pictures leave in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// `420v`: 8-bit luma, then interleaved CbCr.
    Nv12,
    /// `x420`: 16-bit samples, NVDEC's 10-bit surfaces as written (the
    /// sample in the high bits, as P010 wants: unconfirmed on hardware).
    P010,
}

impl Format {
    pub fn sample_bytes(self) -> u32 {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nv12 => "nv12",
            Self::P010 => "p010",
        }
    }
}

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The stream is not one this decoder takes: the client decodes it
    /// another way.
    Unsupported(&'static str),
    /// A sample's bytes were refused by the parser.
    Parse(parse::Error),
    /// The decoding session's.
    Session(session::Error),
    /// The copy engine's.
    Copy(copy::Error),
    /// A call out of order (a submit with pictures still to show).
    Order(&'static str),
}

impl Error {
    /// Whether the engine is gone for the session: a picture or a copy timed
    /// out, or a channel died.
    pub fn lost(&self) -> bool {
        matches!(
            self,
            Self::Session(session::Error::Timeout { .. } | session::Error::Dead)
                | Self::Copy(copy::Error::Timeout { .. } | copy::Error::Dead)
        )
    }

    /// Whether a channel was left busy: the file must stay open.
    pub fn pending(&self) -> bool {
        match self {
            Self::Session(session::Error::Timeout { job_done, .. }) => !job_done,
            Self::Copy(error) => error.pending(),
            _ => false,
        }
    }
}

/// What the open found: the client's pixel buffers are made of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opened {
    pub codec: Codec,
    pub coded: (u32, u32),
    pub display: (u32, u32),
    pub bit_depth: u8,
    pub format: Format,
    pub slots: u8,
    pub surfaces: u8,
}

/// A picture to show, the head of the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The caller's token of the sample that decoded it.
    pub token: u64,
    pub poc: i32,
    /// Its decode in the stream (0 for the first).
    pub decoded: u32,
    /// What the engine reported for its decode.
    pub status: Status,
    /// It was decoded with references stood in for missing ones
    /// ([`Picture::concealed`]).
    pub concealed: bool,
}

/// A picture decoded and not yet shown.
#[derive(Clone, Copy, Debug)]
struct Token {
    decoded: u32,
    token: u64,
    status: Status,
    concealed: bool,
}

/// The picture in flight.
#[derive(Clone, Debug)]
struct Flight {
    token: u64,
    decoded: u32,
    concealed: bool,
    outputs: Vec<Output>,
}

/// The head's place in the staging buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Staging {
    Empty,
    /// The copy engine is writing it.
    Copying,
    /// It is there; the copy took `ce_ns`.
    Ready {
        ce_ns: u64,
    },
}

/// The displayed picture in the staging buffer: `rows` rows of `row_bytes`
/// each, the luma's then the chroma's.
pub struct Planes<'a> {
    pub luma: &'a [u8],
    pub chroma: &'a [u8],
    pub row_bytes: usize,
    pub luma_rows: usize,
    pub chroma_rows: usize,
}

pub struct Decoder<N: Node> {
    session: Session<N>,
    copier: Option<Copier>,
    stream: Stream,
    sequence: Sequence,
    format: Format,
    crop: Crop,
    order: Order,
    ring: u64,
    flight: Option<Flight>,
    tokens: VecDeque<Token>,
    queue: VecDeque<Output>,
    staging: Staging,
}

/// The refusal of a sequence this decoder does not take, if any.
fn refusal(sequence: &Sequence) -> Option<&'static str> {
    let (width, height) = sequence.display;
    let (coded_width, coded_height) = sequence.coded;
    if !sequence.chroma_420 {
        return Some("a stream other than 4:2:0 at one bit depth");
    }
    if !sequence.frames_only {
        return Some("an interlaced H.264 stream (field pictures or MBAFF)");
    }
    match sequence.codec {
        Codec::H264 => {
            if sequence.bit_depth != 8 {
                return Some("H.264 above 8 bits");
            }
            // Constrained Baseline, Main, High; not High 10, 4:2:2, MVC.
            if !matches!(sequence.profile, 66 | 77 | 100) {
                return Some("an H.264 profile other than Baseline, Main or High");
            }
        }
        Codec::Hevc => {
            if !matches!(sequence.bit_depth, 8 | 10) {
                return Some("HEVC other than 8 or 10 bits");
            }
            // Main, Main 10, Main Still Picture.
            if !matches!(sequence.profile, 1..=3) {
                return Some("an HEVC profile other than Main or Main 10");
            }
        }
    }
    if coded_width > MAX_SIDE || coded_height > MAX_SIDE {
        return Some("a picture larger than 4096x4096");
    }
    if width < MIN_SIDE || height < MIN_SIDE || width % 2 != 0 || height % 2 != 0 {
        return Some("a picture smaller than 48x48, or of odd size");
    }
    None
}

/// What a session for the stream `record` describes is made of: the
/// stream, its sequence, its pictures' geometry and its buffers.
fn plan(codec: Codec, record: &[u8]) -> Result<(Stream, Sequence, Geometry, Layout), Error> {
    let stream = Stream::open(codec, record).map_err(Error::Parse)?;
    let sequence = stream
        .sequence()
        .ok_or(Error::Unsupported("the record has no sequence"))?;
    if let Some(why) = refusal(&sequence) {
        return Err(Error::Unsupported(why));
    }
    let geometry = Geometry::new(
        sequence.coded.0.next_multiple_of(16),
        sequence.coded.1.next_multiple_of(16),
    )
    .map_err(Error::Unsupported)?;
    let slots = usize::from(sequence.surfaces.max(1));
    let layout = Layout::with(codec, &geometry, sequence.bit_depth, slots, slots)
        .map_err(Error::Unsupported)?
        .without_cpu_surfaces();
    if layout.vram_bytes() > VRAM_BUDGET {
        return Err(Error::Unsupported(
            "the stream's surfaces take more than a session's 1 GiB",
        ));
    }
    Ok((stream, sequence, geometry, layout))
}

/// The video memory a session for the stream `record` describes takes,
/// before anything is opened: what the gate admits sessions by.
pub fn vram_bytes(codec: Codec, record: &[u8]) -> Result<u64, Error> {
    plan(codec, record).map(|(_, _, _, layout)| layout.vram_bytes())
}

impl<N: Node> Decoder<N> {
    /// Opens a `codec` session for the stream `record` (`avcC`/`hvcC`)
    /// describes, on `node`, a fresh render node file of `chipset`'s GPU:
    /// the stream, the layout and the decoder's channel. Its buffers then
    /// come a step at a time. Each picture waits at most `timeout_ns`.
    pub fn begin(
        node: N,
        chipset: u16,
        codec: Codec,
        record: &[u8],
        timeout_ns: u64,
    ) -> Result<Self, (Error, N)> {
        let (stream, sequence, geometry, layout) = match plan(codec, record) {
            Ok(plan) => plan,
            Err(error) => return Err((error, node)),
        };
        let Some(ring) = copy::ring_after(layout.end()) else {
            return Err((
                Error::Unsupported("no room for the copy engine past the surfaces"),
                node,
            ));
        };
        let format = if sequence.bit_depth > 8 {
            Format::P010
        } else {
            Format::Nv12
        };
        let session = match Session::begin_layout(node, chipset, geometry, layout, timeout_ns) {
            Ok(session) => session,
            Err((error, node)) => return Err((Error::Session(error), node)),
        };
        Ok(Self {
            session,
            copier: None,
            stream,
            sequence,
            format,
            crop: Crop {
                left: 0,
                top: 0,
                width: sequence.display.0,
                height: sequence.display.1,
            },
            order: Order::Display,
            ring,
            flight: None,
            tokens: VecDeque::new(),
            queue: VecDeque::new(),
            staging: Staging::Empty,
        })
    }

    /// Makes the next piece: a buffer, then the copier; whether all are.
    pub fn bring_up_step(&mut self) -> Result<bool, Error> {
        if self.copier.is_some() {
            return Ok(true);
        }
        if !self.session.bring_up_step().map_err(Error::Session)? {
            return Ok(false);
        }
        let staging = copy::picture_bytes(&self.crop, self.format.sample_bytes());
        let copier = Copier::open_at(self.session.node(), self.ring, staging, COPY_BOUND_NS)
            .map_err(Error::Copy)?;
        self.copier = Some(copier);
        Ok(true)
    }

    /// Shows pictures in `order` from now on (set it before the first
    /// sample).
    pub fn set_order(&mut self, order: Order) {
        self.order = order;
    }

    pub fn order(&self) -> Order {
        self.order
    }

    /// The pieces made, and the session's (its buffers and the copier).
    pub fn made(&self) -> (usize, usize) {
        let (made, planned) = self.session.made();
        (made + usize::from(self.copier.is_some()), planned + 1)
    }

    pub fn opened(&self) -> Opened {
        Opened {
            codec: self.sequence.codec,
            coded: self.sequence.coded,
            display: self.sequence.display,
            bit_depth: self.sequence.bit_depth,
            format: self.format,
            slots: self.sequence.slots,
            surfaces: self.sequence.surfaces,
        }
    }

    pub fn layout(&self) -> &Layout {
        self.session.layout()
    }

    /// The copy engine's channel, once made.
    pub fn copy_channel(&self) -> Option<i32> {
        self.copier.as_ref().map(Copier::channel)
    }

    /// Whether a picture is in flight.
    pub fn in_flight(&self) -> bool {
        self.flight.is_some()
    }

    /// The pictures waiting to be shown.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Parses `sample` and submits its picture, which `token` names to the
    /// caller when it is shown; whether there was a picture (a sample of
    /// parameter sets or SEI alone has none). Refused while a picture is in
    /// flight or waits to be shown.
    pub fn submit(&mut self, sample: &[u8], token: u64) -> Result<bool, Error> {
        if self.copier.is_none() {
            return Err(Error::Order("the session is still being made"));
        }
        if self.flight.is_some() {
            return Err(Error::Order("a picture is in flight"));
        }
        if !self.queue.is_empty() {
            return Err(Error::Order("pictures wait to be shown"));
        }
        let Some(picture) = self.stream.decode(sample).map_err(Error::Parse)? else {
            return Ok(false);
        };
        self.check(&picture)?;
        self.session.submit(&picture).map_err(Error::Session)?;
        self.crop = picture.crop;
        let concealed = picture.concealed() > 0;
        let outputs = match self.order {
            Order::Display => picture.outputs,
            // Its own surface, which the next decode does not write: the
            // caller copies it out before then.
            Order::Decode => alloc::vec![Output {
                slot: picture.slot,
                surface: picture.surface,
                poc: picture.poc,
                decoded: picture.decoded,
            }],
        };
        self.flight = Some(Flight {
            token,
            decoded: picture.decoded,
            concealed,
            outputs,
        });
        Ok(true)
    }

    /// Refuses a picture the session's surfaces were not made for.
    fn check(&self, picture: &Picture) -> Result<(), Error> {
        if picture.coded != self.sequence.coded || picture.bit_depth != self.sequence.bit_depth {
            return Err(Error::Unsupported(
                "a new sequence of another size or depth",
            ));
        }
        let (width, height) = (picture.crop.width, picture.crop.height);
        if copy::picture_bytes(&picture.crop, self.format.sample_bytes())
            > copy::picture_bytes(&self.crop_of_sequence(), self.format.sample_bytes())
            || width % 2 != 0
            || height % 2 != 0
        {
            return Err(Error::Unsupported("a picture larger than the sequence's"));
        }
        Ok(())
    }

    fn crop_of_sequence(&self) -> Crop {
        Crop {
            left: 0,
            top: 0,
            width: self.sequence.display.0,
            height: self.sequence.display.1,
        }
    }

    /// One look at the picture in flight (asking nouveau's fence when
    /// `ask`): its decode once released, whose shown pictures then join
    /// the queue; `None` while it is not.
    pub fn poll(&mut self, ask: bool) -> Result<Option<Decoded>, Error> {
        let Some(decoded) = self.session.poll(ask).map_err(|error| {
            self.flight = None;
            Error::Session(error)
        })?
        else {
            return Ok(None);
        };
        let flight = self
            .flight
            .take()
            .ok_or(Error::Order("no picture in flight"))?;
        if self.tokens.len() >= MOST_TOKENS {
            self.tokens.pop_front();
        }
        self.tokens.push_back(Token {
            decoded: flight.decoded,
            token: flight.token,
            status: decoded.status,
            concealed: flight.concealed,
        });
        self.queue.extend(flight.outputs);
        Ok(Some(decoded))
    }

    /// What the stream's parser did about pictures it could not decode
    /// as coded ([`Stream::events`]).
    pub fn events(&self) -> super::parse::hevc::Events {
        self.stream.events()
    }

    /// Why the stream's parser refused its last refused sample
    /// ([`Stream::refusal`]).
    pub fn refusal(&self) -> Option<&str> {
        self.stream.refusal()
    }

    /// Asks nouveau's fence of the picture in flight, which lets Linux run.
    pub fn ask_fence(&mut self) -> bool {
        self.session.ask_fence()
    }

    /// The end of the stream: every picture still waiting joins the queue.
    pub fn flush(&mut self) -> Result<(), Error> {
        if self.flight.is_some() {
            return Err(Error::Order("a picture is in flight"));
        }
        let rest = self.stream.flush();
        // In decode order every picture was shown as it was decoded.
        if self.order == Order::Display {
            self.queue.extend(rest);
        }
        Ok(())
    }

    /// Forgets every picture, as for a seek: nothing waits to be shown, and
    /// the next sample must start afresh (an IDR or an IRAP).
    pub fn reset(&mut self) -> Result<(), Error> {
        if self.flight.is_some() || self.staging == Staging::Copying {
            return Err(Error::Order("a picture or copy is in flight"));
        }
        self.stream.reset();
        self.queue.clear();
        self.tokens.clear();
        self.staging = Staging::Empty;
        Ok(())
    }

    /// The queue's head, with its token.
    pub fn head(&self) -> Option<Shown> {
        let output = self.queue.front()?;
        let token = self
            .tokens
            .iter()
            .rev()
            .find(|token| token.decoded == output.decoded);
        Some(Shown {
            token: token.map_or(0, |token| token.token),
            poc: output.poc,
            decoded: output.decoded,
            status: token.map(|token| token.status).unwrap_or_default(),
            concealed: token.is_some_and(|token| token.concealed),
        })
    }

    /// Puts the head in the staging buffer: starts its copy, or looks at
    /// the copy once (asking nouveau's fence when `ask`). The copy engine's
    /// time once it is there; `None` while it is copied.
    pub fn stage(&mut self, ask: bool) -> Result<Option<u64>, Error> {
        let Some(output) = self.queue.front().copied() else {
            return Err(Error::Order("no picture to show"));
        };
        let copier = self
            .copier
            .as_mut()
            .ok_or(Error::Order("the session is still being made"))?;
        match self.staging {
            Staging::Ready { ce_ns } => return Ok(Some(ce_ns)),
            Staging::Empty => {
                let layout = self.session.layout();
                let (luma, chroma) = layout.surface(output.surface);
                let geometry = *self.session.geometry();
                let blits = copy::planes(
                    luma,
                    chroma,
                    layout.format.pitch,
                    geometry.coded_height,
                    self.format.sample_bytes(),
                    &self.crop,
                    copier.staging_address(),
                );
                copier
                    .submit(self.session.node(), &blits)
                    .map_err(Error::Copy)?;
                self.staging = Staging::Copying;
            }
            Staging::Copying => {}
        }
        match copier.poll(self.session.node(), ask) {
            Ok(Some(copied)) => {
                self.staging = Staging::Ready {
                    ce_ns: copied.ce_ns,
                };
                Ok(Some(copied.ce_ns))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.staging = Staging::Empty;
                if error.pending() {
                    self.session.hold();
                }
                Err(Error::Copy(error))
            }
        }
    }

    /// The staged head, once [`Decoder::stage`] said it is there.
    pub fn staged(&self) -> Option<Planes<'_>> {
        if !matches!(self.staging, Staging::Ready { .. }) {
            return None;
        }
        let sample = self.format.sample_bytes() as usize;
        let row_bytes = self.crop.width as usize * sample;
        let luma_rows = self.crop.height as usize;
        let chroma_rows = luma_rows / 2;
        let bytes = row_bytes * (luma_rows + chroma_rows);
        let all = self.copier.as_ref()?.output(bytes)?;
        let (luma, chroma) = all.split_at(row_bytes * luma_rows);
        Some(Planes {
            luma,
            chroma,
            row_bytes,
            luma_rows,
            chroma_rows,
        })
    }

    /// The shown picture's size: the displayed rectangle.
    pub fn shown_size(&self) -> (u32, u32) {
        (self.crop.width, self.crop.height)
    }

    /// The head is shown: the next is.
    pub fn pop(&mut self) {
        if let Some(output) = self.queue.pop_front() {
            self.tokens.retain(|token| token.decoded != output.decoded);
        }
        self.staging = Staging::Empty;
    }

    /// Whether a picture timed out, and whether its channel is busy.
    pub fn dead(&self) -> Option<bool> {
        self.session.dead()
    }

    /// Ends the session: its node, and whether the file must stay open
    /// (a channel, the decoder's or the copy engine's, is still busy).
    pub fn close(mut self) -> Closed<N> {
        if let Some(copier) = self.copier.take() {
            if self.session.dead() == Some(true) {
                copier.leave(self.session.node());
            } else if copier.close(self.session.node()) {
                self.session.hold();
            }
        }
        self.session.close()
    }
}

#[cfg(test)]
mod tests;
