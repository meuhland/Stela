//! The encoder against nvenc-test's offline proofs and the GPU's own
//! output, and a session on the mock GPU.
use super::super::layout::{Geometry, Layout};
use super::super::mock::{Engine, Gpu};
use super::super::push;
use super::super::selftest::hex;
use super::super::setup::{self, Picture, Templates};
use super::super::surface::{self, Order, Rgb};
use super::*;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use stela_crypto::Sha256;
use stela_crypto::digest::Digest;

const CBR: Rate = Rate::Cbr {
    bitrate: Rate::CAPTURED_BITRATE,
};
const BOUND_NS: u64 = 2_000_000_000;

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn word_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_le_bytes()).collect()
}

/// nvenc-test's `--dry-run -n 30` digests (`fixtures/dry-run.txt`) of
/// `mode`'s `kind` ("setup" or "submit"), frame by frame.
fn golden(mode: &str, kind: &str) -> Vec<&'static str> {
    let text = include_str!("../fixtures/dry-run.txt");
    let rows: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(' ').collect();
            (fields[0] == mode && fields[1] == kind).then_some(fields[3])
        })
        .collect();
    assert_eq!(rows.len(), 30, "{mode} {kind}");
    rows
}

fn hd(codec: Codec, rate: Rate) -> (Config, Geometry, Layout, Templates) {
    let config = Config::hd(codec, rate);
    let geometry = Geometry::new(1920, 1080).unwrap();
    let layout = Layout::new(codec, &geometry, false);
    let templates = Templates::new(&config, &geometry, &layout).unwrap();
    (config, geometry, layout, templates)
}

/// Every setup and every submission of 30 frames is nvenc-test's, byte for
/// byte, in each of its four dry runs: H.264 and HEVC, CBR and QP 25.
#[test]
fn setups_and_method_streams_are_nvenc_tests() {
    for (mode, codec, rate) in [
        ("h264-cbr", Codec::H264, CBR),
        ("h264-qp25", Codec::H264, Rate::ConstQp(25)),
        ("hevc-cbr", Codec::Hevc, CBR),
        ("hevc-qp25", Codec::Hevc, Rate::ConstQp(25)),
    ] {
        let (_, _, layout, templates) = hd(codec, rate);
        let setups = golden(mode, "setup");
        let submits = golden(mode, "submit");
        let mut setup = vec![0; codec.setup_bytes()];
        for n in 0..30u32 {
            let refs = n.min(templates.max_refs);
            let picture = Picture {
                index: n,
                since_idr: n,
                idrs_before: 0,
                refs,
            };
            setup::build(&templates, &picture, &mut setup);
            assert_eq!(sha(&setup), setups[n as usize], "{mode} setup {n}");
            let words = push::words(
                &layout,
                &push::Frame {
                    index: n,
                    idr: n == 0,
                    refs,
                    control: codec.control(rate),
                    payload: n + 1,
                    input: layout.address(layout.input[(n % 4) as usize]),
                    preamble: n == 0,
                },
            );
            assert_eq!(
                sha(&word_bytes(&words)),
                submits[n as usize],
                "{mode} submit {n}"
            );
        }
    }
    // And the raw words of a few, for a readable diff.
    let (_, _, layout, _) = hd(Codec::H264, CBR);
    let frame = |n: u32| push::Frame {
        index: n,
        idr: n == 0,
        refs: n.min(3),
        control: 0x1200_1303,
        payload: n + 1,
        input: layout.address(layout.input[(n % 4) as usize]),
        preamble: n == 0,
    };
    let frame0 = include_bytes!("../fixtures/h264-frame0-submit.bin");
    assert_eq!(word_bytes(&push::words(&layout, &frame(0))), frame0);
    let frame5 = include_bytes!("../fixtures/h264-frame5-submit.bin");
    assert_eq!(word_bytes(&push::words(&layout, &frame(5))), frame5);
}

