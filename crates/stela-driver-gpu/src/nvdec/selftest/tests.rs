use super::*;
use crate::nvdec::mock::{self, Engine, Gpu};
use crate::nvdec::surface::Crop;
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
        let gpu = closed.node;
        assert!(gpu.channel_freed);
        assert_eq!(gpu.unmapped, gpu.mapped);
        assert_eq!(gpu.submissions.len(), 30);
        let name = codec.name();
        assert!(
            lines[0].starts_with(&format!(
                "NVDEC SELFTEST {name} open: 30 pictures, coded 1920x1088 shown 1920x1080"
            )),
            "{}",
            lines[0]
        );
        for n in 0..30 {
            let line = &lines[1 + n];
            assert!(
                line.starts_with(&format!("NVDEC SELFTEST {name} frame {n}: ")),
                "{line}"
            );
            assert!(
                line.contains(" MATCH,")
                    && line.contains(" gpu_us ")
                    && line.contains(" mbs 8160 "),
                "{line}"
            );
        }
        assert!(lines[1].contains(": I slot 0 poc 0,"), "{}", lines[1]);
        assert!(lines[2].contains(": P slot 1 "), "{}", lines[2]);
        assert!(
            lines[31].starts_with(&format!("NVDEC SELFTEST {name} PASS: 30/30 frames match")),
            "{}",
            lines[31]
        );
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
    assert!(lines[1].contains(" MISMATCH,"), "{}", lines[1]);
    assert!(
        lines[2].contains("frame 0 layout probe") && lines[2].contains("no layout matches"),
        "{}",
        lines[2]
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
