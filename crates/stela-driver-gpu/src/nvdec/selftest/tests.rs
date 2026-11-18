use super::*;
use crate::nvdec::mock::{self, Ce, Engine, Gpu};
use crate::nvdec::Picture;
use crate::nvenc::{CHANNEL_ALLOC, CHANNEL_FREE, EXEC};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

/// GA106, the RTX 3050.
const GA106: u16 = 0x176;

/// What the mock decodes for each frame, as the golden checksums.
fn mock_golden(frames: usize) -> Vec<[u8; 16]> {
    let crop = Crop {
        left: 0,
        top: 0,
        width: 1920,
        height: 1080,
    };
    (0..frames as u32)
        .map(|n| {
            let (luma, chroma) = mock::picture(n, 1920, 1088);
            let nv12 = surface::picture(&luma, &chroma, Tiling::Pitch, 1920, &crop);
            md5::md5(&nv12)
        })
        .collect()
}

fn clip(codec: Codec, golden: Vec<[u8; 16]>) -> Clip {
    Clip {
        codec,
        stream: match codec {
            Codec::H264 => H264_CLIP,
            Codec::Hevc => HEVC_CLIP,
        },
        golden,
        pool: match codec {
            Codec::H264 => 16,
            Codec::Hevc => 19,
        },
    }
}

fn run_on(gpu: Gpu, clip: &Clip) -> (Report, Closed<Gpu>, Vec<String>) {
    let mut lines = Vec::new();
    let (report, closed) = run(gpu, GA106, clip, &mut |line| lines.push(format!("{line}")));
    (report, closed, lines)
}

#[test]
fn both_clips_decode_and_match_on_the_mock() {
    for codec in [Codec::H264, Codec::Hevc] {
        let clip = clip(codec, mock_golden(30));
        let (report, closed, lines) = run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)), &clip);
        assert!(report.passed, "{lines:#?}");
        assert_eq!(
            (report.frames, report.matched, report.expected),
            (30, 30, 30)
        );
        assert!(!closed.keeps_file);
        assert_eq!(
            (report.ce_frames, report.bar1_frames, report.ce_hung),
            (30, 0, false)
        );
        let gpu = closed.node;
        assert!(gpu.channel_freed && gpu.ce_freed);
        assert_eq!(gpu.unmapped, gpu.mapped);
        assert_eq!(gpu.submissions.len(), 30);
        assert_eq!(gpu.ce_submissions.len(), 30);
        // The copy engine read every frame; BAR1 none.
        let name = codec.name();
        assert!(
            lines[0].starts_with(&format!(
                "NVDEC SELFTEST {name} open: 30 samples, coded 1920x1088 shown 1920x1080"
            )),
            "{}",
            lines[0]
        );
        assert_eq!(
            lines[1],
            format!(
                "NVDEC SELFTEST {name} readback: copy engine, channel 6 class 0xc7b5, GOB_2 to \
                 pitch-linear NV12 1920x1080 in 3110400 bytes of system memory at 0x130100000, \
                 bound 500 ms"
            )
        );
        for n in 0..30 {
            let line = &lines[2 + n];
            assert!(
                line.starts_with(&format!("NVDEC SELFTEST {name} frame {n}: ")),
                "{line}"
            );
            assert!(
                line.contains(" MATCH,")
                    && line.contains(" gpu_us ")
                    && line.contains(" mbs 8160 ")
                    && line.contains(" (ce_us ")
                    && line.contains(" + cpu_us "),
                "{line}"
            );
        }
        assert!(lines[2].contains(": I slot 0 poc 0,"), "{}", lines[2]);
        assert!(lines[3].contains(": P slot 1 "), "{}", lines[3]);
        assert!(
            lines[32].starts_with(&format!("NVDEC SELFTEST {name} PASS: 30/30 frames match")),
            "{}",
            lines[32]
        );
        assert!(
            lines[32].contains("(copy engine 30 frames: ce_us avg ")
                && lines[32].ends_with("; BAR1 0 frames)"),
            "{}",
            lines[32]
        );
        assert_eq!(lines.len(), 33, "{lines:#?}");
    }
}

