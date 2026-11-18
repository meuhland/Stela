//! The parser, setups and method streams against NVIDIA's driver decoding
//! the same clips on the same GPU (nvdec-cap-1118): every frame's setup
//! page must come out byte for byte, and every frame's method stream word
//! for word.
//!
//! Two sources of NVIDIA's frames:
//!
//! - **`fixtures/capture/`**, checked in, all eight clips: each clip's
//!   `.truth` (every EXECUTE's 4 KiB setup page, its non-zero spans, the
//!   status area zeroed, and its methods from `SET_APPLICATION_ID` to
//!   `EXECUTE`, plus FFmpeg's `ulNumDecodeSurfaces`), and the clip itself
//!   with every slice NAL cut to its first 160 bytes (`.lens` holds each
//!   one's whole length, which the test pads it back to). The parser reads
//!   slice headers only, so the cut clip drives it as the whole one does:
//!   138 KB for 174 frames, where the clips are 2.7 MB. Made by
//!   `nvdec-tools/mkcapture.py --excerpt`.
//! - **`$NVDEC_CAPTURE`**, the whole clips and truths staged by
//!   `nvdec-tools/mkcapture.py` without `--excerpt`, for the ignored test.
//!
//! Each clip is driven as phase 2 drives a guest's stream: its parameter
//! sets become an `avcC`/`hvcC` record, each access unit a sample of
//! length-prefixed NAL units fed to a [`Stream`] one at a time.
extern crate std;

use super::layout::Layout;
use super::parse::{self, Nal, h264, hevc};
use super::push;
use super::stream::{self, Stream};
use super::{Codec, H264_CLIP, HEVC_CLIP, Picture, golden};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

/// One EXECUTE of NVIDIA's.
struct Frame {
    page: Vec<u8>,
    methods: Vec<(u32, u32)>,
}

/// A clip's `.truth`: FFmpeg's surface count and every EXECUTE.
struct Truth {
    pool: usize,
    frames: Vec<Frame>,
}

fn le(bytes: &[u8]) -> u32 {
    bytes.iter().rev().fold(0, |v, &b| v << 8 | u32::from(b))
}

fn truth(bytes: &[u8]) -> Truth {
    assert_eq!(&bytes[..4], b"NVDT");
    assert_eq!(le(&bytes[4..8]), 1);
    let pool = le(&bytes[8..12]) as usize;
    let count = le(&bytes[12..16]) as usize;
    let mut at = 16;
    let mut take = |n: usize| {
        at += n;
        &bytes[at - n..at]
    };
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        let mut page = vec![0u8; 4096];
        for _ in 0..le(take(2)) {
            let offset = le(take(2)) as usize;
            let length = le(take(2)) as usize;
            page[offset..offset + length].copy_from_slice(take(length));
        }
        let methods = (0..le(take(2)))
            .map(|_| (le(take(2)), le(take(4))))
            .collect();
        frames.push(Frame { page, methods });
    }
    Truth { pool, frames }
}

/// The cut clip `excerpt` with each slice NAL padded back to its length.
fn inflate(codec: Codec, excerpt: &[u8], lens: &[u8]) -> Vec<u8> {
    let mut lens = lens.chunks(4).map(|c| le(c) as usize);
    let mut out = Vec::new();
    for nal in parse::nal_units(excerpt) {
        let vcl = match codec {
            Codec::H264 => matches!(nal.h264_type(), 1 | 5),
            Codec::Hevc => nal.hevc_type() < 32,
        };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal.bytes);
        if vcl {
            let whole = lens.next().expect("a length per slice NAL");
            assert!(whole >= nal.bytes.len());
            out.resize(out.len() + whole - nal.bytes.len(), 0xa5);
        }
    }
    assert!(lens.next().is_none(), "a length left over");
    out
}

/// An Annex B clip as an `avcC`/`hvcC` record of its parameter sets and one
/// length-prefixed sample per access unit, without them.
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
            stream::sample(&rest)
        })
        .collect();
    (stream::record(codec, &sets), samples)
}

/// The byte offsets where `ours` and `theirs` differ, as a line.
fn differences(ours: &[u8], theirs: &[u8]) -> String {
    let mut text = String::new();
    for (at, (a, b)) in ours.iter().zip(theirs).enumerate() {
        if a != b {
            text.push_str(&format!(" +{at:#x}: ours {a:#04x} theirs {b:#04x};"));
        }
    }
    text
}

/// How a clip compared.
#[derive(Default)]
struct Compared {
    frames: usize,
    pictures: usize,
    setups: usize,
    pages: usize,
    methods: usize,
    first: Option<String>,
}

