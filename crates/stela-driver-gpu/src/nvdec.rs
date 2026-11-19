//! NVDEC, GA10x's video decoder (`NVC7B0_VIDEO_DECODER`), driven through
//! nouveau from Stela's host.
//!
//! Three users:
//!
//! - **The "alive" probe** ([`probe`], `--probe-nvdec`): does GSP-RM run the
//!   decoder for nouveau on this GPU? It is NVENC's probe
//!   ([`nvenc::probe_engine`]) on the decoder: on a render node of its own it
//!   opens a channel on the decoder's runlist (nouveau's abi16
//!   `NOUVEAU_FIFO_ENGINE_NVDEC`, Stela's Linux patch 0208 "allow allocating
//!   NVDEC channels via abi16"), creates the NVC7B0 class on it, and pushes
//!   `SET_OBJECT`, `SET_APPLICATION_ID` (H.264) and a `SEMAPHORE_A..D`
//!   release of [`PAYLOAD`] to a small buffer of video memory, read until the
//!   payload lands or the caller's bound passes.
//! - **The decoder** ([`session`], [`selftest`], `--nvdec-selftest`): H.264
//!   and HEVC parsed on the host ([`parse`]), fed whole clips or one sample
//!   at a time ([`stream`]), each picture placed in a surface slot and
//!   shown in display order ([`dpb`]), its setup filled as NVIDIA's driver
//!   fills it ([`setup`]), its method stream NVIDIA's ([`push`]) on
//!   NVIDIA's buffer layout ([`layout`]), all read off a capture of
//!   NVIDIA's own driver decoding on the same RTX 3050 (nvdec-cap-1118,
//!   `nvdec-conventions.md`), and the decoded pictures read back by the copy
//!   engine, blitted block-linear to pitch-linear NV12 in system memory
//!   ([`copy`]), or, when it fails, over BAR1 and de-tiled on the CPU
//!   ([`surface`]).
//! - **The guest's decode sessions** ([`decoder`], the Metal gate's
//!   `DecoderOpen`/`DecoderDecode`, `metal::service::decoders`): a
//!   [`stream::Stream`] fed StelaVD's samples, a [`session::Session`]
//!   decoding each picture, and a [`copy::Copier`] reading each picture
//!   shown into the staging buffer the host copies into the guest's pixel
//!   buffers.
//!
//! None ever stops the boot: every refusal is a line and a value, waits
//! are bounded by the caller's clock, and a channel whose push never
//! finished is left open with its file.
use crate::nouveau_chip::{Architecture, Chip};
use crate::nvenc::layout::Geometry;
use crate::nvenc::{self, Log, Node, Outcome, PUSH_WORDS, Target};
use alloc::vec::Vec;

#[cfg(test)]
pub(crate) mod captured;
pub mod copy;
pub mod decoder;
pub mod dpb;
mod fields;
pub mod layout;
pub mod md5;
#[cfg(test)]
pub(crate) mod mock;
pub mod parse;
pub mod push;
pub mod selftest;
pub mod session;
pub mod setup;
pub mod stream;
pub mod surface;

use dpb::Output;
use parse::{h264, hevc};

/// `NVC7B0_VIDEO_DECODER`, GA10x's NVDEC (`clc7b0.h`). GA100's NVC6B0,
/// Turing's NVC4B0 and Ada's NVC9B0 are not driven.
pub const DECODER_CLASS: u32 = 0xc7b0;
/// What the probe's release writes: "NVDE" in ASCII.
pub const PAYLOAD: u32 = 0x4e56_4445;
/// The decoder's object handle on its channel.
pub const OBJECT_HANDLE: u32 = 0xc7b0_0001;

/// `NOUVEAU_FIFO_ENGINE_NVDEC`, Stela's abi16 engine (Linux patch 0208).
/// Provisional: the NVDEC engine queued upstream for 7.3 is not in Stela's
/// tree, so this is Stela's own value, the next free bit after
/// [`nvenc::FIFO_ENGINE_NVENC`], to be aligned with upstream's once seen.
pub const FIFO_ENGINE_NVDEC: u32 = 0x800;

