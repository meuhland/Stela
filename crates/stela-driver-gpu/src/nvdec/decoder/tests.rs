use super::*;
use crate::nvdec::mock::{self, Ce, Engine, Gpu};
use crate::nvdec::stream;
use crate::nvdec::surface::Tiling;
use crate::nvdec::{H264_CLIP, captured};
use alloc::format;
use alloc::string::String;
use alloc::vec;

/// GA106, the RTX 3050.
const GA106: u16 = 0x176;
const BOUND_NS: u64 = 1_000_000_000;

impl<N: Node> Decoder<N> {
    fn node(&mut self) -> &mut N {
        self.session.node()
    }
}

/// A decoder of `clip` on a mock GPU that writes pictures of its size,
/// every piece made.
fn open(codec: Codec, record: &[u8], ce: Ce) -> Decoder<Gpu> {
    let gpu = Gpu::new(Engine::Decode(Tiling::NVDEC)).with_ce(ce);
    let mut decoder = Decoder::begin(gpu, GA106, codec, record, BOUND_NS)
        .map_err(|(error, _)| error)
        .unwrap();
    let pitch = decoder.layout().format.pitch;
    let rows = decoder.session.geometry().coded_height;
    let gpu = decoder.node();
    gpu.size = (pitch, rows);
    gpu.check_conceal = false;
    let mut steps = 0;
    while !decoder.bring_up_step().unwrap() {
        steps += 1;
    }
    assert_eq!(
        steps + 1,
        decoder.made().1 - 1,
        "one step a buffer, the copier last"
    );
    decoder
}

/// The displayed picture the mock decodes as its `index`th, as the
/// staging buffer holds it.
fn expected(decoder: &Decoder<Gpu>, index: u32, crop: &Crop) -> Vec<u8> {
    let (pitch, rows) = (
        decoder.layout().format.pitch,
        decoder.session.geometry().coded_height,
    );
    let (luma, chroma) = mock::picture(index, pitch, rows);
    let sample = decoder.format.sample_bytes() as usize;
    let (left, width) = (crop.left as usize * sample, crop.width as usize * sample);
    let mut out = Vec::new();
    for y in 0..crop.height as usize {
        let at = (crop.top as usize + y) * pitch as usize + left;
        out.extend_from_slice(&luma[at..at + width]);
    }
    for y in 0..crop.height as usize / 2 {
        let at = (crop.top as usize / 2 + y) * pitch as usize + left;
        out.extend_from_slice(&chroma[at..at + width]);
    }
    out
}

/// Shows every queued picture, checking each against the mock's, and
/// gives back (token, decoded) of each in order.
fn drain(decoder: &mut Decoder<Gpu>, crop: &Crop) -> Vec<(u64, u32)> {
    let mut shown = Vec::new();
    while decoder.queued() > 0 {
        let head = decoder.head().unwrap();
        let mut looks = 0;
        while decoder.stage(true).unwrap().is_none() {
            looks += 1;
            assert!(looks < 10);
        }
        let planes = decoder.staged().unwrap();
        let mut got = planes.luma.to_vec();
        got.extend_from_slice(planes.chroma);
        assert_eq!(planes.luma_rows, crop.height as usize);
        assert!(
            got == expected(decoder, head.decoded, crop),
            "picture {} differs",
            head.decoded
        );
        shown.push((head.token, head.decoded));
        decoder.pop();
    }
    shown
}

/// Every captured clip (H.264 with B-frames, CQM and CAVLC slices; HEVC
/// with B-frames, slices and Main 10 4K) decodes sample by sample: each
/// picture is shown once, in the stream's display order, with the token of
/// the sample that decoded it, and its bytes are the picture the engine
/// wrote, cropped, as NV12 or P010.
#[test]
fn every_captured_clip_is_shown_in_display_order_with_its_tokens() {
    for (name, codec, clip) in captured::clips() {
        let (record, samples) = stream::samples(codec, &clip).unwrap();
        let mut decoder = open(codec, &record, Ce::Copies);
        let opened = decoder.opened();
        assert_eq!(
            opened.format,
            if name.contains("main10") {
                Format::P010
            } else {
                Format::Nv12
            },
            "{name}"
        );
        // The display order Picture::all gives, decode indices.
        let order: Vec<u32> = Picture::all(codec, &clip)
            .unwrap()
            .iter()
            .flat_map(|p| p.outputs.iter().map(|o| o.decoded))
            .collect();
        let mut tokens = Vec::new();
        let mut shown = Vec::new();
        for (n, sample) in samples.iter().enumerate() {
            let token = 1000 + n as u64;
            if !decoder.submit(sample, token).unwrap() {
                continue;
            }
            tokens.push(token);
            assert_eq!(
                decoder.submit(sample, token),
                Err(Error::Order("a picture is in flight"))
            );
            let decoded = decoder
                .poll(true)
                .unwrap()
                .expect("the mock decodes at once");
            assert!(decoded.status.clean());
            let crop = decoder.crop;
            if decoder.queued() > 0 {
                assert_eq!(
                    decoder.submit(sample, token),
                    Err(Error::Order("pictures wait to be shown"))
                );
            }
            shown.extend(drain(&mut decoder, &crop));
        }
        decoder.flush().unwrap();
        let crop = decoder.crop;
        shown.extend(drain(&mut decoder, &crop));
        let decoded: Vec<u32> = shown.iter().map(|s| s.1).collect();
        assert_eq!(decoded, order, "{name}: display order");
        for (token, index) in &shown {
            assert_eq!(*token, tokens[*index as usize], "{name}: token of {index}");
        }
        assert_eq!(shown.len(), tokens.len(), "{name}: each picture once");
        let closed = decoder.close();
        assert!(!closed.keeps_file, "{name}");
        assert!(closed.node.channel_freed && closed.node.ce_freed, "{name}");
        assert_eq!(closed.node.mapped, closed.node.unmapped, "{name}");
    }
}