/// Drives `clip` through a [`Stream`] sample by sample and compares each
/// picture's setup page and method stream with NVIDIA's.
fn compare(codec: Codec, clip: &[u8], truth: &Truth) -> Result<Compared, parse::Error> {
    let size = match codec {
        Codec::H264 => super::fields::h264::SIZE,
        Codec::Hevc => super::fields::hevc::SIZE,
    };
    let (record, samples) = samples(codec, clip);
    let mut stream = Stream::open(codec, &record)?;
    let sequence = stream.sequence().expect("the record's sequence");
    let mut out = Compared {
        frames: truth.frames.len(),
        ..Compared::default()
    };
    let mut layout: Option<Layout> = None;
    for sample in &samples {
        let Some(picture) = stream.decode(sample)? else {
            continue;
        };
        let n = out.pictures;
        out.pictures += 1;
        let Some(theirs) = truth.frames.get(n) else {
            continue;
        };
        let geometry = picture.geometry().unwrap();
        let layout = layout.get_or_insert_with(|| {
            let slots = usize::from(sequence.surfaces).min(truth.pool);
            Layout::with(codec, &geometry, sequence.bit_depth, slots, truth.pool).unwrap()
        });
        match picture.page(&geometry) {
            Ok(ours) => {
                if ours[..size] == theirs.page[..size] {
                    out.setups += 1;
                }
                if ours == theirs.page {
                    out.pages += 1;
                } else {
                    out.first.get_or_insert(format!(
                        "frame {n} page:{}",
                        differences(&ours, &theirs.page)
                    ));
                }
            }
            Err(why) => {
                out.first
                    .get_or_insert(format!("frame {n}: setup refused: {}", why.0));
            }
        }
        let words = push::words(layout, &picture, n as u32, 0x1234, false);
        let ours: Vec<(u32, u32)> = push::decode(&words)
            .into_iter()
            .filter(|(m, _)| *m >= 0x200 && !matches!(m, 0x240 | 0x244 | 0x248 | 0x304))
            .collect();
        if ours == theirs.methods {
            out.methods += 1;
        } else {
            out.first.get_or_insert(format!(
                "frame {n} methods:\n ours   {ours:x?}\n theirs {:x?}",
                theirs.methods
            ));
        }
    }
    Ok(out)
}

/// Compares every clip of `clips` (name, codec, Annex B clip, truth) and
/// says how each did; whether all matched entirely.
fn report(clips: &[(String, Codec, Vec<u8>, Truth)]) -> bool {
    let mut all = true;
    for (name, codec, clip, truth) in clips {
        match compare(*codec, clip, truth) {
            Ok(c) => {
                std::println!(
                    "{name}: {}/{} setups, {}/{} pages, {}/{} method streams equal NVIDIA's \
                     ({} pictures parsed){}",
                    c.setups,
                    c.frames,
                    c.pages,
                    c.frames,
                    c.methods,
                    c.frames,
                    c.pictures,
                    c.first
                        .as_ref()
                        .map(|f| format!("; first difference {f}"))
                        .unwrap_or_default()
                );
                all &= c.pictures == c.frames
                    && c.setups == c.frames
                    && c.pages == c.frames
                    && c.methods == c.frames;
            }
            Err(error) => {
                std::println!("{name}: refused: {} ({})", error.describe(), error.kind());
                all = false;
            }
        }
    }
    all
}

macro_rules! excerpt {
    ($name:literal, $ext:literal, $codec:expr) => {
        (
            String::from($name),
            $codec,
            inflate(
                $codec,
                include_bytes!(concat!("fixtures/capture/", $name, ".", $ext)),
                include_bytes!(concat!("fixtures/capture/", $name, ".lens")),
            ),
            truth(include_bytes!(concat!(
                "fixtures/capture/",
                $name,
                ".truth"
            ))),
        )
    };
}

/// All eight clips, from the checked-in excerpt.
#[test]
fn every_captured_frame_is_nvidias() {
    let clips = [
        excerpt!("h264-cavlc-slices-720p", "h264", Codec::H264),
        excerpt!("h264-high-bf-1080p", "h264", Codec::H264),
        excerpt!("h264-high-cqm-720p", "h264", Codec::H264),
        excerpt!("h264-qp25", "h264", Codec::H264),
        excerpt!("hevc-main-bf-1080p", "hevc", Codec::Hevc),
        excerpt!("hevc-main-slices-1080p", "hevc", Codec::Hevc),
        excerpt!("hevc-main10-4k", "hevc", Codec::Hevc),
        excerpt!("hevc-qp25", "hevc", Codec::Hevc),
    ];
    assert!(report(&clips), "a captured frame differs (see above)");
}