/// The 1080p session's size and rate leave NVIDIA's setups as captured,
/// but for the H.264 P setup's drifting bitrate.
#[test]
fn a_1080p_session_keeps_the_captured_setups() {
    for codec in [Codec::H264, Codec::Hevc] {
        let (_, _, _, templates) = hd(codec, CBR);
        assert_eq!(templates.idr, setup::template(codec, true), "{codec:?} IDR");
        let captured = setup::template(codec, false);
        let differing: Vec<usize> = (0..captured.len())
            .filter(|&i| captured[i] != templates.p[i])
            .collect();
        match codec {
            // rate_control.nal_bitrate, bytes 148..152.
            Codec::H264 => assert!(differing.iter().all(|i| (148..152).contains(i))),
            Codec::Hevc => assert!(differing.is_empty(), "{differing:?}"),
        }
        assert_eq!(templates.max_refs, if codec == Codec::H264 { 3 } else { 1 });
    }
}

#[test]
fn parameter_sets_are_nvenc_tests() {
    let (_, geometry, _, templates) = hd(Codec::H264, CBR);
    assert_eq!(
        h264_parameter_sets(&templates.idr, geometry.width, geometry.height),
        include_bytes!("../fixtures/h264-sps-pps.bin")
    );
    let (_, geometry, _, templates) = hd(Codec::Hevc, CBR);
    assert_eq!(
        hevc_parameter_sets(&templates.idr, geometry.width, geometry.height, 4).unwrap(),
        include_bytes!("../fixtures/hevc-vps-sps-pps.bin")
    );
}

/// The pattern and its block-linear surface are nvenc-test's `tiling.c`
/// (`fixtures/surfaces.txt`, from its own code), and reading the surface back
/// gives the pattern.
#[test]
fn surfaces_are_nvenc_tests_and_round_trip() {
    let geometry = Geometry::new(1920, 1080).unwrap();
    let expected: Vec<(&str, &str)> = include_str!("../fixtures/surfaces.txt")
        .lines()
        .map(|line| line.split_once(' ').unwrap())
        .collect();
    let want = |name: &str| expected.iter().find(|(n, _)| *n == name).unwrap().1;
    for frame in [0, 7, 29] {
        let pattern = surface::pattern(&geometry, frame);
        assert_eq!(sha(&pattern), want(&alloc::format!("pattern_{frame}")));
        let mut stored = vec![0u8; geometry.surface_bytes() as usize];
        let picture = surface::pattern_picture(&geometry, &pattern);
        // SAFETY: `stored` holds the surface and the pattern fits it.
        unsafe { surface::store_nv12(stored.as_mut_ptr(), &geometry, &picture) };
        assert_eq!(sha(&stored), want(&alloc::format!("surface_{frame}")));
        // SAFETY: as above, reading.
        let loaded = unsafe { surface::load_nv12(stored.as_ptr(), &geometry) };
        assert_eq!(loaded, pattern);
    }
    // Each 16-byte line of a plane lands at its own offset.
    let mut seen = vec![false; 1920 * 1088 / 16];
    for y in 0..1088 {
        for x in (0..1920).step_by(16) {
            let offset = surface::block_linear_offset(x, y, 1920);
            assert_eq!(offset % 16, 0);
            assert!(!core::mem::replace(&mut seen[offset / 16], true));
        }
    }
}

#[test]
fn bgra_converts_to_bt709_limited_nv12() {
    let geometry = Geometry::new(64, 32).unwrap();
    let mut pixels = vec![0u8; 64 * 4 * 32];
    for (index, pixel) in pixels.chunks_mut(4).enumerate() {
        // Left half white, right half pure red, as B, G, R, A.
        let colour = if index % 64 < 32 {
            [255, 255, 255, 255]
        } else {
            [0, 0, 255, 255]
        };
        pixel.copy_from_slice(&colour);
    }
    let mut stored = vec![0u8; geometry.surface_bytes() as usize];
    let picture = Rgb {
        pixels: &pixels,
        pitch: 256,
        order: Order::Bgra,
    };
    // SAFETY: `stored` holds the surface and the picture fits it.
    unsafe { surface::store_bgra(stored.as_mut_ptr(), &geometry, &picture) };
    // SAFETY: as above, reading.
    let nv12 = unsafe { surface::load_nv12(stored.as_ptr(), &geometry) };
    let (luma, chroma) = nv12.split_at(64 * 32);
    assert_eq!((luma[0], luma[63]), (235, 63)); // white; red's 16 + 46.7
    assert_eq!((chroma[0], chroma[1]), (128, 128));
    // Red: Cb 102, Cr 240 in BT.709 limited range.
    assert_eq!((chroma[62], chroma[63]), (102, 240));
}