/// The `clc7b0.h` methods the probe and the decoder send.
pub mod method {
    /// The host's `NV_UDMA_OBJECT`: binds the class to the subchannel.
    pub const SET_OBJECT: u32 = 0x0000;
    pub const NOP: u32 = 0x0100;
    pub const SET_APPLICATION_ID: u32 = 0x0200;
    pub const SET_WATCHDOG_TIMER: u32 = 0x0204;
    pub const SEMAPHORE_A: u32 = 0x0240;
    pub const SEMAPHORE_B: u32 = 0x0244;
    pub const SEMAPHORE_C: u32 = 0x0248;
    pub const EXECUTE: u32 = 0x0300;
    pub const SEMAPHORE_D: u32 = 0x0304;
    pub const SET_CONTROL_PARAMS: u32 = 0x0400;
    pub const SET_DRV_PIC_SETUP_OFFSET: u32 = 0x0404;
    pub const SET_IN_BUF_BASE_OFFSET: u32 = 0x0408;
    pub const SET_PICTURE_INDEX: u32 = 0x040c;
    pub const SET_SLICE_OFFSETS_BUF_OFFSET: u32 = 0x0410;
    pub const SET_COLOC_DATA_OFFSET: u32 = 0x0414;
    pub const SET_HISTORY_OFFSET: u32 = 0x0418;
    pub const SET_NVDEC_STATUS_OFFSET: u32 = 0x0424;
    pub const SET_PICTURE_LUMA_OFFSET0: u32 = 0x0430;
    pub const SET_PICTURE_CHROMA_OFFSET0: u32 = 0x0474;
    /// What NVIDIA's driver sends for HEVC at 0x4c8: the intra-prediction
    /// top row inside the filter buffer.
    pub const SET_INTRA_TOP_BUF_OFFSET: u32 = 0x04c8;
    pub const H264_SET_MBHIST_BUF_OFFSET: u32 = 0x0500;
    pub const HEVC_SET_SCALING_LIST_OFFSET: u32 = 0x0580;
    pub const HEVC_SET_TILE_SIZES_OFFSET: u32 = 0x0584;
    pub const HEVC_SET_FILTER_BUFFER_OFFSET: u32 = 0x0588;
}

// The probe's push is NVENC's: the decoder's methods must sit where the
// encoder's do.
const _: () = assert!(
    method::SET_OBJECT == nvenc::method::SET_OBJECT
        && method::SET_APPLICATION_ID == nvenc::method::SET_APPLICATION_ID
        && method::SEMAPHORE_A == nvenc::method::SEMAPHORE_A
        && method::SEMAPHORE_B == nvenc::method::SEMAPHORE_B
        && method::SEMAPHORE_C == nvenc::method::SEMAPHORE_C
        && method::SEMAPHORE_D == nvenc::method::SEMAPHORE_D
);

/// `NVC7B0_SET_APPLICATION_ID_ID_H264`.
pub const APPLICATION_H264: u32 = 3;
/// `NVC7B0_SET_APPLICATION_ID_ID_HEVC`.
pub const APPLICATION_HEVC: u32 = 7;

/// The decoder class of `chipset`, when this module knows it: the GA10x
/// parts with a display, GA102 to GA107, as NVENC's.
pub fn decoder_class(chipset: u16) -> Option<u32> {
    let chip = Chip::find(chipset)?;
    (chip.architecture == Architecture::Ampere && chip.display).then_some(DECODER_CLASS)
}

/// The decoder's probe (`--probe-nvdec`).
pub const PROBE: Target = Target {
    name: "NVDEC",
    engine: "decoder",
    class: decoder_class,
    parts: "GA102-GA107 only",
    fifo_engine: FIFO_ENGINE_NVDEC,
    patch: "0208",
    application: APPLICATION_H264,
    application_name: "H264",
    object_handle: OBJECT_HANDLE,
    payload: PAYLOAD,
};

/// The probe's push: bind the decoder, name H.264, and release `payload` to
/// the GPU address `semaphore`.
pub const fn push(semaphore: u64, payload: u32) -> [u32; PUSH_WORDS] {
    nvenc::probe_push(DECODER_CLASS, APPLICATION_H264, semaphore, payload)
}

