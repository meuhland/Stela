use super::*;
use crate::nvdec::mock::{self, CE_CHANNEL, Ce, Engine, Gpu};
use crate::nvdec::session::Session;
use crate::nvdec::{Codec, H264_CLIP, Picture, surface};
use crate::nvenc::{
    CHANNEL_ALLOC, CHANNEL_FREE, GEM_CLOSE, GEM_NEW, GROBJ_ALLOC, SYNCOBJ_CREATE, SYNCOBJ_DESTROY,
    SYNCOBJ_WAIT, VM_BIND,
};

/// GA106, the RTX 3050.
const GA106: u16 = 0x176;

fn hd() -> Geometry {
    Geometry::new(1920, 1088).unwrap()
}

const FULL: Crop = Crop {
    left: 0,
    top: 0,
    width: 1920,
    height: 1080,
};

#[test]
fn the_copy_engine_is_named_as_clc7b5_does() {
    // clc7b5.h, and nouveau_drm.h's NOUVEAU_FIFO_ENGINE_CE.
    assert_eq!(COPY_CLASS, 0xc7b5);
    assert_eq!(FIFO_ENGINE_CE, 0x30);
    assert_eq!(
        (
            method::SET_SEMAPHORE_A,
            method::SET_SEMAPHORE_B,
            method::SET_SEMAPHORE_PAYLOAD,
            method::LAUNCH_DMA,
        ),
        (0x240, 0x244, 0x248, 0x300)
    );
    assert_eq!(
        [
            method::OFFSET_IN_UPPER,
            method::OFFSET_IN_LOWER,
            method::OFFSET_OUT_UPPER,
            method::OFFSET_OUT_LOWER,
            method::PITCH_IN,
            method::PITCH_OUT,
            method::LINE_LENGTH_IN,
            method::LINE_COUNT,
        ],
        [0x400, 0x404, 0x408, 0x40c, 0x410, 0x414, 0x418, 0x41c]
    );
    assert_eq!(
        [
            method::SET_SRC_BLOCK_SIZE,
            method::SET_SRC_WIDTH,
            method::SET_SRC_HEIGHT,
            method::SET_SRC_DEPTH,
            method::SET_SRC_LAYER,
            method::SRC_ORIGIN_X,
            method::SRC_ORIGIN_Y,
        ],
        [0x728, 0x72c, 0x730, 0x734, 0x738, 0x744, 0x748]
    );
    // LAUNCH_DMA: NON_PIPELINED (1:0 = 2), FLUSH_ENABLE (2), the source
    // block-linear (7 = 0), the destination pitch (8), MULTI_LINE (9),
    // DISABLE_PLC (26); the release: no data, FLUSH_ENABLE, one word (4:3).
    assert_eq!(launch::COPY, 0x0400_0306);
    assert_eq!(launch::RELEASE, 0x0400_000c);
    // The kernel's own pitch-to-pitch copies (nouveau_bo90b5.c) launch
    // 0x386: the same with a pitch source and no DISABLE_PLC.
    assert_eq!(
        (launch::COPY | launch::SRC_PITCH) & !launch::DISABLE_PLC,
        0x386
    );
    // SET_SRC_BLOCK_SIZE: HEIGHT log2 at 7:4, GOB_HEIGHT_FERMI_8 at 15:12.
    assert_eq!(block_size(1), 0x1000);
    assert_eq!(block_size(2), 0x1010);
    assert_eq!(block_size(32), 0x1050);
}

#[test]
fn a_1080p_frame_is_two_blits_of_its_displayed_rectangle() {
    let [luma, chroma] = nv12(0x1_2040_0000, 0x1_209f_e000, &hd(), &FULL, STAGING_ADDRESS);
    assert_eq!(
        luma,
        Blit {
            src: 0x1_2040_0000,
            src_pitch: 1920,
            src_rows: 1088,
            gobs: 2,
            x: 0,
            y: 0,
            width: 1920,
            rows: 1080,
            dst: STAGING_ADDRESS,
            dst_pitch: 1920,
        }
    );
    assert_eq!(
        chroma,
        Blit {
            src: 0x1_209f_e000,
            src_rows: 544,
            rows: 540,
            dst: STAGING_ADDRESS + 1920 * 1080,
            ..luma
        }
    );
    assert_eq!(nv12_bytes(&FULL), 3_110_400);
    assert_eq!(
        chroma.dst + chroma.span(),
        STAGING_ADDRESS + nv12_bytes(&FULL)
    );
    for blit in [luma, chroma] {
        assert_eq!(
            blit.refusal(STAGING_ADDRESS, STAGING_ADDRESS + nv12_bytes(&FULL)),
            None
        );
    }
    // A crop moves the origin; chroma rows are half the luma's.
    let crop = Crop {
        left: 64,
        top: 16,
        width: 1280,
        height: 720,
    };
    let [luma, chroma] = nv12(0x1_2040_0000, 0x1_209f_e000, &hd(), &crop, STAGING_ADDRESS);
    assert_eq!((luma.x, luma.y, luma.width, luma.rows), (64, 16, 1280, 720));
    assert_eq!(
        (chroma.x, chroma.y, chroma.width, chroma.rows),
        (64, 8, 1280, 360)
    );
    assert_eq!(
        (luma.dst_pitch, chroma.dst),
        (1280, STAGING_ADDRESS + 1280 * 720)
    );
}