/// The GPU's own output (VM 118, 2026-11-16, `es13b`): where each status
/// says the slices are, they begin with a start code and an IDR, then a P
/// slice.
#[test]
fn real_statuses_point_at_their_slices() {
    for (codec, start, idr_type, p_type) in [(Codec::H264, 0, 5, 1), (Codec::Hevc, 0xff, 19, 1)] {
        let hevc = codec == Codec::Hevc;
        let (statuses, bitstreams): ([&[u8]; 2], [&[u8]; 2]) = match codec {
            Codec::H264 => (
                [
                    include_bytes!("../fixtures/h264-status-0.bin"),
                    include_bytes!("../fixtures/h264-status-1.bin"),
                ],
                [
                    include_bytes!("../fixtures/h264-bitstream-0.bin"),
                    include_bytes!("../fixtures/h264-bitstream-1.bin"),
                ],
            ),
            Codec::Hevc => (
                [
                    include_bytes!("../fixtures/hevc-status-0.bin"),
                    include_bytes!("../fixtures/hevc-status-1.bin"),
                ],
                [
                    include_bytes!("../fixtures/hevc-bitstream-0.bin"),
                    include_bytes!("../fixtures/hevc-bitstream-1.bin"),
                ],
            ),
        };
        for (n, nal) in [(0usize, idr_type), (1, p_type)] {
            let status = Status::read(codec, statuses[n]);
            assert_eq!(status.picture_index, n as u32);
            assert!(status.accepted_error(), "{codec:?} {n}: {status:?}");
            assert_eq!(status.bitstream_start, start);
            assert!(status.total_bits > 0 && status.total_bits.is_multiple_of(8));
            let at = status.bitstream_start as usize;
            assert!(start_code_at(bitstreams[n], at));
            let types = nal_types(&bitstreams[n][at..], hevc);
            assert_eq!(types.first(), Some(&nal), "{codec:?} {n}");
            if n == 1 {
                // The whole P frame was kept: its bytes end where it says.
                assert!(at + (status.total_bits / 8) as usize <= bitstreams[n].len());
            }
        }
    }
}

#[test]
fn the_captured_layout_is_nvidias() {
    let geometry = Geometry::new(1920, 1080).unwrap();
    let h264 = Layout::new(Codec::H264, &geometry, false);
    assert!(h264.captured);
    let at = |layout: &Layout, index: usize| layout.address(index);
    assert_eq!(at(&h264, h264.push), 0x1_1000_0000);
    assert_eq!(at(&h264, h264.fence), 0x1_1004_0000);
    assert_eq!(at(&h264, h264.setup[15]), 0x1_2009_0000 + 15 * 0x3000);
    assert_eq!(at(&h264, h264.rc_data[3]), 0x1_2017_3000 + 3 * 0x6000);
    assert_eq!(at(&h264, h264.recon[4]), 0x1_2140_0000);
    assert_eq!(at(&h264, h264.input[1]), 0x1_2280_0000);
    assert_eq!(at(&h264, h264.bitstream[3]), 0x1_2230_c000 + 3 * 0xc0_0000);
    assert_eq!(h264.bitstream_bytes(), 0x30_c000);
    let hevc = Layout::new(Codec::Hevc, &geometry, false);
    assert_eq!(at(&hevc, hevc.mepred[1]), 0x1_201f_0000);
    assert_eq!(at(&hevc, hevc.status[2]), 0x1_2183_e000);
    assert_eq!(at(&hevc, hevc.bitstream[1]), 0x1_2280_0000);
    assert!(hevc.coloc.is_none());
    assert_eq!(hevc.bitstream_bytes(), 0x4f_c000);
}