/// The same over the whole clips: `NVDEC_CAPTURE=<dir> cargo test -p
/// stela-driver-gpu --lib every_whole_captured_clip -- --ignored
/// --nocapture`, the directory from `nvdec-tools/mkcapture.py`.
#[test]
#[ignore]
fn every_whole_captured_clip() {
    let Ok(dir) = std::env::var("NVDEC_CAPTURE") else {
        return;
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".h264") || name.ends_with(".hevc"))
        .collect();
    names.sort();
    let clips: Vec<(String, Codec, Vec<u8>, Truth)> = names
        .iter()
        .map(|name| {
            let (clip, ext) = name.rsplit_once('.').unwrap();
            let codec = if ext == "h264" {
                Codec::H264
            } else {
                Codec::Hevc
            };
            let stream = std::fs::read(format!("{dir}/{name}")).unwrap();
            let truth = truth(&std::fs::read(format!("{dir}/{clip}.truth")).unwrap());
            (String::from(clip), codec, stream, truth)
        })
        .collect();
    assert!(report(&clips), "a captured frame differs (see above)");
}

/// The self-test's clips with their real slices: the method streams
/// fixtures (nvdec-p1's `mkfixtures.py`) name each frame's `stream_len`.
fn methods_match(codec: Codec, captured: &str, pool: usize) {
    let pictures = Picture::all(
        codec,
        match codec {
            Codec::H264 => H264_CLIP,
            Codec::Hevc => HEVC_CLIP,
        },
    )
    .unwrap();
    let geometry = pictures[0].geometry().unwrap();
    let layout = Layout::with(codec, &geometry, 8, 5, pool).unwrap();
    let mut matched = 0;
    let mut report = String::new();
    let lines: Vec<&str> = captured.lines().collect();
    for (n, line) in lines.iter().enumerate() {
        let mut words = line.split_whitespace().skip(1);
        let len: usize = words
            .next()
            .unwrap()
            .strip_prefix("len=")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(pictures[n].bitstream().len(), len, "frame {n}'s stream_len");
        let theirs: Vec<(u32, u32)> = words
            .map(|pair| {
                let (m, v) = pair.split_once('=').unwrap();
                (
                    u32::from_str_radix(m, 16).unwrap(),
                    u32::from_str_radix(v, 16).unwrap(),
                )
            })
            .collect();
        let words = push::words(&layout, &pictures[n], n as u32, 0x1234, false);
        let ours: Vec<(u32, u32)> = push::decode(&words)
            .into_iter()
            .filter(|(m, _)| *m >= 0x200 && !matches!(m, 0x240 | 0x244 | 0x248 | 0x304))
            .collect();
        if ours == theirs {
            matched += 1;
        } else {
            report.push_str(&format!(
                "\nframe {n}:\n ours   {ours:x?}\n theirs {theirs:x?}"
            ));
        }
    }
    assert_eq!(matched, lines.len(), "{report}");
}

#[test]
fn the_self_test_clips_with_their_real_slices_are_nvidias() {
    let fixtures = [
        (
            Codec::H264,
            &include_bytes!("fixtures/h264-qp25.setups.bin")[..],
            super::fields::h264::SIZE,
        ),
        (
            Codec::Hevc,
            &include_bytes!("fixtures/hevc-qp25.setups.bin")[..],
            super::fields::hevc::SIZE,
        ),
    ];
    for (codec, setups, size) in fixtures {
        let clip = match codec {
            Codec::H264 => H264_CLIP,
            Codec::Hevc => HEVC_CLIP,
        };
        let pictures = Picture::all(codec, clip).unwrap();
        assert_eq!(pictures.len(), 30);
        for (n, picture) in pictures.iter().enumerate() {
            let ours = picture.setup(&picture.geometry().unwrap()).unwrap();
            let theirs = &setups[n * size..(n + 1) * size];
            assert!(
                ours == theirs,
                "{} frame {n}:{}",
                codec.name(),
                differences(&ours, theirs)
            );
        }
    }
    methods_match(
        Codec::H264,
        include_str!("fixtures/h264-qp25.methods.txt"),
        16,
    );
    methods_match(
        Codec::Hevc,
        include_str!("fixtures/hevc-qp25.methods.txt"),
        19,
    );
}

#[test]
fn the_golden_checksums_are_thirty_md5s_each() {
    for codec in [Codec::H264, Codec::Hevc] {
        let table = golden(codec);
        assert_eq!(table.len(), 30);
        assert!(table.iter().all(|d| d.len() == 16));
    }
    // h264-qp25's first frame, sw-native.framemd5 (NV12).
    assert_eq!(
        golden(Codec::H264)[0],
        [
            0x84, 0x7b, 0xd5, 0xc4, 0x0b, 0x38, 0x73, 0x14, 0x29, 0x8d, 0x9a, 0x88, 0x71, 0xd2,
            0x6a, 0x33
        ]
    );
}
