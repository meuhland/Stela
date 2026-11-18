//! The parser and setups against NVIDIA's driver decoding the same clips on
//! the same GPU (nvdec-cap-1118, `fixtures/`): every frame's setup must come
//! out byte for byte, and every frame's method stream word for word.
extern crate std;

use super::layout::Layout;
use super::parse::{h264, hevc};
use super::push;
use super::setup;
use super::{Codec, H264_CLIP, HEVC_CLIP, golden};
use crate::nvenc::layout::Geometry;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

const H264_SETUPS: &[u8] = include_bytes!("fixtures/h264-qp25.setups.bin");
const HEVC_SETUPS: &[u8] = include_bytes!("fixtures/hevc-qp25.setups.bin");
const H264_METHODS: &str = include_str!("fixtures/h264-qp25.methods.txt");
const HEVC_METHODS: &str = include_str!("fixtures/hevc-qp25.methods.txt");

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

/// One frame's methods as the fixture holds them: `len` and (method, data).
fn captured_methods(text: &str) -> Vec<(usize, Vec<(u32, u32)>)> {
    text.lines()
        .map(|line| {
            let mut words = line.split_whitespace().skip(1);
            let len = words
                .next()
                .unwrap()
                .strip_prefix("len=")
                .unwrap()
                .parse()
                .unwrap();
            let methods = words
                .map(|pair| {
                    let (m, v) = pair.split_once('=').unwrap();
                    (
                        u32::from_str_radix(m, 16).unwrap(),
                        u32::from_str_radix(v, 16).unwrap(),
                    )
                })
                .collect();
            (len, methods)
        })
        .collect()
}

fn geometry() -> Geometry {
    Geometry::new(1920, 1088).unwrap()
}

#[test]
fn every_h264_setup_is_nvidias() {
    let pictures = h264::pictures(H264_CLIP).expect("the clip parses");
    assert_eq!(pictures.len(), 30);
    let size = super::fields::h264::SIZE;
    let mut matched = 0;
    let mut report = String::new();
    for (n, picture) in pictures.iter().enumerate() {
        let ours = setup::h264(picture, &geometry()).unwrap();
        let theirs = &H264_SETUPS[n * size..(n + 1) * size];
        if ours == theirs {
            matched += 1;
        } else {
            report.push_str(&format!("\nframe {n}:{}", differences(&ours, theirs)));
        }
    }
    std::println!("h264-qp25: {matched}/30 setups equal NVIDIA's");
    assert_eq!(matched, 30, "{report}");
}

#[test]
fn every_hevc_setup_is_nvidias() {
    let pictures = hevc::pictures(HEVC_CLIP).expect("the clip parses");
    assert_eq!(pictures.len(), 30);
    let size = super::fields::hevc::SIZE;
    let mut matched = 0;
    let mut report = String::new();
    for (n, picture) in pictures.iter().enumerate() {
        let ours = setup::hevc(picture, &geometry()).unwrap();
        let theirs = &HEVC_SETUPS[n * size..(n + 1) * size];
        if ours == theirs {
            matched += 1;
        } else {
            report.push_str(&format!("\nframe {n}:{}", differences(&ours, theirs)));
        }
        assert_eq!(
            setup::hevc_tile_sizes(&picture.sps),
            0x0022_003cu32.to_le_bytes()
        );
    }
    std::println!("hevc-qp25: {matched}/30 setups equal NVIDIA's");
    assert_eq!(matched, 30, "{report}");
}