#[test]
fn the_method_stream_is_word_for_word() {
    // Slot 0 of NVIDIA's captured layout: the luma at 0x1_2040_0000, the
    // chroma 0x5fe000 after it.
    let blits = nv12(0x1_2040_0000, 0x1_209f_e000, &hd(), &FULL, STAGING_ADDRESS);
    let words = words(&blits, RING_ADDRESS + SEMAPHORE_OFFSET, 7, true);
    #[rustfmt::skip]
    let expected = [
        0x2001_8000, 0xc7b5,
        // OFFSET_IN_UPPER..LINE_COUNT
        0x2008_8100, 0x1, 0x2040_0000, 0x1, 0x3010_0000, 1920, 1920, 1920, 1080,
        // SET_SRC_BLOCK_SIZE..SET_SRC_LAYER
        0x2005_81ca, 0x1010, 1920, 1088, 1, 0,
        // SRC_ORIGIN_X, _Y
        0x2002_81d1, 0, 0,
        0x2001_80c0, 0x0400_0306,
        0x2008_8100, 0x1, 0x209f_e000, 0x1, 0x302f_a400, 1920, 1920, 1920, 540,
        0x2005_81ca, 0x1010, 1920, 544, 1, 0,
        0x2002_81d1, 0, 0,
        0x2001_80c0, 0x0400_0306,
        // SET_SEMAPHORE_A, _B, _PAYLOAD, then the release
        0x2003_8090, 0x1, 0x3000_f000, 7,
        0x2001_80c0, 0x0400_000c,
    ];
    assert_eq!(words, expected);
    assert_eq!(words.len(), 2 + 2 * BLIT_WORDS + 6);
    let decoded = mock::decode(&words);
    assert_eq!(decoded[0], (method::SET_OBJECT, COPY_CLASS));
    assert_eq!(decoded[1], (method::OFFSET_IN_UPPER, 1));
    assert_eq!(decoded[8], (method::LINE_COUNT, 1080));
    assert_eq!(decoded[14], (method::SRC_ORIGIN_X, 0));
    assert_eq!(decoded.last(), Some(&(method::LAUNCH_DMA, launch::RELEASE)));
    // Later copies do not bind the class again.
    let again = super::words(&blits, RING_ADDRESS + SEMAPHORE_OFFSET, 8, false);
    let mut want = words[2..words.len() - 6].to_vec();
    want.extend([0x2003_8090, 0x1, 0x3000_f000, 8, 0x2001_80c0, 0x0400_000c]);
    assert_eq!(again, want);
}

#[test]
fn blits_outside_their_plane_or_the_staging_are_refused() {
    let [luma, _] = nv12(0x1_2040_0000, 0x1_209f_e000, &hd(), &FULL, STAGING_ADDRESS);
    let end = STAGING_ADDRESS + nv12_bytes(&FULL);
    let cases = [
        (Blit { width: 0, ..luma }, "an empty blit"),
        (
            Blit { gobs: 3, ..luma },
            "a block height the engine has no field for",
        ),
        (
            Blit {
                src_pitch: 1900,
                ..luma
            },
            "a block-linear plane of partial blocks",
        ),
        (Blit { x: 16, ..luma }, "a blit past its plane"),
        (Blit { y: 9, ..luma }, "a blit past its plane"),
        (
            Blit {
                dst_pitch: 1024,
                ..luma
            },
            "a line longer than the output's pitch",
        ),
        (
            Blit {
                dst: STAGING_ADDRESS - 0x1000,
                ..luma
            },
            "a blit outside the staging buffer",
        ),
        (
            Blit {
                dst: end - 1920 * 1080 + 1,
                ..luma
            },
            "a blit outside the staging buffer",
        ),
    ];
    for (blit, why) in cases {
        assert_eq!(blit.refusal(STAGING_ADDRESS, end), Some(why), "{blit:?}");
    }
}