#[test]
fn bar1_alone_reads_every_frame_without_a_copy_channel() {
    let clip = clip(Codec::H264, mock_golden(30));
    let mut lines = Vec::new();
    let (report, closed) = run_with(
        Gpu::new(Engine::Decode(Tiling::NVDEC)),
        GA106,
        &clip,
        Readback::Bar1,
        &mut |line| lines.push(format!("{line}")),
    );
    assert!(report.passed, "{lines:#?}");
    assert_eq!((report.ce_frames, report.bar1_frames), (0, 30));
    assert!(lines[1].contains("readback: BAR1"), "{}", lines[1]);
    assert!(lines[2].ends_with(" (bar1)"), "{}", lines[2]);
    let gpu = closed.node;
    assert_eq!(gpu.syncobjs, 1, "no copier");
    assert!(gpu.ce_submissions.is_empty() && !gpu.ce_freed);
}

#[test]
fn without_a_copy_engine_bar1_reads_every_frame() {
    for (ce, why) in [
        (
            Ce::NoChannel(-22),
            "copy channel refused: EINVAL (-22): nouveau found no copy-engine runlist",
        ),
        (
            Ce::NoClass(-19),
            "copy class refused: ENODEV (-19): GSP-RM or nouveau has no NVC7B5",
        ),
    ] {
        let clip = clip(Codec::Hevc, mock_golden(30));
        let (report, closed, lines) =
            run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)).with_ce(ce), &clip);
        assert!(report.passed, "{lines:#?}");
        assert_eq!((report.ce_frames, report.bar1_frames), (0, 30));
        assert!(
            lines[1].contains("readback: the copy engine is unavailable (")
                && lines[1].contains(why)
                && lines[1].ends_with("; reading over BAR1"),
            "{}",
            lines[1]
        );
        assert!(!closed.keeps_file);
        assert_eq!(closed.node.mapped, closed.node.unmapped);
    }
}

#[test]
fn a_hung_copy_falls_back_to_bar1_and_keeps_the_file() {
    let clip = clip(Codec::H264, mock_golden(30));
    let (report, closed, lines) = run_on(
        Gpu::new(Engine::Decode(Tiling::NVDEC)).with_ce(Ce::Hangs),
        &clip,
    );
    // The decode still passes, read over BAR1.
    assert!(report.passed && !report.timed_out, "{lines:#?}");
    assert!(report.ce_hung);
    assert_eq!((report.ce_frames, report.bar1_frames), (0, 30));
    assert!(
        lines[2].contains("frame 0: copy engine TIMEOUT")
            && lines[2].contains("pending")
            && lines[2].ends_with("BAR1 reads this frame and the rest"),
        "{}",
        lines[2]
    );
    assert!(lines[3].contains("frame 0: I slot 0") && lines[3].ends_with(" (bar1)"));
    // One copy only, then BAR1.
    let gpu = &closed.node;
    assert_eq!(gpu.ce_submissions.len(), 1);
    // Nothing is given back under the copy's pending job: neither channel,
    // no buffer, and the file stays open.
    assert!(closed.keeps_file);
    assert!(!gpu.ce_freed && !gpu.channel_freed);
    assert!(gpu.closed.is_empty());
    assert_eq!(gpu.unmapped, gpu.mapped);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the copy engine's channel is still busy")),
        "{lines:#?}"
    );
    assert!(
        !lines
            .iter()
            .any(|l| l.ends_with("so it and its file stay open"))
    );
}

