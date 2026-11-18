//! A stream fed one sample at a time, as a guest's VideoToolbox session
//! feeds it: MP4-style samples of length-prefixed NAL units, described by
//! the `avcC` or `hvcC` record of the format description (phase 2's
//! `DecoderOpen`/`DecoderDecode`), or Annex B access units.
//!
//! Each [`Stream::decode`] parses one access unit and returns its
//! [`Picture`]: the setup inputs, the surface slot to decode into, and
//! [`Picture::outputs`], the pictures that may be shown once it is decoded,
//! in display order. The parser keeps the state across samples (parameter
//! sets, references, the pictures waiting to be shown, the free slots);
//! an IDR (or an IRAP that starts afresh) flushes the references, and
//! [`Stream::reset`] forgets every picture for a seek.
//!
//! **Slot and surface lifetime.** A slot is busy from the picture decoded
//! into it until it is neither a reference nor waiting to be shown; each
//! slot is bound to a surface ([`super::dpb::Surfaces`]). A picture's
//! [`Picture::outputs`] are the pictures shown as it starts, maybe itself,
//! each with its surface: none of those surfaces is written by the
//! picture's decode (a slot just given back moves to another surface), so
//! the caller copies them out once the picture is decoded and before it
//! submits the next one. [`Stream::held_slots`] says which slots are busy
//! now, and [`Stream::flush`] lists the pictures still waiting at the end
//! of the stream (their slots stay busy until a reset).
use super::dpb::{Output, Surfaces};
use super::parse::{self, Error, Nal, h264, hevc};
use super::{Codec, Picture};
use alloc::vec::Vec;

/// How the samples carry their NAL units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes (`00 00 01`, `00 00 00 01`).
    AnnexB,
    /// A big-endian length of 1, 2 or 4 bytes before each NAL unit.
    Length(u8),
}

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
enum Parser {
    H264(h264::Parser),
    Hevc(hevc::Parser),
}

/// What a session needs to know of the stream before its first picture:
/// from the record's sequence parameter set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence {
    pub codec: Codec,
    /// The coded size (H.264: macroblocks; HEVC: the SPS's luma samples).
    pub coded: (u32, u32),
    /// The displayed size, the coded one cropped.
    pub display: (u32, u32),
    pub bit_depth: u8,
    /// The slots the stream may hold at once.
    pub slots: u8,
    /// The surfaces those slots may be bound to: twice as many, since a
    /// slot moves off a surface being shown ([`Surfaces`]), at most 32.
    pub surfaces: u8,
}

/// A stream of samples of one codec.
#[derive(Clone, Debug)]
pub struct Stream {
    codec: Codec,
    framing: Framing,
    parser: Parser,
    sequence: Option<Sequence>,
    surfaces: Surfaces,
}

fn h264_sequence(sps: &h264::Sps) -> Sequence {
    Sequence {
        codec: Codec::H264,
        coded: (sps.coded_width(), sps.coded_height()),
        display: sps.display_size(),
        bit_depth: sps.bit_depth_luma,
        slots: sps.slots(),
        surfaces: surfaces(sps.slots()),
    }
}

/// The surfaces `slots` may be bound to.
fn surfaces(slots: u8) -> u8 {
    (2 * slots).min(32)
}

fn hevc_sequence(sps: &hevc::Sps) -> Sequence {
    Sequence {
        codec: Codec::Hevc,
        coded: (sps.width, sps.height),
        display: sps.display_size(),
        bit_depth: sps.bit_depth_luma,
        slots: sps.slots(),
        surfaces: surfaces(sps.slots()),
    }
}

/// A big-endian integer of `bytes` bytes.
fn be(data: &[u8]) -> usize {
    data.iter().fold(0, |value, &b| value << 8 | usize::from(b))
}

/// The NAL units of a length-prefixed sample.
pub fn length_prefixed(sample: &[u8], length_size: u8) -> parse::Result<Vec<Nal<'_>>> {
    let size = usize::from(length_size);
    if !matches!(size, 1 | 2 | 4) {
        return Err(Error::Invalid("a NAL length size other than 1, 2 or 4"));
    }
    let mut nals = Vec::new();
    let mut at = 0;
    while at < sample.len() {
        let length = be(sample.get(at..at + size).ok_or(Error::Truncated)?);
        at += size;
        let bytes = sample.get(at..at + length).ok_or(Error::Truncated)?;
        if !bytes.is_empty() {
            nals.push(Nal { bytes, offset: at });
        }
        at += length;
    }
    Ok(nals)
}