#[test]
fn a_larger_picture_gets_a_scaled_layout_that_fits() {
    for codec in [Codec::H264, Codec::Hevc] {
        for (width, height) in [(2560, 1440), (3840, 2160), (4096, 4096)] {
            let geometry = Geometry::new(width, height).unwrap();
            let layout = Layout::new(codec, &geometry, true);
            assert!(!layout.captured);
            let mut spans: Vec<(u64, u64)> = layout
                .plans
                .iter()
                .map(|plan| {
                    assert!(plan.span >= plan.bytes && plan.address % 0x1000 == 0);
                    (plan.address, plan.address + plan.span)
                })
                .collect();
            spans.sort();
            assert!(spans.windows(2).all(|pair| pair[0].1 <= pair[1].0));
            assert!(spans.last().unwrap().1 <= super::super::layout::KERNEL_VA_START);
            assert!(layout.bitstream_bytes() > geometry.surface_bytes());
        }
    }
    assert!(Geometry::new(4098, 64).is_err());
    assert!(Geometry::new(1919, 1080).is_err());
    assert_eq!(Geometry::new(1280, 720).unwrap().pitch, 1280);
    assert_eq!(Geometry::new(1366, 768).unwrap().pitch, 1408);
}

fn open(engine: Engine, config: Config) -> Session<Gpu> {
    Session::open(Gpu::new(engine), 0x176, config, BOUND_NS)
        .map_err(|(error, _)| error)
        .unwrap()
}

fn encode_pattern(session: &mut Session<Gpu>, n: u32, options: Options) -> Result<Encoded, Error> {
    let geometry = *session.geometry();
    let pattern = surface::pattern(&geometry, n);
    let picture = surface::pattern_picture(&geometry, &pattern);
    session.encode(Frame::Nv12(picture), options)
}

/// A session's own submissions are nvenc-test's, and its frames come back
/// as Annex B with the parameter sets on the IDR.
#[test]
fn a_session_encodes_what_nvenc_test_sends() {
    let mut session = open(Engine::Encode, Config::hd(Codec::H264, CBR));
    let submits = golden("h264-cbr", "submit");
    for n in 0..6u32 {
        let encoded = encode_pattern(&mut session, n, Options::default()).unwrap();
        assert_eq!((encoded.picture, encoded.idr), (n, n == 0));
        assert!(encoded.status.accepted_error());
        let types = nal_types(&encoded.bytes, false);
        assert_eq!(types, if n == 0 { vec![7, 8, 5] } else { vec![1] });
        assert!(encoded.bytes.starts_with(&[0, 0, 0, 1]));
        // The engine's reconstruction is the input here.
        let recon = session.recon(n);
        assert!(recon.is_none(), "not mapped unless asked");
    }
    let closed = session.close();
    let gpu = closed.node;
    assert!(!closed.keeps_file);
    for (n, words) in gpu.submissions.iter().enumerate() {
        assert_eq!(sha(&word_bytes(words)), submits[n], "submission {n}");
    }
    assert_eq!(
        gpu.requests[..4],
        [VM_INIT, CHANNEL_ALLOC, GROBJ_ALLOC, SYNCOBJ_CREATE]
    );
    assert!(gpu.channel_freed && gpu.syncobj_destroyed);
    assert_eq!(gpu.closed.len(), gpu.buffers.len());
    assert_eq!(gpu.mapped, gpu.unmapped);
}

#[test]
fn hevc_finds_its_slices_at_0xff() {
    let config = Config {
        map_recon: true,
        ..Config::hd(Codec::Hevc, Rate::ConstQp(25))
    };
    let mut session = open(Engine::Encode, config);
    for n in 0..3u32 {
        let encoded = encode_pattern(&mut session, n, Options::default()).unwrap();
        let types = nal_types(&encoded.bytes, true);
        assert_eq!(
            types,
            if n == 0 {
                vec![32, 33, 34, 19]
            } else {
                vec![1]
            }
        );
        let geometry = *session.geometry();
        let recon = session.recon(n).unwrap();
        let pattern = surface::pattern(&geometry, n);
        assert_eq!(
            surface::psnr_luma_centi_db(&pattern, &recon, &geometry),
            9900
        );
    }
    let gpu = session.close().node;
    // HEVC sends no co-located methods.
    assert!(gpu.submissions.iter().all(|words| {
        super::super::tests::decode(words)
            .iter()
            .all(|(_, m, _)| !matches!(*m, method::SET_IN_COLOC_DATA | method::SET_OUT_COLOC_DATA))
    }));
}