#[test]
fn the_rings_sit_past_every_buffer_of_the_captured_layout() {
    for codec in [Codec::H264, Codec::Hevc] {
        let layout = crate::nvdec::layout::Layout::new(codec, &hd(), 5).unwrap();
        for plan in &layout.plans {
            assert!(plan.address + plan.span <= RING_ADDRESS, "{plan:?}");
        }
    }
}

/// A session that decoded the H.264 clip's first picture into slot 0, and
/// that picture.
fn decoded(gpu: Gpu) -> (Session<Gpu>, Picture) {
    let picture = Picture::all(Codec::H264, H264_CLIP).unwrap().remove(0);
    let geometry = picture.geometry().unwrap();
    let mut session = Session::open(gpu, GA106, Codec::H264, geometry, 4, 1_000_000_000)
        .map_err(|(error, _)| error)
        .unwrap();
    let done = session.decode(&picture).unwrap();
    assert_eq!(done.slot, 0);
    (session, picture)
}

#[test]
fn a_copy_lands_the_displayed_nv12_picture_in_system_memory() {
    let (luma, chroma) = mock::picture(0, 1920, 1088);
    for crop in [
        FULL,
        Crop {
            left: 64,
            top: 16,
            width: 1280,
            height: 720,
        },
        Crop {
            left: 8,
            top: 2,
            width: 1906,
            height: 1074,
        },
    ] {
        // A copier a file: its buffers' addresses stay bound until the file
        // closes.
        let (mut session, _) = decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)));
        let (luma_at, chroma_at) = session.layout().surface(0);
        let bytes = nv12_bytes(&crop);
        let mut copier = Copier::open(session.node(), bytes, 500_000_000).unwrap();
        assert_eq!(copier.channel(), CE_CHANNEL);
        let blits = nv12(luma_at, chroma_at, &hd(), &crop, STAGING_ADDRESS);
        let copied = copier.copy(session.node(), &blits).unwrap();
        assert!(copied.ce_ns > 0);
        let want = surface::picture(&luma, &chroma, surface::Tiling::Pitch, 1920, &crop);
        assert_eq!(
            copier.output(bytes as usize).unwrap(),
            &want[..],
            "{crop:?}"
        );
        // Again: the payload counts up and the class is not bound twice.
        copier.copy(session.node(), &blits).unwrap();
        assert_eq!(
            copier.output(bytes as usize).unwrap(),
            &want[..],
            "{crop:?}"
        );
        assert!(!copier.close(session.node()));
        let closed = session.close();
        assert!(!closed.keeps_file);
        let gpu = closed.node;
        let payloads: Vec<u32> = gpu
            .ce_submissions
            .iter()
            .map(|s| {
                s.iter()
                    .find(|(m, _)| *m == method::SET_SEMAPHORE_PAYLOAD)
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(payloads, [1, 2]);
        assert_eq!(gpu.ce_submissions[1][0].0, method::OFFSET_IN_UPPER);
        assert!(gpu.ce_freed && gpu.channel_freed);
        assert_eq!(gpu.mapped, gpu.unmapped);
    }
}

#[test]
fn opening_asks_for_a_channel_the_class_a_syncobj_and_two_buffers() {
    let (mut session, _) = decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)));
    let before = session.node().requests.len();
    let copier = Copier::open(session.node(), nv12_bytes(&FULL), 1).unwrap();
    assert_eq!(
        session.node().requests[before..],
        [
            CHANNEL_ALLOC,
            GROBJ_ALLOC,
            SYNCOBJ_CREATE,
            GEM_NEW,
            VM_BIND,
            GEM_NEW,
            VM_BIND
        ]
    );
    let bound: Vec<(u64, u64, u8)> = session
        .node()
        .buffers
        .iter()
        .rev()
        .take(2)
        .map(|b| (b.address.unwrap(), b.bytes, b.kind))
        .collect();
    assert_eq!(
        bound,
        [
            (STAGING_ADDRESS, 0x2f_8000, 0),
            (RING_ADDRESS, RING_BYTES, 0)
        ]
    );
    let closed_before = session.node().closed.len();
    let before = session.node().requests.len();
    assert!(!copier.close(session.node()));
    assert_eq!(
        session.node().requests[before..],
        [GEM_CLOSE, GEM_CLOSE, SYNCOBJ_DESTROY, CHANNEL_FREE]
    );
    assert_eq!(session.node().closed.len(), closed_before + 2);
}