/// A reader of a configuration record's bytes.
struct Record<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Record<'a> {
    fn take(&mut self, bytes: usize) -> parse::Result<&'a [u8]> {
        let out = self
            .data
            .get(self.at..self.at + bytes)
            .ok_or(Error::Truncated)?;
        self.at += bytes;
        Ok(out)
    }

    fn byte(&mut self) -> parse::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn word(&mut self) -> parse::Result<usize> {
        Ok(be(self.take(2)?))
    }
}

impl Stream {
    /// A stream of Annex B access units.
    pub fn annex_b(codec: Codec) -> Self {
        Self {
            codec,
            framing: Framing::AnnexB,
            parser: match codec {
                Codec::H264 => Parser::H264(h264::Parser::new()),
                Codec::Hevc => Parser::Hevc(hevc::Parser::new()),
            },
            sequence: None,
            surfaces: Surfaces::new(0),
        }
    }

    /// A stream of length-prefixed samples that `record` describes: an
    /// `avcC` (ISO/IEC 14496-15 5.3.3.1) for H.264, an `hvcC` (8.3.3.1)
    /// for HEVC. Its parameter sets are taken now.
    pub fn open(codec: Codec, record: &[u8]) -> parse::Result<Self> {
        let mut stream = Self::annex_b(codec);
        let mut r = Record {
            data: record,
            at: 0,
        };
        if r.byte()? != 1 {
            return Err(Error::Unsupported(
                "a configuration record version other than 1",
            ));
        }
        let mut sets: Vec<&[u8]> = Vec::new();
        let length_size = match codec {
            Codec::H264 => {
                r.take(3)?; // profile, compatibility, level
                let length_size = (r.byte()? & 3) + 1;
                let count = r.byte()? & 0x1f;
                for _ in 0..count {
                    let length = r.word()?;
                    sets.push(r.take(length)?);
                }
                let count = r.byte()?;
                for _ in 0..count {
                    let length = r.word()?;
                    sets.push(r.take(length)?);
                }
                length_size
            }
            Codec::Hevc => {
                r.take(20)?; // profile, level, segmentation, format, rate
                let length_size = (r.byte()? & 3) + 1;
                let arrays = r.byte()?;
                for _ in 0..arrays {
                    let kind = r.byte()? & 0x3f;
                    let count = r.word()?;
                    for _ in 0..count {
                        let length = r.word()?;
                        let nal = r.take(length)?;
                        if matches!(kind, hevc::NAL_VPS | hevc::NAL_SPS | hevc::NAL_PPS) {
                            sets.push(nal);
                        }
                    }
                }
                length_size
            }
        };
        if length_size == 3 {
            return Err(Error::Invalid("a NAL length size of 3"));
        }
        stream.framing = Framing::Length(length_size);
        for bytes in sets {
            stream.parameter_set(&Nal { bytes, offset: 0 })?;
        }
        if stream.sequence.is_none() {
            return Err(Error::Missing("the record has no sequence parameter set"));
        }
        Ok(stream)
    }