#[test]
fn a_silent_copy_falls_back_to_bar1_and_frees_its_channel() {
    let clip = clip(Codec::Hevc, mock_golden(30));
    let (report, closed, lines) = run_on(
        Gpu::new(Engine::Decode(Tiling::NVDEC)).with_ce(Ce::Silent),
        &clip,
    );
    assert!(report.passed && !report.ce_hung, "{lines:#?}");
    assert!(lines[2].contains("copy engine TIMEOUT") && lines[2].contains("released nothing"));
    assert!(!closed.keeps_file);
    assert!(closed.node.ce_freed && closed.node.channel_freed);
}

#[test]
fn a_wrong_blit_is_caught_by_bar1_which_reads_the_rest() {
    let clip = clip(Codec::H264, mock_golden(30));
    let (report, closed, lines) = run_on(
        Gpu::new(Engine::Decode(Tiling::NVDEC)).with_ce(Ce::Garbles),
        &clip,
    );
    assert!(report.passed, "{lines:#?}");
    assert_eq!((report.ce_frames, report.bar1_frames), (0, 30));
    assert!(
        lines[2].starts_with("NVDEC SELFTEST h264 frame 0 readback check: copy engine md5 ")
            && lines[2].contains("MATCHES: the copy engine's blit is wrong"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].contains(" MATCH,") && lines[3].ends_with(" (bar1)"),
        "{}",
        lines[3]
    );
    assert_eq!(closed.node.ce_submissions.len(), 1);
    assert!(closed.node.ce_freed && !closed.keeps_file);
}

#[test]
fn a_wrong_decode_read_by_the_copy_engine_is_checked_once_over_bar1() {
    // NVIDIA's real checksums against the mock's made-up pictures: the copy
    // engine and BAR1 agree, so the decode differs.
    let clip = clip(Codec::Hevc, golden(Codec::Hevc));
    let (report, _, lines) = run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)), &clip);
    assert!(!report.passed);
    assert_eq!(report.ce_frames, 30, "the copy engine keeps reading");
    let checks: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("readback check"))
        .collect();
    assert_eq!(checks.len(), 1);
    assert!(
        checks[0].contains("agrees with the copy engine: the decode itself differs"),
        "{}",
        checks[0]
    );
}

#[test]
fn every_copy_reads_the_decoded_slot() {
    let clip = clip(Codec::H264, mock_golden(30));
    let (_, closed, _) = run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)), &clip);
    let gpu = closed.node;
    let pictures = Picture::all(Codec::H264, H264_CLIP).unwrap();
    for (submission, picture) in gpu.ce_submissions.iter().zip(&pictures) {
        let lows: Vec<u32> = submission
            .iter()
            .filter(|(m, _)| *m == copy::method::OFFSET_IN_LOWER)
            .map(|(_, d)| *d)
            .collect();
        let luma = 0x2040_0000 + 0x80_0000 * u32::from(picture.slot);
        assert_eq!(lows, [luma, luma + 0x5f_e000]);
    }
}

#[test]
fn a_wrong_picture_is_a_mismatch_and_the_layout_probe_says_so() {
    // NVIDIA's real checksums against the mock's made-up pictures.
    let clip = clip(Codec::H264, golden(Codec::H264));
    let (report, closed, lines) = run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)), &clip);
    assert!(!report.passed);
    assert_eq!((report.frames, report.matched), (30, 0));
    assert!(!closed.keeps_file);
    // The copy engine's read is checked over BAR1, then the layouts.
    assert!(lines[2].contains("frame 0 readback check"), "{}", lines[2]);
    assert!(lines[3].contains(" MISMATCH,"), "{}", lines[3]);
    assert!(
        lines[4].contains("frame 0 layout probe") && lines[4].contains("no layout matches"),
        "{}",
        lines[4]
    );
    // One probe per clip.
    assert_eq!(
        lines.iter().filter(|l| l.contains("layout probe")).count(),
        1
    );
    assert!(
        lines.last().unwrap().contains("FAIL: 0/30 frames match"),
        "{lines:#?}"
    );
}