/// The method stream of each frame, between `SET_APPLICATION_ID` and
/// `EXECUTE`, must be NVIDIA's: same methods, same order, same data.
fn methods_match(codec: Codec, captured: &str, stream_lens: impl Iterator<Item = usize>) {
    let frames = captured_methods(captured);
    let mut matched = 0;
    let mut report = String::new();
    let mut lens = stream_lens;
    let pictures = super::Picture::all(
        codec,
        match codec {
            Codec::H264 => H264_CLIP,
            Codec::Hevc => HEVC_CLIP,
        },
    )
    .unwrap();
    let layout = Layout::new(codec, &geometry(), usize::from(pictures[0].slots)).unwrap();
    for (n, (len, theirs)) in frames.iter().enumerate() {
        assert_eq!(lens.next(), Some(*len), "frame {n}'s stream_len");
        let words = push::words(&layout, &pictures[n], n as u32, 0x1234, false);
        let ours: Vec<(u32, u32)> = push::decode(&words)
            .into_iter()
            .filter(|(m, _)| *m >= 0x200 && !matches!(m, 0x240 | 0x244 | 0x248 | 0x304))
            .collect();
        if ours == *theirs {
            matched += 1;
        } else {
            report.push_str(&format!(
                "\nframe {n}:\n ours   {ours:x?}\n theirs {theirs:x?}"
            ));
        }
    }
    std::println!(
        "{}: {matched}/{} method streams equal NVIDIA's",
        codec.name(),
        frames.len()
    );
    assert_eq!(matched, frames.len(), "{report}");
}

#[test]
fn every_h264_method_stream_is_nvidias() {
    let lens = h264::pictures(H264_CLIP)
        .unwrap()
        .into_iter()
        .map(|p| p.bitstream.len());
    methods_match(
        Codec::H264,
        H264_METHODS,
        lens.collect::<Vec<_>>().into_iter(),
    );
}

#[test]
fn every_hevc_method_stream_is_nvidias() {
    let lens = hevc::pictures(HEVC_CLIP)
        .unwrap()
        .into_iter()
        .map(|p| p.bitstream.len());
    methods_match(
        Codec::Hevc,
        HEVC_METHODS,
        lens.collect::<Vec<_>>().into_iter(),
    );
}

/// Every clip of the capture, not just the self-test's two: how many of
/// NVIDIA's setups the parser gives byte for byte, and the fields that
/// differ. Ignored: it reads `$NVDEC_CAPTURE/<clip>.{h264,hevc}` and
/// `<clip>.setups.bin` (nvdec-p1's `mkall.py`), which the tree does not
/// carry. `NVDEC_CAPTURE=... cargo test -p stela-driver-gpu --lib
/// every_captured_clip -- --ignored --nocapture`.
#[test]
#[ignore]
fn every_captured_clip() {
    let Ok(dir) = std::env::var("NVDEC_CAPTURE") else {
        return;
    };
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".h264") || name.ends_with(".hevc"))
        .collect();
    names.sort();
    for name in names {
        let (clip, ext) = name.rsplit_once('.').unwrap();
        let stream = std::fs::read(format!("{dir}/{name}")).unwrap();
        let theirs = std::fs::read(format!("{dir}/{clip}.setups.bin")).unwrap();
        let (codec, size) = match ext {
            "h264" => (Codec::H264, super::fields::h264::SIZE),
            _ => (Codec::Hevc, super::fields::hevc::SIZE),
        };
        let frames = theirs.len() / size;
        let pictures = match super::Picture::all(codec, &stream) {
            Ok(pictures) => pictures,
            Err(error) => {
                std::println!("{clip}: refused: {} ({})", error.describe(), error.kind());
                continue;
            }
        };
        let mut matched = 0;
        let mut first = None;
        for (n, picture) in pictures.iter().enumerate().take(frames) {
            let geometry = picture.geometry().unwrap();
            let ours = match picture.setup(&geometry) {
                Ok(ours) => ours,
                Err(why) => {
                    first.get_or_insert(format!("frame {n}: setup refused: {}", why.0));
                    continue;
                }
            };
            let them = &theirs[n * size..(n + 1) * size];
            if ours == them {
                matched += 1;
            } else {
                first.get_or_insert(format!("frame {n}:{}", differences(&ours, them)));
            }
        }
        std::println!(
            "{clip}: {matched}/{frames} setups equal NVIDIA's ({} pictures parsed){}",
            pictures.len(),
            first
                .map(|f| format!("; first difference {f}"))
                .unwrap_or_default()
        );
    }
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
    let _ = vec![0u8; 0];
}