    fn parameter_set(&mut self, nal: &Nal<'_>) -> parse::Result<()> {
        match &mut self.parser {
            Parser::H264(parser) => {
                parser.parameter_set(nal)?;
                if nal.h264_type() == h264::NAL_SPS && self.sequence.is_none() {
                    self.sequence = Some(h264_sequence(&h264::sps(&nal.bytes[1..])?));
                }
            }
            Parser::Hevc(parser) => {
                parser.parameter_set(nal)?;
                if nal.hevc_type() == hevc::NAL_SPS && self.sequence.is_none() {
                    self.sequence = Some(hevc_sequence(&hevc::sps(&nal.bytes[2..])?));
                }
            }
        }
        Ok(())
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// The stream's sequence: the record's, else its first picture's.
    pub fn sequence(&self) -> Option<Sequence> {
        self.sequence
    }

    /// Decodes one sample: one access unit, in the stream's framing. Its
    /// picture, or none for a sample of parameter sets or SEI alone.
    pub fn decode(&mut self, sample: &[u8]) -> parse::Result<Option<Picture>> {
        let nals = match self.framing {
            Framing::AnnexB => parse::nal_units(sample),
            Framing::Length(size) => length_prefixed(sample, size)?,
        };
        self.nal_units(&nals)
    }

    /// Decodes one access unit given as its NAL units.
    pub fn nal_units(&mut self, nals: &[Nal<'_>]) -> parse::Result<Option<Picture>> {
        let units = match self.codec {
            Codec::H264 => h264::access_units(nals)?,
            Codec::Hevc => hevc::access_units(nals),
        };
        let mut picture = None;
        for unit in &units {
            let parsed = match &mut self.parser {
                Parser::H264(parser) => parser.access_unit(unit)?.map(Picture::from_h264),
                Parser::Hevc(parser) => parser.access_unit(unit)?.map(Picture::from_hevc),
            };
            if let Some(parsed) = parsed {
                if picture.is_some() {
                    return Err(Error::Unsupported("a sample of more than one picture"));
                }
                picture = Some(self.bind(parsed)?);
            }
        }
        if self.sequence.is_none() {
            self.sequence = match &self.parser {
                Parser::H264(parser) => parser.active_sps().map(h264_sequence),
                Parser::Hevc(parser) => parser.active_sps().map(hevc_sequence),
            };
        }
        Ok(picture)
    }

    /// Binds `picture`'s slot to a surface, and names the surface of each
    /// picture it shows: those shown before it is decoded keep theirs, so
    /// its slot moves off a surface one of them is in.
    fn bind(&mut self, mut picture: Picture) -> parse::Result<Picture> {
        self.surfaces.widen(surfaces(picture.slots));
        let current = picture.decoded;
        let mut showing = Vec::new();
        for output in &mut picture.outputs {
            if output.decoded != current {
                output.surface = self.surfaces.of(output.slot).unwrap_or(output.slot);
                showing.push(output.surface);
            }
        }
        let surface = self
            .surfaces
            .bind(picture.slot, &showing)
            .ok_or(Error::Invalid("every surface holds a picture"))?;
        for output in &mut picture.outputs {
            if output.decoded == current {
                output.surface = surface;
            }
        }
        picture.surface = surface;
        picture.bindings = self.surfaces.table();
        Ok(picture)
    }

    /// The pictures still waiting to be shown, in display order: what an
    /// end of stream shows.
    pub fn flush(&mut self) -> Vec<Output> {
        let mut outputs = match &mut self.parser {
            Parser::H264(parser) => parser.flush(),
            Parser::Hevc(parser) => parser.flush(),
        };
        for output in &mut outputs {
            output.surface = self.surfaces.of(output.slot).unwrap_or(output.slot);
        }
        outputs
    }

    /// Forgets every picture, as for a seek: every slot is free, nothing
    /// waits to be shown, and the next picture must be an IDR (H.264) or
    /// an IRAP (HEVC). The parameter sets stay.
    pub fn reset(&mut self) {
        match &mut self.parser {
            Parser::H264(parser) => parser.reset(),
            Parser::Hevc(parser) => parser.reset(),
        }
        self.surfaces = Surfaces::new(self.surfaces.most());
    }

    /// The slots busy now: references and pictures waiting to be shown.
    pub fn held_slots(&self) -> Vec<u8> {
        match &self.parser {
            Parser::H264(parser) => parser.held_slots(),
            Parser::Hevc(parser) => parser.held_slots(),
        }
    }
}

/// An `avcC` or `hvcC` record of NAL length size 4 holding `sets` (each a
/// NAL unit without start code), for tests and tools that turn an Annex B
/// stream into samples.
pub fn record(codec: Codec, sets: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    match codec {
        Codec::H264 => {
            let sps: Vec<&&[u8]> = sets.iter().filter(|s| s[0] & 0x1f == 7).collect();
            let pps: Vec<&&[u8]> = sets.iter().filter(|s| s[0] & 0x1f == 8).collect();
            let first = sps.first().map_or(&[0u8, 0, 0, 0][..], |s| &s[..]);
            out.extend_from_slice(&[1, first[1], first[2], first[3], 0xff]);
            out.push(0xe0 | sps.len() as u8);
            for set in sps {
                out.extend_from_slice(&(set.len() as u16).to_be_bytes());
                out.extend_from_slice(set);
            }
            out.push(pps.len() as u8);
            for set in pps {
                out.extend_from_slice(&(set.len() as u16).to_be_bytes());
                out.extend_from_slice(set);
            }
        }
        Codec::Hevc => {
            out.push(1);
            out.extend_from_slice(&[0; 20]);
            out.push(0x0f); // lengthSizeMinusOne 3
            let kinds = [hevc::NAL_VPS, hevc::NAL_SPS, hevc::NAL_PPS];
            out.push(kinds.len() as u8);
            for kind in kinds {
                let of_kind: Vec<&&[u8]> =
                    sets.iter().filter(|s| (s[0] >> 1) & 0x3f == kind).collect();
                out.push(0x80 | kind);
                out.extend_from_slice(&(of_kind.len() as u16).to_be_bytes());
                for set in of_kind {
                    out.extend_from_slice(&(set.len() as u16).to_be_bytes());
                    out.extend_from_slice(set);
                }
            }
        }
    }
    out
}

/// A sample of `nals`, each behind a 4-byte big-endian length.
pub fn sample(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvdec::{H264_CLIP, HEVC_CLIP};

    /// The clip as a record of its parameter sets and one sample per
    /// access unit, without them.
    fn samples(codec: Codec, clip: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let nals = parse::nal_units(clip);
        let is_set = |nal: &Nal<'_>| match codec {
            Codec::H264 => matches!(nal.h264_type(), 7 | 8),
            Codec::Hevc => matches!(nal.hevc_type(), 32..=34),
        };
        let sets: Vec<&[u8]> = nals.iter().filter(|n| is_set(n)).map(|n| n.bytes).collect();
        let units = match codec {
            Codec::H264 => h264::access_units(&nals).unwrap(),
            Codec::Hevc => hevc::access_units(&nals),
        };
        let samples = units
            .iter()
            .map(|unit| {
                let rest: Vec<&[u8]> = unit
                    .iter()
                    .filter(|n| !is_set(n))
                    .map(|n| n.bytes)
                    .collect();
                sample(&rest)
            })
            .collect();
        (record(codec, &sets), samples)
    }

    #[test]
    fn samples_decode_as_the_annex_b_clip_does() {
        for (codec, clip) in [(Codec::H264, H264_CLIP), (Codec::Hevc, HEVC_CLIP)] {
            let whole = Picture::all(codec, clip).unwrap();
            let (record, samples) = samples(codec, clip);
            let mut stream = Stream::open(codec, &record).unwrap();
            assert_eq!(stream.framing(), Framing::Length(4));
            let sequence = stream.sequence().unwrap();
            assert_eq!(sequence.coded.0, 1920);
            assert_eq!(sequence.bit_depth, 8);
            let mut pictures = Vec::new();
            for sample in &samples {
                if let Some(picture) = stream.decode(sample).unwrap() {
                    pictures.push(picture);
                }
            }
            assert_eq!(pictures.len(), whole.len());
            for (ours, theirs) in pictures.iter().zip(&whole) {
                assert_eq!(ours.slot, theirs.slot);
                assert_eq!(ours.bitstream(), theirs.bitstream());
            }
            // Picture::all adds the end of stream's outputs to the last.
            let last = pictures.len() - 1;
            for (ours, theirs) in pictures[..last].iter().zip(&whole) {
                assert_eq!(ours.outputs, theirs.outputs);
            }
            // P-only clips: each picture is shown as it is decoded.
            let shown: Vec<u32> = pictures
                .iter()
                .flat_map(|p| p.outputs.iter().map(|o| o.decoded))
                .chain(stream.flush().iter().map(|o| o.decoded))
                .collect();
            assert_eq!(shown, (0..30).collect::<Vec<u32>>());
        }
    }

    #[test]
    fn a_reset_frees_every_slot_and_waits_for_an_idr() {
        let (record, samples) = samples(Codec::H264, H264_CLIP);
        let mut stream = Stream::open(Codec::H264, &record).unwrap();
        for sample in &samples[..5] {
            stream.decode(sample).unwrap();
        }
        assert!(!stream.held_slots().is_empty());
        stream.reset();
        assert!(stream.held_slots().is_empty());
        assert_eq!(
            stream.decode(&samples[5]),
            Err(Error::Missing("the stream does not start with an IDR"))
        );
        let first = stream.decode(&samples[0]).unwrap().unwrap();
        assert!(first.idr);
        assert_eq!(first.slot, 0);
    }

    #[test]
    fn broken_records_and_samples_are_refused() {
        assert_eq!(Stream::open(Codec::H264, &[]).err(), Some(Error::Truncated));
        assert_eq!(
            Stream::open(Codec::H264, &[1, 100, 0, 40, 0xff, 0xe0, 0]).err(),
            Some(Error::Missing("the record has no sequence parameter set"))
        );
        assert_eq!(
            length_prefixed(&[0, 0, 0, 9, 1, 2], 4).err(),
            Some(Error::Truncated)
        );
        assert_eq!(
            length_prefixed(&[0, 2, 0x65, 1], 2).unwrap()[0].bytes,
            &[0x65, 1]
        );
    }
}