#[test]
fn a_refused_channel_or_class_gives_everything_back() {
    for (ce, step, errno) in [
        (Ce::NoChannel(-22), "copy channel", -22),
        (Ce::NoClass(-22), "copy class", -22),
    ] {
        let (mut session, _) =
            decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)).with_ce(ce));
        let error = Copier::open(session.node(), nv12_bytes(&FULL), 1)
            .err()
            .unwrap();
        assert_eq!(error, Error::Refused { step, errno });
        let gpu = session.node();
        assert_eq!(gpu.ce_freed, step == "copy class");
        assert!(gpu.ce_submissions.is_empty());
    }
    let text = alloc::format!(
        "{}",
        Error::Refused {
            step: "copy channel",
            errno: -22
        }
    );
    assert_eq!(
        text,
        "copy channel refused: EINVAL (-22): nouveau found no copy-engine runlist"
    );
}

#[test]
fn a_hung_copy_times_out_within_the_bound_and_keeps_its_channel() {
    let (mut session, _) =
        decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)).with_ce(Ce::Hangs));
    let (luma_at, chroma_at) = session.layout().surface(0);
    let mut copier = Copier::open(session.node(), nv12_bytes(&FULL), 50_000_000).unwrap();
    let blits = nv12(luma_at, chroma_at, &hd(), &FULL, STAGING_ADDRESS);
    let before = session.node().clock;
    let error = copier.copy(session.node(), &blits).unwrap_err();
    assert_eq!(
        error,
        Error::Timeout {
            value: 0,
            job_done: false
        }
    );
    assert!(error.pending());
    // Bounded: 50 ms of 1 ms pauses, a fence query each.
    let waited = session.node().clock - before;
    assert!((50_000_000..60_000_000).contains(&waited), "{waited}");
    assert!(copier.keeps_channel());
    assert_eq!(copier.copy(session.node(), &blits), Err(Error::Dead));
    assert_eq!(copier.output(16), None);
    let closed = session.node().closed.len();
    assert!(copier.close(session.node()));
    let gpu = session.node();
    assert!(!gpu.ce_freed);
    assert_eq!(
        gpu.closed.len(),
        closed,
        "nothing freed under a pending job"
    );
    assert!(!gpu.requests.ends_with(&[SYNCOBJ_DESTROY]));
    // Each bound look asked nouveau's fence.
    assert!(gpu.requests.iter().filter(|&&r| r == SYNCOBJ_WAIT).count() >= 50);
}

#[test]
fn a_silent_copy_times_out_but_its_channel_is_freed() {
    let (mut session, _) =
        decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)).with_ce(Ce::Silent));
    let (luma_at, chroma_at) = session.layout().surface(0);
    let mut copier = Copier::open(session.node(), nv12_bytes(&FULL), 5_000_000).unwrap();
    let blits = nv12(luma_at, chroma_at, &hd(), &FULL, STAGING_ADDRESS);
    let error = copier.copy(session.node(), &blits).unwrap_err();
    assert_eq!(
        error,
        Error::Timeout {
            value: 0,
            job_done: true
        }
    );
    assert!(!error.pending() && !copier.keeps_channel());
    assert!(!copier.close(session.node()));
    assert!(session.node().ce_freed);
}

#[test]
fn a_blit_outside_the_staging_is_refused_before_the_engine() {
    let (mut session, _) = decoded(Gpu::new(Engine::Decode(surface::Tiling::NVDEC)));
    let (luma_at, chroma_at) = session.layout().surface(0);
    // Staging for a 720p picture, a 1080p copy.
    let small = Crop {
        left: 0,
        top: 0,
        width: 1280,
        height: 720,
    };
    let mut copier = Copier::open(session.node(), nv12_bytes(&small), 1).unwrap();
    let blits = nv12(luma_at, chroma_at, &hd(), &FULL, STAGING_ADDRESS);
    assert_eq!(
        copier.copy(session.node(), &blits),
        Err(Error::Config("a blit outside the staging buffer"))
    );
    assert!(session.node().ce_submissions.is_empty());
    // Still open: a refused copy is not a failed one.
    assert_eq!(copier.dead(), None);
    assert!(!copier.close(session.node()));
}