/// Probes the decoder of `chipset` through `node`, waiting at most
/// `timeout_ns` for its semaphore, and says what it found to `log` as
/// `NVDEC PROBE` lines.
pub fn probe<N: Node>(node: &mut N, chipset: u16, timeout_ns: u64, log: Log<'_>) -> Outcome {
    nvenc::probe_engine(node, &PROBE, chipset, timeout_ns, log)
}

/// The codecs the decoder takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    /// A word for the lines.
    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }

    /// `SET_APPLICATION_ID`.
    pub fn application(self) -> u32 {
        match self {
            Self::H264 => APPLICATION_H264,
            Self::Hevc => APPLICATION_HEVC,
        }
    }
}

/// A codec's parsed picture. One is made per frame, so the H.264 variant's
/// size (its 16 reference places) costs nothing worth a box.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Parsed {
    H264(h264::Picture),
    Hevc(hevc::Picture),
}

/// One picture of a stream, parsed, in decoding order: what a session
/// decodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub codec: Codec,
    /// The slot it is decoded to: the picture index its setup names.
    pub slot: u8,
    /// The surface that slot is bound to, which the method stream names
    /// ([`dpb::Surfaces`]).
    pub surface: u8,
    /// Each slot's surface, as this picture is decoded.
    pub bindings: [u8; 16],
    /// The stream's surface slots.
    pub slots: u8,
    /// The highest slot used so far, this picture's included.
    pub high_water: u8,
    /// What the engine conceals a damaged picture from
    /// (`SET_CONTROL_PARAMS.ERROR_FRM_IDX`): H.264's last reference
    /// picture's slot, HEVC's first reference of list 0 (0 for the first
    /// picture or an intra one).
    pub previous_slot: u8,
    /// `I`, `P` or `B`.
    pub kind: char,
    pub idr: bool,
    /// Its picture order count (H.264: the top field's).
    pub poc: i32,
    /// The coded size and the displayed rectangle in it.
    pub coded: (u32, u32),
    pub crop: surface::Crop,
    /// Bits per sample: 8, or 10 for HEVC Main 10.
    pub bit_depth: u8,
    /// Which decode of the stream it is (0 for the first).
    pub decoded: u32,
    /// The pictures shown as it starts, in display order, maybe itself,
    /// with their surfaces: copy them out after its decode, before the
    /// next picture's ([`stream`]).
    pub outputs: Vec<Output>,
    pub parsed: Parsed,
}

impl Picture {
    pub(crate) fn from_h264(picture: h264::Picture) -> Self {
        let sps = &picture.sps;
        let (width, height) = sps.display_size();
        Self {
            codec: Codec::H264,
            slot: picture.slot,
            surface: picture.slot,
            bindings: core::array::from_fn(|k| k as u8),
            slots: picture.slots,
            high_water: picture.high_water,
            previous_slot: picture.previous_slot,
            kind: picture.kind,
            idr: picture.idr,
            poc: picture.poc[0],
            coded: (sps.coded_width(), sps.coded_height()),
            crop: surface::Crop {
                left: sps.crop.left,
                top: sps.crop.top,
                width,
                height,
            },
            bit_depth: sps.bit_depth_luma,
            decoded: picture.decoded,
            outputs: picture.outputs.clone(),
            parsed: Parsed::H264(picture),
        }
    }

    pub(crate) fn from_hevc(picture: hevc::Picture) -> Self {
        let sps = &picture.sps;
        let (width, height) = sps.display_size();
        Self {
            codec: Codec::Hevc,
            slot: picture.slot,
            surface: picture.slot,
            bindings: core::array::from_fn(|k| k as u8),
            slots: picture.slots,
            high_water: picture.high_water,
            previous_slot: picture.previous_slot,
            kind: picture.kind,
            idr: picture.idr,
            poc: picture.poc,
            coded: (sps.width, sps.height),
            crop: surface::Crop {
                left: sps.window.left,
                top: sps.window.top,
                width,
                height,
            },
            bit_depth: sps.bit_depth_luma,
            decoded: picture.decoded,
            outputs: picture.outputs.clone(),
            parsed: Parsed::Hevc(picture),
        }
    }