/// The session's surfaces are not mapped for the CPU (BAR1's room), its
/// copy ring sits past them, and 4K Main 10's fit the 1 GiB budget.
#[test]
fn surfaces_stay_off_the_cpu_and_the_copy_ring_past_them() {
    for (name, codec, clip) in captured::clips() {
        let (record, _) = stream::samples(codec, &clip).unwrap();
        let decoder = open(codec, &record, Ce::Copies);
        let layout = decoder.layout();
        for &index in &layout.surfaces {
            assert!(!layout.plans[index].mapped(), "{name}");
        }
        assert!(layout.vram_bytes() <= VRAM_BUDGET, "{name}");
        assert!(decoder.ring >= layout.end(), "{name}");
        let _ = decoder.close();
    }
}

/// A reset forgets what waits to be shown and the next sample must start
/// afresh; the stream then goes on.
#[test]
fn a_reset_drops_the_queue_and_waits_for_an_idr() {
    let (record, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let mut decoder = open(Codec::H264, &record, Ce::Copies);
    for (n, sample) in samples[..3].iter().enumerate() {
        decoder.submit(sample, n as u64).unwrap();
        decoder.poll(true).unwrap().unwrap();
        let crop = decoder.crop;
        drain(&mut decoder, &crop);
    }
    decoder.reset().unwrap();
    assert_eq!(decoder.queued(), 0);
    assert!(matches!(
        decoder.submit(&samples[5], 5),
        Err(Error::Parse(_))
    ));
    assert!(decoder.submit(&samples[0], 7).unwrap());
    decoder.poll(true).unwrap().unwrap();
    assert_eq!(decoder.head().unwrap().token, 7);
}

/// A copy that never lands is the engine lost: the decoder says so, and
/// its file stays open with the busy copy channel.
#[test]
fn a_hung_copy_is_lost_and_keeps_the_file() {
    let (record, samples) = stream::samples(Codec::H264, H264_CLIP).unwrap();
    let mut decoder = open(Codec::H264, &record, Ce::Hangs);
    decoder.submit(&samples[0], 1).unwrap();
    decoder.poll(true).unwrap().unwrap();
    let error = loop {
        match decoder.stage(true) {
            Ok(None) => decoder.node().pause(),
            Ok(Some(_)) => panic!("a hung copy landed"),
            Err(error) => break error,
        }
    };
    assert!(error.lost() && error.pending(), "{error:?}");
    let closed = decoder.close();
    assert!(closed.keeps_file);
    assert!(!closed.node.ce_freed && !closed.node.channel_freed);
}

/// A decoder that never answers is lost within the bound.
#[test]
fn a_hung_decoder_is_lost_within_its_bound() {
    let (record, samples) = stream::samples(Codec::Hevc, crate::nvdec::HEVC_CLIP).unwrap();
    let gpu = Gpu::new(Engine::HangsAfter(1));
    let mut decoder = Decoder::begin(gpu, GA106, Codec::Hevc, &record, 5_000_000)
        .map_err(|(e, _)| e)
        .unwrap();
    while !decoder.bring_up_step().unwrap() {}
    decoder.submit(&samples[0], 0).unwrap();
    decoder.poll(true).unwrap().unwrap();
    let crop = decoder.crop;
    drain(&mut decoder, &crop);
    decoder.submit(&samples[1], 1).unwrap();
    let error = loop {
        match decoder.poll(true) {
            Ok(None) => decoder.node().pause(),
            Ok(Some(_)) => panic!("a hung decoder decoded"),
            Err(error) => break error,
        }
    };
    assert!(error.lost() && error.pending());
    assert!(!decoder.in_flight());
    assert!(decoder.close().keeps_file);
}

/// Streams this decoder does not take are refused at the open, before any
/// request: the client decodes them another way.
#[test]
fn streams_it_does_not_take_are_refused_at_the_open() {
    let base = Sequence {
        codec: Codec::H264,
        coded: (1920, 1088),
        display: (1920, 1080),
        bit_depth: 8,
        slots: 5,
        surfaces: 10,
        profile: 100,
        chroma_420: true,
        frames_only: true,
    };
    assert_eq!(refusal(&base), None);
    let cases: [(Sequence, &str); 7] = [
        (
            Sequence {
                frames_only: false,
                ..base
            },
            "interlaced",
        ),
        (
            Sequence {
                chroma_420: false,
                ..base
            },
            "4:2:0",
        ),
        (
            Sequence {
                profile: 110,
                ..base
            },
            "profile",
        ),
        (
            Sequence {
                bit_depth: 10,
                ..base
            },
            "8 bits",
        ),
        (
            Sequence {
                coded: (4112, 1088),
                ..base
            },
            "4096",
        ),
        (
            Sequence {
                display: (32, 32),
                ..base
            },
            "48x48",
        ),
        (
            Sequence {
                codec: Codec::Hevc,
                profile: 4,
                ..base
            },
            "HEVC profile",
        ),
    ];
    for (sequence, why) in cases {
        let refused = refusal(&sequence).unwrap_or("");
        assert!(refused.contains(why), "{sequence:?}: {refused}");
    }
    // A record that is not one: nothing reaches the node.
    let refused = Decoder::begin(
        Gpu::new(Engine::Decode(Tiling::NVDEC)),
        GA106,
        Codec::H264,
        &[1, 2],
        BOUND_NS,
    );
    let (error, gpu) = refused.err().unwrap();
    assert!(matches!(error, Error::Parse(_)));
    assert!(gpu.requests.is_empty());
    let _ = (format!("{error:?}"), String::new(), vec![0u8]);
}