#[test]
fn a_surface_in_another_block_height_is_found() {
    let clip = clip(Codec::Hevc, mock_golden(30));
    let (report, _, lines) = run_on(Gpu::new(Engine::Decode(Tiling::Blocks(4))), &clip);
    assert!(!report.passed);
    let probe = lines.iter().find(|l| l.contains("layout probe")).unwrap();
    assert!(probe.contains("MATCHES as GOB_4"), "{probe}");
}

#[test]
fn a_hung_decoder_times_out_and_keeps_its_channel() {
    let clip = clip(Codec::H264, mock_golden(30));
    let (report, closed, lines) = run_on(Gpu::new(Engine::HangsAfter(3)), &clip);
    assert!(!report.passed && report.timed_out);
    assert_eq!(report.frames, 3);
    assert!(closed.keeps_file, "{lines:#?}");
    let gpu = closed.node;
    assert!(!gpu.channel_freed);
    assert!(
        gpu.closed.is_empty(),
        "no buffer is freed under a pending job"
    );
    assert_eq!(gpu.unmapped, gpu.mapped);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("frame 3: P slot 3: TIMEOUT") && l.contains("pending")),
        "{lines:#?}"
    );
    assert!(lines.iter().any(|l| l.contains("stay open")));
}

#[test]
fn a_silent_decoder_times_out_and_frees_its_idle_channel() {
    let clip = clip(Codec::Hevc, mock_golden(30));
    let (report, closed, lines) = run_on(Gpu::new(Engine::Silent), &clip);
    assert!(!report.passed && report.timed_out);
    assert!(!closed.keeps_file, "{lines:#?}");
    assert!(closed.node.channel_freed);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("TIMEOUT") && l.contains("wrote nothing"))
    );
}

#[test]
fn a_refused_channel_names_the_linux_patch() {
    let clip = clip(Codec::H264, mock_golden(30));
    let mut gpu = Gpu::new(Engine::Decode(Tiling::NVDEC));
    gpu.refuse.push((CHANNEL_ALLOC, -38));
    let (report, closed, lines) = run_on(gpu, &clip);
    assert!(!report.passed && !report.timed_out);
    assert!(!closed.keeps_file);
    assert!(
        lines[0].contains("FAIL at open: channel: ENOSYS") && lines[0].contains("0208"),
        "{}",
        lines[0]
    );
    assert!(!closed.node.requests.contains(&EXEC));
    assert!(!closed.node.requests.contains(&CHANNEL_FREE));
}

#[test]
fn other_gpus_are_refused_before_any_request() {
    let clip = clip(Codec::H264, mock_golden(30));
    let mut lines = Vec::new();
    let (report, closed) = run(
        Gpu::new(Engine::Decode(Tiling::NVDEC)),
        0x194,
        &clip,
        &mut |line| lines.push(format!("{line}")),
    );
    assert!(!report.passed);
    assert!(closed.node.requests.is_empty());
    assert!(lines[0].contains("no decoder"), "{}", lines[0]);
}

#[test]
fn the_first_submission_binds_the_class_and_the_rest_do_not() {
    let clip = clip(Codec::H264, mock_golden(30));
    let (_, closed, _) = run_on(Gpu::new(Engine::Decode(Tiling::NVDEC)), &clip);
    let gpu = closed.node;
    assert_eq!(gpu.submissions[0][0], (0, 0xc7b0));
    assert_eq!(gpu.submissions[0][1], (0x204, 0x5fff_fffe));
    for submission in &gpu.submissions[1..] {
        assert_eq!(submission[0], (0x200, 3));
    }
    // Payloads count up from 1.
    let payloads: Vec<u32> = gpu
        .submissions
        .iter()
        .map(|s| s.iter().find(|(m, _)| *m == 0x248).unwrap().1)
        .collect();
    assert_eq!(payloads, (1..=30).collect::<Vec<u32>>());
    let _ = vec![0u8; 0];
}