#[test]
fn a_forced_idr_restarts_the_references() {
    let mut session = open(Engine::Encode, Config::hd(Codec::H264, Rate::ConstQp(25)));
    for n in 0..3 {
        encode_pattern(&mut session, n, Options::default()).unwrap();
    }
    let forced = encode_pattern(&mut session, 3, Options { force_idr: true }).unwrap();
    assert!(forced.idr);
    assert_eq!(nal_types(&forced.bytes, false), [7, 8, 5]);
    let after = encode_pattern(&mut session, 4, Options::default()).unwrap();
    assert!(!after.idr);
    let mut gpu = session.close().node;
    // The forced IDR is the session's second: idr_pic_id 1, frame_num 0;
    // the frame after it references it alone.
    let methods = super::super::tests::decode(&gpu.submissions[4]);
    let refs = methods
        .iter()
        .filter(|(_, m, _)| {
            (method::SET_IN_REF_PIC0_LUMA..method::SET_IN_REF_PIC0_CHROMA).contains(m)
        })
        .count();
    assert_eq!(refs, 1);
    let setup_address = |words: &Vec<u32>| {
        super::super::tests::decode(words)
            .iter()
            .find(|(_, m, _)| *m == method::SET_IN_DRV_PIC_SETUP)
            .map(|(_, _, data)| u64::from(*data) << 8)
            .unwrap()
    };
    use super::super::fields::h264 as f;
    let address = setup_address(&gpu.submissions[3]);
    let setup = gpu.read(address, Codec::H264.setup_bytes());
    assert_eq!(setup::picture_type(Codec::H264, &setup), 3);
    assert_eq!(f::PIC_CONTROL_IDR_PIC_ID.get(&setup), 1);
    assert_eq!(f::PIC_CONTROL_FRAME_NUM.get(&setup), 0);
    let address = setup_address(&gpu.submissions[4]);
    let setup = gpu.read(address, Codec::H264.setup_bytes());
    assert_eq!(f::PIC_CONTROL_FRAME_NUM.get(&setup), 1);
    assert_eq!(f::PIC_CONTROL_PIC_ORDER_CNT_LSB.get(&setup), 2);
    assert_eq!(f::SLICE_NUM_REF_IDX_L0_ACTIVE_MINUS1.get(&setup), 0);
    // Frame 3 sits in DPB slot 3.
    assert_eq!(f::PIC_CONTROL_L0[0].get(&setup), 6);
    assert_eq!(f::PIC_CONTROL_L0[1].get(&setup), 0xfe);
}

#[test]
fn bgra_frames_encode() {
    let mut session = open(Engine::Encode, Config::hd(Codec::Hevc, CBR));
    let pixels = vec![0x40u8; 1920 * 4 * 1080];
    let picture = Rgb {
        pixels: &pixels,
        pitch: 1920 * 4,
        order: Order::Bgra,
    };
    let encoded = session
        .encode(Frame::Rgb(picture), Options::default())
        .unwrap();
    assert!(encoded.idr);
    let small = Rgb {
        pixels: &pixels[..1000],
        ..picture
    };
    assert!(matches!(
        session.encode(Frame::Rgb(small), Options::default()),
        Err(Error::Input(_))
    ));
}

#[test]
fn a_hung_engine_times_out_and_keeps_the_channel_and_file() {
    let mut session = open(Engine::HangsAfter(2), Config::hd(Codec::H264, CBR));
    for n in 0..2 {
        encode_pattern(&mut session, n, Options::default()).unwrap();
    }
    let error = encode_pattern(&mut session, 2, Options::default()).unwrap_err();
    assert_eq!(
        error,
        Error::Timeout {
            value: 2,
            job_done: false
        }
    );
    assert_eq!(session.dead(), Some(true));
    assert_eq!(
        encode_pattern(&mut session, 3, Options::default()),
        Err(Error::Dead)
    );
    let closed = session.close();
    assert!(closed.keeps_file);
    let gpu = closed.node;
    // Nothing that would make nouveau wait for the push; mappings go.
    assert!(!gpu.channel_freed && !gpu.syncobj_destroyed && gpu.closed.is_empty());
    assert_eq!(gpu.mapped, gpu.unmapped);
    // Bounded: two seconds of 1 ms pauses, with one fence query each.
    let queries = gpu.requests.iter().filter(|&&r| r == SYNCOBJ_WAIT).count();
    assert!((1990..=2010).contains(&queries), "{queries}");
}