    /// Every picture of the Annex B `stream`, in decoding order, one
    /// access unit at a time through a [`stream::Stream`]. The last
    /// picture's [`Picture::outputs`] also lists what the end of the stream
    /// shows.
    pub fn all(codec: Codec, stream: &[u8]) -> Result<Vec<Self>, parse::Error> {
        let nals = parse::nal_units(stream);
        let units = match codec {
            Codec::H264 => h264::access_units(&nals)?,
            Codec::Hevc => hevc::access_units(&nals),
        };
        let mut decoder = stream::Stream::annex_b(codec);
        let mut pictures = Vec::new();
        for unit in &units {
            if let Some(picture) = decoder.nal_units(unit)? {
                pictures.push(picture);
            }
        }
        if let Some(last) = pictures.last_mut() {
            last.outputs.extend(decoder.flush());
        }
        Ok(pictures)
    }

    /// The slices as NVDEC reads them.
    pub fn bitstream(&self) -> &[u8] {
        match &self.parsed {
            Parsed::H264(p) => &p.bitstream,
            Parsed::Hevc(p) => &p.bitstream,
        }
    }

    /// H.264's slice offsets; none for HEVC.
    pub fn slice_offsets(&self) -> &[u32] {
        match &self.parsed {
            Parsed::H264(p) => &p.slice_offsets,
            Parsed::Hevc(_) => &[],
        }
    }

    /// The slices in the picture.
    pub fn slices(&self) -> usize {
        match &self.parsed {
            Parsed::H264(p) => p.slice_offsets.len(),
            Parsed::Hevc(p) => p.slices,
        }
    }

    /// The surfaces' geometry: the coded size, 16-aligned.
    pub fn geometry(&self) -> Result<Geometry, &'static str> {
        Geometry::new(
            self.coded.0.next_multiple_of(16),
            self.coded.1.next_multiple_of(16),
        )
    }

    /// The picture's setup for surfaces of `geometry`.
    pub fn setup(&self, geometry: &Geometry) -> Result<Vec<u8>, setup::Refused> {
        match &self.parsed {
            Parsed::H264(p) => setup::h264(p, geometry),
            Parsed::Hevc(p) => setup::hevc(p, geometry),
        }
    }

    /// The picture's whole setup page (4 KiB, the status area zero): the
    /// setup, and H.264's slice offsets or HEVC's scaling lists and tile
    /// sizes.
    pub fn page(&self, geometry: &Geometry) -> Result<Vec<u8>, setup::Refused> {
        match &self.parsed {
            Parsed::H264(p) => setup::h264_page(p, geometry),
            Parsed::Hevc(p) => setup::hevc_page(p, geometry),
        }
    }
}

/// The self-test's clips: NVENC's own boot-1 streams, 1920x1080, an IDR
/// and 29 P frames (`h264-qp25`, `hevc-qp25`).
pub const H264_CLIP: &[u8] = include_bytes!("nvdec/fixtures/h264-qp25.h264");
pub const HEVC_CLIP: &[u8] = include_bytes!("nvdec/fixtures/hevc-qp25.hevc");
const H264_GOLDEN: &str = include_str!("nvdec/fixtures/h264-qp25.md5");
const HEVC_GOLDEN: &str = include_str!("nvdec/fixtures/hevc-qp25.md5");

/// The golden per-frame checksums of a clip: FFmpeg's `framemd5` of its
/// software decode as NV12 (`sw-native.framemd5`), which NVIDIA's own NVDEC
/// decode matched frame for frame.
pub fn golden(codec: Codec) -> Vec<[u8; 16]> {
    let text = match codec {
        Codec::H264 => H264_GOLDEN,
        Codec::Hevc => HEVC_GOLDEN,
    };
    text.lines()
        .filter(|line| line.len() == 32)
        .map(|line| {
            let mut digest = [0u8; 16];
            for (k, byte) in digest.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&line[2 * k..2 * k + 2], 16).unwrap_or(0);
            }
            digest
        })
        .collect()
}

#[cfg(test)]
mod tests;