#[test]
fn a_silent_engine_times_out_but_frees_the_channel() {
    let mut session = open(Engine::Silent, Config::hd(Codec::Hevc, CBR));
    let error = encode_pattern(&mut session, 0, Options::default()).unwrap_err();
    assert_eq!(
        error,
        Error::Timeout {
            value: 0,
            job_done: true
        }
    );
    let closed = session.close();
    assert!(!closed.keeps_file);
    assert!(closed.node.channel_freed);
}

#[test]
fn bad_output_is_refused_and_the_next_frame_is_an_idr() {
    let mut session = open(Engine::WrongType, Config::hd(Codec::H264, CBR));
    encode_pattern(&mut session, 0, Options::default()).unwrap();
    let error = encode_pattern(&mut session, 1, Options::default()).unwrap_err();
    assert!(matches!(error, Error::Engine { why, .. } if why.contains("picture type")));
    assert_eq!(session.dead(), None);
    let next = encode_pattern(&mut session, 2, Options::default()).unwrap();
    assert!(next.idr);
    let mut session = open(Engine::Error, Config::hd(Codec::H264, CBR));
    let error = encode_pattern(&mut session, 0, Options::default()).unwrap_err();
    assert!(matches!(error, Error::Engine { status, .. } if status.error_status == 1));
}

#[test]
fn refusals_give_back_what_was_made() {
    let mut gpu = Gpu::new(Engine::Encode);
    gpu.refuse.push((CHANNEL_ALLOC, -38));
    let (error, gpu) = Session::open(gpu, 0x176, Config::hd(Codec::H264, CBR), BOUND_NS)
        .err()
        .unwrap();
    assert_eq!(
        error,
        Error::Refused {
            step: "channel",
            errno: -38
        }
    );
    assert_eq!(gpu.requests, [VM_INIT, CHANNEL_ALLOC]);
    // A buffer refused part way: the others, the syncobj and the channel go.
    let mut gpu = Gpu::new(Engine::Encode);
    gpu.refuse.push((VM_BIND, -28));
    let (error, gpu) = Session::open(gpu, 0x176, Config::hd(Codec::H264, CBR), BOUND_NS)
        .err()
        .unwrap();
    assert_eq!(
        error,
        Error::Refused {
            step: "push",
            errno: -28
        }
    );
    assert_eq!(gpu.closed, [1]);
    assert!(gpu.channel_freed && gpu.syncobj_destroyed);
    // A chipset without the class, or a size NVENC does not take, asks
    // nothing of the GPU.
    for (chipset, config) in [
        (0x194, Config::hd(Codec::H264, CBR)),
        (
            0x176,
            Config {
                width: 8192,
                ..Config::hd(Codec::H264, CBR)
            },
        ),
        (0x176, Config::hd(Codec::H264, Rate::ConstQp(52))),
    ] {
        let (_, gpu) = Session::open(Gpu::new(Engine::Encode), chipset, config, BOUND_NS)
            .err()
            .unwrap();
        assert!(gpu.requests.is_empty());
    }
}

#[test]
fn a_1440p_session_runs_on_its_scaled_layout() {
    let config = Config {
        width: 2560,
        height: 1440,
        ..Config::hd(Codec::H264, CBR)
    };
    let mut session = open(Engine::Encode, config);
    assert!(!session.layout().captured);
    for n in 0..3 {
        let encoded = encode_pattern(&mut session, n, Options::default()).unwrap();
        assert_eq!(encoded.idr, n == 0);
    }
    // The SPS crops nothing: 1440 is whole macroblocks.
    assert_eq!(session.geometry().coded_height, 1440);
}
